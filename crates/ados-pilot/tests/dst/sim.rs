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
    /// Vertical trunks the lidar sees, `(centre NE, radius)`: with any trunk
    /// or crown set, each scan is cast against them from the vehicle
    /// ([`Sim::stand_scan`]).
    pub stand: Vec<([f32; 2], f32)>,
    /// Tree crowns the lidar returns from.
    pub crowns: Vec<Crown>,
    /// The lidar is fixed to the frame: each scan tilts with the copter's
    /// lean. Off, every scan is cast level (the planar stands' scenarios).
    pub tilted_scan: bool,
    /// The obstacle sensor the run flies with ([`crate::sensors`]).
    pub tier: crate::sensors::SensorTier,
    /// The 3D tiers fly on Navigation's flattened scan instead of the voxel
    /// map (the comparison `DST_BENCH_FLAT=1` selects).
    pub flatten_3d: bool,
    /// The 3D tiers' voxel map, from their first frame.
    pub vox: Option<crate::voxmap::DstVoxmap>,
    /// The snapshot the map is published into, reused.
    pub vox_buf: ados_protocol::voxmap::VoxSnapshot,
    /// Pedestrians every sensor tier sees.
    pub walkers: Vec<crate::sensors::Walker>,
    /// The leaf returns' own stream, so crowns leave every other draw of
    /// the run as it was.
    scan_rng: Rng,
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
            stand: Vec::new(),
            crowns: Vec::new(),
            tilted_scan: false,
            tier: crate::sensors::SensorTier::Planar,
            flatten_3d: false,
            vox: None,
            vox_buf: Default::default(),
            walkers: Vec::new(),
            scan_rng: Rng::new(seed ^ 0x5CA9_1EAF),
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
            let planar = self.tier == crate::sensors::SensorTier::Planar;
            if !planar && !self.flatten_3d {
                self.feed_voxmap();
            } else {
                if !self.stand.is_empty() || !self.crowns.is_empty() || !self.walkers.is_empty() {
                    self.scan = match self.tier {
                        crate::sensors::SensorTier::Planar => self.stand_scan(),
                        t => self.flat_scan(t),
                    };
                }
                self.p
                    .on_feed(ados_pilot::events::TOPIC_SCAN, &self.scan, now);
            }
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

    /// A frame from the FC outside its own streams (an ack to a ground
    /// station, a STATUSTEXT), taken by the pilot now.
    pub fn fc_frame(&mut self, m: MavMessage) {
        self.trace.line(format_args!("{} fc_frame {m:?}", self.now));
        self.p.on_fc(FC_SYS, FC_COMP, &m, self.now);
        self.collect();
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

/// The planar lidar's reach, m (the 12 m class the pilot is built for).
const LIDAR_RANGE_M: f32 = 12.0;
/// Leaf extinction in a crown, 1/m: a conifer crown's leaf area density of
/// 1.0 m²/m³ with a spherical leaf-angle G of 0.5.
const LEAF_EXTINCTION_PER_M: f32 = 0.5;

/// A tree crown as the World Engine casts it: an upright cylinder of
/// leaves over the ground (`d = 0`), returning a beam by Beer–Lambert.
#[derive(Debug, Clone, Copy)]
pub struct Crown {
    pub c: [f32; 2],
    pub r: f32,
    /// Height of its base and top over the ground, m.
    pub base_m: f32,
    pub top_m: f32,
}

/// Where a beam from `p` along the horizontal unit `u` meets the vertical
/// disk `(c, r)`: the horizontal distances it enters and leaves at.
fn disk_span(p: [f32; 2], u: [f32; 2], c: [f32; 2], r: f32) -> Option<(f32, f32)> {
    let rel = [c[0] - p[0], c[1] - p[1]];
    let along = rel[0] * u[0] + rel[1] * u[1];
    let perp2 = rel[0] * rel[0] + rel[1] * rel[1] - along * along;
    if perp2 >= r * r {
        return None;
    }
    let half = (r * r - perp2).sqrt();
    (along + half > 0.0).then(|| ((along - half).max(0.0), along + half))
}

impl Sim {
    /// One planar scan from the vehicle: 360 bins clockwise from the nose,
    /// each beam tilted with the copter's attitude while
    /// [`Sim::tilted_scan`] (the lidar fixed to the frame) and cast against
    /// the trunks (vertical cylinders from the
    /// ground), the crowns (leaf returns at an exponential depth, from the
    /// scan's own seeded stream) and the ground; the slant range of the
    /// first return, 0 for none within [`LIDAR_RANGE_M`].
    pub fn stand_scan(&mut self) -> Value {
        let t = self.fc.truth();
        let p = [t[0], t[1]];
        let h0 = -t[2];
        let [mut roll, mut pitch, yaw] = self.fc.att;
        if !self.tilted_scan {
            (roll, pitch) = (0.0, 0.0);
        }
        let (sr, cr) = roll.sin_cos();
        let (sp, cp) = pitch.sin_cos();
        let mut ranges = Vec::with_capacity(360);
        for k in 0..360 {
            let th = (k as f32).to_radians();
            // Body FRD beam, rotated by roll then pitch (as the pilot levels
            // it), then by the heading.
            let (bx, by) = (th.cos(), th.sin());
            let (y1, z1) = (by * cr, by * sr);
            let x2 = bx * cp + z1 * sp;
            let down = -bx * sp + z1 * cp;
            let hz = x2.hypot(y1).max(1e-6);
            // A level scan keeps its bins' own bearings (no atan2 rounding).
            let b = yaw
                + if self.tilted_scan {
                    ados_pilot::geo::atan2(y1, x2)
                } else {
                    th
                };
            let u = [b.cos(), b.sin()];
            // Height over the ground after `s` metres of horizontal travel.
            let rise = -down / hz;
            let height = |s: f32| h0 + rise * s;
            let mut best = LIDAR_RANGE_M * hz;
            if rise < 0.0 {
                best = best.min(h0 / -rise);
            }
            for &(c, r) in &self.stand {
                if let Some((s, _)) = disk_span(p, u, c, r) {
                    if height(s) >= 0.0 {
                        best = best.min(s);
                    }
                }
            }
            for w in &self.walkers {
                if let Some((s, _)) = disk_span(p, u, w.at(self.now), crate::sensors::WALKER_R_M) {
                    if (0.0..=crate::sensors::WALKER_H_M).contains(&height(s)) {
                        best = best.min(s);
                    }
                }
            }
            let mut leaves: Vec<(f32, f32)> = self
                .crowns
                .iter()
                .filter_map(|cw| {
                    let (a, z) = disk_span(p, u, cw.c, cw.r)?;
                    // The horizontal span inside the crown's height band.
                    let (lo, hi) = if rise.abs() < 1e-6 {
                        if (cw.base_m..=cw.top_m).contains(&h0) {
                            (0.0, f32::INFINITY)
                        } else {
                            return None;
                        }
                    } else {
                        let s1 = (cw.base_m - h0) / rise;
                        let s2 = (cw.top_m - h0) / rise;
                        (s1.min(s2), s1.max(s2))
                    };
                    let (a, z) = (a.max(lo), z.min(hi));
                    (a < z && a < best).then_some((a, z))
                })
                .collect();
            leaves.sort_by(|x, y| x.0.total_cmp(&y.0));
            for (a, z) in leaves {
                if a >= best {
                    break;
                }
                // Exponential depth along the slant beam.
                let depth = -(1.0 - self.scan_rng.unit()).ln() / LEAF_EXTINCTION_PER_M * hz;
                if a + depth < z {
                    best = best.min(a + depth);
                    break;
                }
            }
            let slant = best / hz;
            ranges.push(if slant < LIDAR_RANGE_M {
                slant.max(0.0)
            } else {
                0.0
            });
        }
        json!({"frame": "body_frd", "angle_start_rad": 0.0,
            "angle_increment_rad": std::f32::consts::TAU / 360.0, "ranges_m": ranges})
    }

    /// The nearest trunk surface to the vehicle, m (infinite with no stand).
    pub fn stand_clearance_m(&self) -> f32 {
        self.stand
            .iter()
            .map(|&(c, r)| {
                let t = self.fc.truth();
                (c[0] - t[0]).hypot(c[1] - t[1]) - r
            })
            .fold(f32::INFINITY, f32::min)
    }
}
