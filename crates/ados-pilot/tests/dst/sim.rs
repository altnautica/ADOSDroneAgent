//! One simulated flight: the pilot core, the simulated FC ([`Fc`]), an
//! operator surface (heartbeats, sticks, actions) and the router's mission
//! state, stepped together on one thread in 20 ms ticks of simulated time,
//! as `runtime.rs` steps the core (tick, published frame every 100 ms).
//! Every input, output and published frame is written to the run's
//! [`Trace`].

use std::io::Write;

use ados_pilot::config::Settings;
use ados_pilot::core::{Out, Pilot, Reply, TICK_MS};
use ados_protocol::mavlink::ardupilotmega::*;
use ados_protocol::pilot::{ActionReq, ActionResult, Input, PilotState, Request};
use serde_json::{json, Value};

use crate::fc::Fc;
use crate::rng::Rng;

const FC_SYS: u8 = 1;
const FC_COMP: u8 = 1;
/// The published-frame period, as `runtime.rs` publishes it.
const STATE_MS: u64 = 100;
/// The operator surface's heartbeat period.
const OPERATOR_HB_MS: u64 = 1_000;
/// The router's mission-state period.
const MISSION_MS: u64 = 200;
/// The lidar's scan period.
const SCAN_MS: u64 = 100;

/// Everything a run did, in order, as text: byte-identical for one seed.
#[derive(Default)]
pub struct Trace {
    pub bytes: Vec<u8>,
}

impl Trace {
    fn line(&mut self, args: std::fmt::Arguments) {
        let _ = self.bytes.write_fmt(args);
        self.bytes.push(b'\n');
    }

    /// FNV-1a over the trace.
    pub fn hash(&self) -> u64 {
        self.bytes.iter().fold(0xcbf2_9ce4_8422_2325, |h, b| {
            (h ^ u64::from(*b)).wrapping_mul(0x0000_0100_0000_01b3)
        })
    }

    /// The first line two traces differ on, for a failing comparison.
    pub fn first_difference(&self, other: &Trace) -> Option<(usize, String, String)> {
        let a = String::from_utf8_lossy(&self.bytes);
        let b = String::from_utf8_lossy(&other.bytes);
        let mut la = a.lines();
        let mut lb = b.lines();
        let mut i = 0;
        loop {
            match (la.next(), lb.next()) {
                (None, None) => return None,
                (x, y) if x != y => {
                    return Some((i, x.unwrap_or("").into(), y.unwrap_or("").into()));
                }
                _ => i += 1,
            }
        }
    }
}

pub struct Sim {
    /// The scenario's name, for the trace file.
    pub name: &'static str,
    pub seed: u64,
    pub p: Pilot,
    pub fc: Fc,
    pub rng: Rng,
    pub now: u64,
    pub start: u64,
    pub trace: Trace,
    /// The operator surface is connected: heartbeats (and sticks, when set).
    pub link: bool,
    /// The operator's stick axes, streamed every tick while set.
    pub sticks: Option<[f32; 4]>,
    stick_seq: u32,
    hb_ms: Option<u64>,
    /// The last time the operator surface spoke to the pilot.
    pub last_contact_ms: u64,
    /// Every frame the pilot sent the FC, with its time.
    pub sent: Vec<(u64, MavMessage)>,
    answers: Vec<(u64, ActionResult)>,
    /// The latest published frame.
    pub state: PilotState,
    scan: Value,
}

/// With `DST_TRACE_DIR` set, every run writes its trace there as
/// `<scenario>-s<seed>.trace`, a failing run's included, so a seed's run can
/// be read line by line.
impl Drop for Sim {
    fn drop(&mut self) {
        if let Some(dir) = std::env::var_os("DST_TRACE_DIR") {
            let path =
                std::path::Path::new(&dir).join(format!("{}-s{}.trace", self.name, self.seed));
            if let Err(e) = std::fs::write(&path, &self.trace.bytes) {
                eprintln!("dst: could not write {}: {e}", path.display());
            }
        }
    }
}

