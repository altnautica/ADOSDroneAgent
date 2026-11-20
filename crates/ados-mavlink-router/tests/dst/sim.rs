//! One simulated run: the router under test ([`Router`]), the simulated FC
//! ([`Fc`]) on a lossy serial link, and, as a scenario asks, a ground
//! station ([`Gcs`]) on the router's proxy and the pilot daemon with its aux
//! radio downlink ([`PilotSide`]). Stepped in 10 ms ticks of simulated time
//! on one thread: the router's 1 Hz heartbeat and request tick, the mission
//! mirror's 200 ms tick and every frame in between run in a fixed order, so
//! a seed replays byte for byte. The invariants ([`crate::checks`]) run
//! every tick; every event goes to the run's [`Trace`].

use std::io::Write;
use std::time::Duration;

use ados_mavlink_router::config::MavlinkConfig;
use ados_protocol::mavlink::ardupilotmega::MavMessage;

use crate::chan::{Faults, Link};
use crate::checks::{Dl, Live};
use crate::downlink::PilotSide;
use crate::fc::{self, Fc};
use crate::gcs::{self, Gcs};
use crate::mav;
use crate::rng::Rng;
use crate::router::{recv, Router, MIRROR_MS, OURS};

/// Simulated time per tick, ms.
pub const STEP: u64 = 10;

/// Everything a run did, in order, as text: byte-identical for one seed.
#[derive(Default)]
pub struct Trace {
    pub bytes: Vec<u8>,
}

impl Trace {
    pub fn line(&mut self, args: std::fmt::Arguments) {
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
        let (mut la, mut lb) = (a.lines(), b.lines());
        for i in 0.. {
            match (la.next(), lb.next()) {
                (None, None) => return None,
                (x, y) if x != y => {
                    return Some((i, x.unwrap_or("").into(), y.unwrap_or("").into()))
                }
                _ => {}
            }
        }
        None
    }
}

/// The ground station, on its own lossy link to the router's proxy.
pub struct GcsSide {
    pub gcs: Gcs,
    pub to_router: Link,
    pub to_gcs: Link,
    /// Commands per tick it sends for the gate scenarios (0 for none).
    pub command_rate: f32,
    pub flight_sent: u64,
    pub benign_sent: u64,
    /// Client writes the router reported as reaching the FC.
    pub accepted: u64,
    /// The ground station's frames the FC decoded: `[benign, flight]`.
    pub heard: [u64; 2],
}

pub struct World {
    pub name: &'static str,
    pub seed: u64,
    pub rng: Rng,
    pub now: u64,
    pub trace: Trace,
    pub cfg: MavlinkConfig,
    pub fc: Fc,
    pub router: Router,
    /// Router -> FC.
    pub up: Link,
    /// FC -> router; frames may arrive split across reads.
    pub down: Link,
    pub gcs: Option<GcsSide>,
    pub pilot: Option<PilotSide>,
    /// Whether stream requests must keep [`crate::checks`]' spacing (off
    /// where a sensor-table change re-requests at once by design).
    pub check_stream_spacing: bool,
    pub live: Live,
    pub dl: Dl,
    /// The FC's missions over time: `(from, until, keys)`.
    pub fc_hist: Vec<(u64, u64, Vec<crate::checks::Key>)>,
}

/// With `DST_TRACE_DIR` set, every run writes its trace there as
/// `<scenario>-s<seed>.trace`, a failing run's included.
impl Drop for World {
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

impl World {
    pub async fn new(name: &'static str, seed: u64, cfg: MavlinkConfig, serial: Faults) -> Self {
        let mut rng = Rng::new(seed);
        let n = rng.between(3, 12) as u16;
        let fc = Fc::new(fc::random_mission(&mut rng, n));
        let now = 1_000;
        let mut router = Router::new(cfg.clone(), now);
        router.open().await;
        let mut w = Self {
            name,
            seed,
            rng,
            now,
            trace: Trace::default(),
            cfg,
            fc,
            router,
            up: Link::new(serial),
            down: Link::new(serial),
            gcs: None,
            pilot: None,
            check_stream_spacing: true,
            live: Live::default(),
            dl: Dl::default(),
            fc_hist: Vec::new(),
        };
        w.live.opened_at = now;
        w.note_mission();
        w.note(format_args!("start seed={seed} mission={}", n + 1));
        w
    }