impl Sim {
    pub fn new(name: &'static str, seed: u64, settings: impl FnOnce(&mut Settings)) -> Self {
        let mut rng = Rng::new(seed);
        let fc = Fc::new(&mut rng);
        let mut s = Settings::default();
        settings(&mut s);
        let p = Pilot::new(s);
        let now = 1_000;
        let state = p.state(now);
        let scan = json!({"frame": "body_frd", "angle_start_rad": 0.0,
            "angle_increment_rad": std::f32::consts::TAU / 360.0, "ranges_m": vec![0.0f32; 360]});
        let mut sim = Self {
            name,
            seed,
            p,
            fc,
            rng,
            now,
            start: now,
            trace: Trace::default(),
            link: true,
            sticks: None,
            stick_seq: 1,
            hb_ms: None,
            last_contact_ms: now,
            sent: Vec::new(),
            answers: Vec::new(),
            state,
            scan,
        };
        sim.trace.line(format_args!("{name} seed {seed}"));
        sim
    }

    /// Simulated seconds since the run began.
    pub fn sim_s(&self) -> f64 {
        (self.now - self.start) as f64 / 1000.0
    }

    /// The launch is still in its take-off step, per the last published frame.
    fn launch_taking_off(&self) -> bool {
        self.state
            .sequence
            .as_ref()
            .is_some_and(|s| s.step == "takeoff")
    }

    /// One 20 ms tick.
    pub fn step(&mut self) {
        self.now += TICK_MS;
        let now = self.now;
        let dt = TICK_MS as f32 / 1000.0;
        let taking_off = self.launch_taking_off();
        self.fc.step(now, dt, &mut self.rng, taking_off);
        for m in self.fc.frames(now, &mut self.rng) {
            self.p.on_fc(FC_SYS, FC_COMP, &m, now);
        }
        if self.link {
            if self
                .hb_ms
                .is_none_or(|t| now.saturating_sub(t) >= OPERATOR_HB_MS)
            {
                self.hb_ms = Some(now);
                self.last_contact_ms = now;
                let r = self.p.on_request(Request::Hb { src: "mc".into() }, now);
                self.reply(r);
            }
            if let Some(axes) = self.sticks {
                self.last_contact_ms = now;
                let input = Input {
                    src: "mc".into(),
                    session: 1,
                    seq: self.stick_seq,
                    t_ms: now,
                    axes,
                    buttons: 0,
                };
                self.stick_seq = self.stick_seq.wrapping_add(1);
                let r = self.p.on_request(Request::Input(input), now);
                self.reply(r);
            }
        }
        if !self.fc.mission.is_empty() && (now - self.start).is_multiple_of(MISSION_MS) {
            let m = ados_pilot::router_state::mission_of(self.router_snapshot());
            self.p
                .on_feed(ados_pilot::router_state::MISSION_TOPIC, &m, now);
        }
        if (now - self.start).is_multiple_of(SCAN_MS) {
            self.p
                .on_feed(ados_pilot::events::TOPIC_SCAN, &self.scan, now);
        }
        self.p.tick(now);
        self.collect();
        if (now - self.start).is_multiple_of(STATE_MS) {
            self.state = self.p.state(now);
            let json = serde_json::to_string(&self.state).expect("state serialises");
            self.trace.line(format_args!("{now} state {json}"));
            self.p.state_published();
            if self.p.obstacles_due(now) {
                if let Some(o) = self.p.obstacles_frame(now) {
                    let json = serde_json::to_string(&o).expect("obstacles serialise");
                    self.trace.line(format_args!("{now} obstacles {json}"));
                }
            }
        }
        let f = &self.fc;
        self.trace.line(format_args!(
            "{now} fc mode={} armed={} landed={} pos={:?} vel={:?} att={:?} batt={:?} seq={} owner={:?}",
            f.mode, f.armed, f.landed, f.pos, f.vel, f.att, f.battery_pct, f.seq, self.p.owner()
        ));
    }

    fn reply(&mut self, r: Reply) {
        if !matches!(r, Reply::None) {
            self.trace.line(format_args!("{} reply {r:?}", self.now));
        }
    }

    fn collect(&mut self) {
        for o in self.p.drain() {
            self.trace.line(format_args!("{} out {o:?}", self.now));
            match o {
                Out::Mav(m) => {
                    self.fc.apply(&m, self.now);
                    self.sent.push((self.now, *m));
                }
                Out::Answer(t, a) => self.answers.push((t, a)),
                Out::Revoked(_) | Out::Sticks(_) | Out::GoalStatus(_) | Out::Persist { .. } => {}
            }
        }
    }

    /// The router's state snapshot of the FC's mission.
    fn router_snapshot(&self) -> Value {
        let f = &self.fc;
        let total = f.mission.len() as u32 + 1;
        let hold_mode = matches!(
            f.mode,
            ados_pilot::modes::copter::LOITER
                | ados_pilot::modes::copter::BRAKE
                | ados_pilot::modes::copter::POSHOLD
        );
        let state = if f.mission_complete {
            "complete"
        } else if f.seq > 0 && (f.mission_paused || hold_mode) {
            "paused"
        } else if f.mode == ados_pilot::modes::copter::AUTO && f.armed && f.seq > 0 {
            "active"
        } else {
            "loaded"
        };
        let ned = |s: u32| {
            f.mission
                .get(s as usize - 1)
                .map(|w| json!([w[0] as f64, w[1] as f64, w[2] as f64]))
        };
        let target = (f.seq > 0).then(|| json!({"seq": f.seq, "cmd": 16, "ned": ned(f.seq)}));
        let items: Vec<Value> = (1..total)
            .map(|s| json!({"seq": s, "cmd": 16, "ned": ned(s)}))
            .collect();
        json!({
            "armed": f.armed,
            "mission": {"state": state, "seq": f.seq, "total": total,
                        "item_reached": f.seq.checked_sub(1), "id": null,
                        "source": "mission_state", "age_ms": 100, "landing_seq": false,
                        "target": target, "rev": 1, "uploads_seen": 0, "items": items},
        })
    }

    /// An operator action from Mission Control; a deferred one runs the
    /// world (at most 3 s) until its answer comes.
    pub fn action(&mut self, name: &str, args: Option<Value>) -> ActionResult {
        self.trace
            .line(format_args!("{} action {name} {args:?}", self.now));
        self.last_contact_ms = self.now;
        let req = Request::Action(ActionReq {
            name: name.into(),
            src: "mc".into(),
            args,
            id: None,
        });
        let r = self.p.on_request(req, self.now);
        self.collect();
        self.reply(r.clone());
        match r {
            Reply::Action(a) => a,
            Reply::Deferred(token) => {
                let end = self.now + 3_000;
                while self.now < end {
                    if let Some(i) = self.answers.iter().position(|(t, _)| *t == token) {
                        return self.answers.remove(i).1;
                    }
                    self.step();
                }
                panic!("{name}: deferred answer {token} never came");
            }
            other => panic!("{name}: {other:?}"),
        }
    }

    /// A request from Mission Control answered at once (a goto).
    pub fn request(&mut self, r: Request) -> Reply {
        self.trace.line(format_args!("{} request {r:?}", self.now));
        self.last_contact_ms = self.now;
        let rep = self.p.on_request(r, self.now);
        self.collect();
        self.reply(rep.clone());
        rep
    }

    /// Run for `ms`.
    pub fn run(&mut self, ms: u64) {
        let end = self.now + ms;
        while self.now < end {
            self.step();
        }
    }

    /// Run until `done` holds (checked every tick), at most `ms`; whether it
    /// came.
    pub fn run_until(&mut self, ms: u64, mut done: impl FnMut(&Sim) -> bool) -> bool {
        let end = self.now + ms;
        while self.now < end {
            if done(self) {
                return true;
            }
            self.step();
        }
        done(self)
    }

    /// When the pilot commanded the FC into `mode`.
    pub fn mode_commands(&self, mode: u32) -> Vec<u64> {
        self.commands(MavCmd::MAV_CMD_DO_SET_MODE)
            .filter(|(_, c)| c.param2 as u32 == mode)
            .map(|(t, _)| t)
            .collect()
    }

    /// The COMMAND_LONGs the pilot sent with `command`.
    pub fn commands(
        &self,
        command: MavCmd,
    ) -> impl Iterator<Item = (u64, &COMMAND_LONG_DATA)> + '_ {
        self.sent.iter().filter_map(move |(t, m)| match m {
            MavMessage::COMMAND_LONG(c) if c.command == command => Some((*t, c)),
            _ => None,
        })
    }

    /// Horizontal distance from home, m.
    pub fn home_distance_m(&self) -> f32 {
        (self.fc.pos[0] * self.fc.pos[0] + self.fc.pos[1] * self.fc.pos[1]).sqrt()
    }

    /// The FC has read the pilot's parameters and nothing blocks a launch:
    /// boot on the ground until then (at most 30 s).
    pub fn boot(&mut self) {
        let ready = self.run_until(30_000, |s| {
            s.now - s.start >= 2_000
                && s.state
                    .readiness
                    .iter()
                    .all(|r| r.ok || r.severity != ados_protocol::pilot::Severity::Block)
        });
        assert!(
            ready,
            "still blocked after boot: {:?}",
            self.state
                .readiness
                .iter()
                .filter(|r| !r.ok)
                .collect::<Vec<_>>()
        );
    }
}