    pub fn with_gcs(&mut self, faults: Faults, command_rate: f32) {
        self.gcs = Some(GcsSide {
            gcs: Gcs::new(),
            to_router: Link::new(faults),
            to_gcs: Link::new(faults),
            command_rate,
            flight_sent: 0,
            benign_sent: 0,
            accepted: 0,
            heard: [0, 0],
        });
    }

    pub fn note(&mut self, args: std::fmt::Arguments) {
        let now = self.now;
        self.trace.line(format_args!("{now:>7} {args}"));
    }

    /// Fail the run with its reproducer.
    pub fn fail(&self, msg: std::fmt::Arguments) -> ! {
        panic!(
            "dst {} seed {} at t={} ms: {msg}\n  reproduce: DST_SEED={} cargo test -p ados-mavlink-router --test dst dst_{}",
            self.name, self.seed, self.now, self.seed, self.name
        );
    }

    /// Run until `until` ms.
    pub async fn run_to(&mut self, until: u64) {
        while self.now < until {
            self.step().await;
        }
    }

    /// The FC's USB cable pulled: the transport ends (EOF) and the FC,
    /// powered by the cable, goes down with it until re-plugged at `back`.
    pub async fn unplug(&mut self, back: u64) {
        self.router.close().await;
        self.up.flush();
        self.down.flush();
        self.up.down = true;
        self.down.down = true;
        self.fc.power_off(back + 200);
        self.live.closed();
        self.note(format_args!("unplug until {back}"));
    }

    pub async fn replug(&mut self) {
        self.up.down = false;
        self.down.down = false;
        self.router.open().await;
        self.live.opened(self.now);
        self.note(format_args!("replug"));
    }

    /// The router process restarts: a fresh connection and mirror.
    pub async fn restart_router(&mut self) {
        self.router = Router::new(self.cfg.clone(), self.now);
        self.router.open().await;
        self.up.flush();
        self.down.flush();
        if let Some(g) = self.gcs.as_mut() {
            g.to_router.flush();
            g.to_gcs.flush();
        }
        self.live = Live::default();
        self.live.opened_at = self.now;
        self.dl = Dl::default();
        self.note(format_args!("router restart"));
    }

    fn note_mission(&mut self) {
        let keys: Vec<_> = self.fc.mission.iter().map(crate::checks::key).collect();
        if self.fc_hist.last().is_some_and(|(_, _, k)| *k == keys) {
            return;
        }
        if let Some(last) = self.fc_hist.last_mut() {
            last.1 = self.now;
        }
        self.fc_hist.push((self.now, u64::MAX, keys));
    }

    pub async fn step(&mut self) {
        self.now += STEP;
        let now = self.now;
        tokio::time::advance(Duration::from_millis(STEP)).await;
        self.router.set_time(now);

        let was_on = self.fc.powered;
        for f in self.fc.tick(now) {
            let cut = if f.len() > 12 && self.rng.chance(0.1) {
                self.rng.between(1, f.len() as u64 - 1) as usize
            } else {
                f.len()
            };
            self.down.send(now, f[..cut].to_vec(), &mut self.rng);
            if cut < f.len() {
                self.down.send(now, f[cut..].to_vec(), &mut self.rng);
            }
        }
        if self.fc.powered != was_on {
            let boots = self.fc.boots;
            self.note(format_args!("fc boot {boots}"));
        }
        for chunk in self.down.recv(now) {
            self.router.ingest(&chunk).await;
        }
        self.observe_fanout();
        self.router.mirror_frames().await;
        if now.is_multiple_of(MIRROR_MS) {
            self.router.mirror_tick().await;
        }
        if now.is_multiple_of(1_000) {
            self.router.fc.send_heartbeat().await;
            self.router.fc.request_tick().await;
        }
        self.gcs_step().await;
        crate::downlink::step(self).await;
        let writes = self.router.take_writes();
        for wr in &writes {
            self.observe_write(wr);
        }
        for wr in writes {
            self.up.send(now, wr, &mut self.rng);
        }
        for p in self.up.recv(now) {
            self.fc.on_bytes(now, &p);
        }
        self.note_mission();
        self.check().await;
    }

    /// The router's fan-out as a ground station would see it: what it
    /// decoded off the FC link this tick.
    fn observe_fanout(&mut self) {
        while let Some(f) = recv(&mut self.router.obs_rx) {
            let b = &f.bytes;
            if b.len() < 10 || b[0] != 0xFD || (b[5], b[6]) != (fc::SYS, fc::COMP) {
                continue;
            }
            match u32::from_le_bytes([b[7], b[8], b[9], 0]) {
                0 => self.live.hb = Some(self.now),
                id => {
                    self.live.data = Some(self.now);
                    if id == 30 {
                        self.live.attitude(self.now);
                    }
                }
            }
        }
    }

    /// What the router wrote toward the FC: stream requests and its own
    /// mission download requests.
    fn observe_write(&mut self, wr: &[u8]) {
        for (h, msg, _) in mav::decode(wr) {
            if (h.system_id, h.component_id) != OURS {
                continue;
            }
            match msg {
                MavMessage::COMMAND_LONG(c) if c.command as u16 == 511 && c.param2 > 0.0 => {
                    self.live.req = Some(self.now);
                    if self.live.burst_at != Some(self.now) {
                        crate::checks::stream_burst(self);
                    }
                }
                MavMessage::MISSION_REQUEST_LIST(_) | MavMessage::MISSION_REQUEST_INT(_) => {
                    let now = self.now;
                    let start = self.dl.in_flight.map_or(now, |(s, _)| s);
                    self.dl.in_flight = Some((start, now));
                }
                _ => {}
            }
        }
    }

    async fn gcs_step(&mut self) {
        let now = self.now;
        let Some(g) = self.gcs.as_mut() else {
            while recv(&mut self.router.proxy_rx).is_some() {}
            return;
        };
        while let Some(f) = recv(&mut self.router.proxy_rx) {
            g.to_gcs.send(now, f.bytes.to_vec(), &mut self.rng);
        }
        for p in g.to_gcs.recv(now) {
            g.gcs.on_bytes(now, &p);
        }
        let mut out = g.gcs.tick(now);
        if g.command_rate > 0.0 && self.rng.chance(g.command_rate) {
            let mut write = Vec::new();
            for _ in 0..self.rng.between(1, 3) {
                let (kind, f) = gcs::command(&mut g.gcs, &mut self.rng);
                match kind {
                    gcs::Kind::Flight => g.flight_sent += 1,
                    gcs::Kind::Benign => g.benign_sent += 1,
                }
                write.extend(f);
            }
            out.push(write);
        }
        for f in out {
            g.to_router.send(now, f, &mut self.rng);
        }
        let delivered = g.to_router.recv(now);
        let mut foreign = false;
        for p in delivered {
            foreign |= mav::decode(&p).iter().any(|(_, m, _)| {
                matches!(
                    m,
                    MavMessage::MISSION_COUNT(_) | MavMessage::MISSION_ITEM_INT(_)
                )
            });
            let ok = self.router.client_write(&p).await;
            let g = self.gcs.as_mut().expect("present");
            g.accepted += u64::from(ok);
            if ok && self.router.fc.command_down_gated() {
                self.fail(format_args!(
                    "a client write reached the FC through a command-down-gated link"
                ));
            }
        }
        if foreign {
            self.dl.in_flight = None;
        }
    }
}
