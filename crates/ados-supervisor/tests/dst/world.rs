//! The simulated world both rigs live in: their units, files, bind tunnels and
//! process lives, the air between them, the fault plan and the trace. One
//! value behind one lock; every actor reads and writes it on the runtime's
//! paused clock, so a seed names a run exactly.

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ados_supervisor::bind::BindRole;
use serde_norway::Mapping;
use tokio::time::Instant;

use crate::rng::Rng;

/// Index of the drone in [`World::rigs`].
pub const DRONE: usize = 0;
/// Index of the ground station in [`World::rigs`].
pub const GS: usize = 1;

/// The world, shared by every actor.
pub type Shared = Arc<Mutex<World>>;

/// Lock the world. A panic elsewhere already fails the run, so a poisoned lock
/// is taken as is.
pub fn lock(w: &Shared) -> MutexGuard<'_, World> {
    w.lock().unwrap_or_else(|e| e.into_inner())
}

/// One file on a rig.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct File {
    pub data: Vec<u8>,
    pub mtime: SystemTime,
}

/// One line of the upstream bind conversation (`wfb_bind_client.sh` against
/// `wfb_bind_server.sh`), or the stream's end.
#[derive(Clone, Debug)]
pub enum Msg {
    Version,
    VersionOk,
    Bind(Vec<u8>),
    BindOk,
    Fin,
}

/// One rig: a drone or a ground station.
pub struct Rig {
    pub role: BindRole,
    pub name: &'static str,
    /// The supervisor process generation. A host or service-manager handle
    /// made for an older generation belongs to a killed process.
    pub epoch: u32,
    /// Units known to the service manager, and whether each is running.
    pub units: BTreeMap<String, bool>,
    /// When the bind tunnel interface has an address (`None`: no tunnel).
    pub iface_up_at: Option<Instant>,
    /// The tunnel's received-packet counter (a fresh tunnel starts at 0).
    pub rx_packets: u64,
    pub files: BTreeMap<String, File>,
    /// The agent config, written through the production pair-field logic.
    pub config: Mapping,
    pub setup_complete: bool,
    /// The bind-state sentinel as last written.
    pub sentinel: Option<Vec<u8>>,
    /// The drone's socat listener is waiting for a connection.
    pub listening: bool,
    /// Live socat processes.
    pub socats: u32,
    /// A bind call is in flight in the live process.
    pub busy: bool,
    /// The live process finished starting (false from a kill until its boot).
    pub settled: bool,
    /// The state the last bind call that returned reported.
    pub last_state: Option<String>,
    /// The sequence number of the last `systemctl` job issued per unit.
    pub jobs: BTreeMap<String, u64>,
    /// An auto-pair bind (which carries no fleet assignment) paired this rig.
    pub auto_paired: bool,
}

impl Rig {
    pub fn unit_running(&self, unit: &str) -> bool {
        self.units.get(unit).copied().unwrap_or(false)
    }

    /// The sentinel's `active` flag (absent or garbled: false).
    pub fn sentinel_active(&self) -> bool {
        self.sentinel
            .as_deref()
            .and_then(|b| serde_json::from_slice::<serde_json::Value>(b).ok())
            .and_then(|v| v.get("active").and_then(serde_json::Value::as_bool))
            .unwrap_or(false)
    }

    /// The key file this rig's radio runs on.
    pub fn key(&self) -> Option<&[u8]> {
        self.files
            .get(self.role.key_path())
            .map(|f| f.data.as_slice())
    }
}

/// One TCP connection over the bind tunnels; `queues[i]` holds the segments in
/// flight to rig `i`, each with the instant it arrives.
pub struct Conn {
    pub id: u32,
    pub queues: [VecDeque<(Instant, Msg, u64)>; 2],
    /// Arrival of the last segment queued to each rig (TCP delivers in order).
    pub last: [Instant; 2],
    /// Whether each end still has its socket.
    pub open: [bool; 2],
}

/// The fault plan for one run, drawn from the seed.
#[derive(Clone, Debug)]
pub struct Faults {
    /// Chance one transmission of a segment is lost (TCP resends it).
    pub loss: f32,
    /// Chance a delivered segment also arrives as a duplicate frame.
    pub dup: f32,
    /// One-way air latency range, seconds.
    pub latency_s: (f32, f32),
    /// Windows with no air link at all: start and length.
    pub outages: Vec<(Duration, Duration)>,
    /// `wfb-server --version` time on the drone, seconds.
    pub version_s: (f32, f32),
    /// `systemctl` job time, seconds.
    pub systemctl_s: (f32, f32),
    /// Chance starting a bind unit fails.
    pub bind_unit_fail: f32,
    /// Tunnel address time after the bind unit starts, seconds.
    pub iface_s: (f32, f32),
    /// Chance a started bind unit never brings its tunnel up.
    pub iface_never: f32,
}

impl Faults {
    /// A clean link: no loss, no outages, prompt units.
    pub fn clean() -> Self {
        Self {
            loss: 0.0,
            dup: 0.0,
            latency_s: (0.005, 0.02),
            outages: Vec::new(),
            version_s: (0.3, 2.0),
            systemctl_s: (0.05, 0.5),
            bind_unit_fail: 0.0,
            iface_s: (0.5, 3.0),
            iface_never: 0.0,
        }
    }
}

/// The run's byte trace: one line per event, stamped with simulated time.
#[derive(Default)]
pub struct Trace {
    pub bytes: Vec<u8>,
}

impl Trace {
    pub fn lines(&self) -> Vec<&str> {
        std::str::from_utf8(&self.bytes)
            .unwrap_or("")
            .lines()
            .collect()
    }

    /// The first line where two traces part, with both lines.
    pub fn first_difference(&self, other: &Trace) -> Option<(usize, String, String)> {
        let (a, b) = (self.lines(), other.lines());
        (0..a.len().max(b.len())).find_map(|i| {
            let (x, y) = (a.get(i).copied(), b.get(i).copied());
            (x != y).then(|| (i, x.unwrap_or("").to_string(), y.unwrap_or("").to_string()))
        })
    }
}

/// Everything in the simulation.
pub struct World {
    pub rng: Rng,
    pub t0: Instant,
    /// A std clock origin the auto-pair latch's debounce measures from.
    pub std_t0: std::time::Instant,
    pub rigs: [Rig; 2],
    pub conn: Option<Conn>,
    /// Connections accepted so far.
    pub conns: u32,
    /// Keypairs generated so far (the ground station's `wfb_keygen`).
    pub keygens: u32,
    pub faults: Faults,
    pub trace: Trace,
    /// Invariant breaks seen by the per-tick monitor.
    pub violations: Vec<String>,
    /// Each supervisor kill: the rig, and whether its bind had passed the
    /// peer-evidence gate when it died.
    pub kills: Vec<(usize, bool)>,
}

impl World {
    pub fn new(seed: u64, faults: Faults) -> Self {
        Self {
            rng: Rng::new(seed),
            t0: Instant::now(),
            std_t0: std::time::Instant::now(),
            rigs: [rig(BindRole::Drone, "drone"), rig(BindRole::Gs, "gs")],
            conn: None,
            conns: 0,
            keygens: 0,
            faults,
            trace: Trace::default(),
            violations: Vec::new(),
            kills: Vec::new(),
        }
    }

    /// Simulated milliseconds since the run began.
    pub fn now_ms(&self) -> u64 {
        Instant::now().duration_since(self.t0).as_millis() as u64
    }

    /// The simulated instant on a std clock (the latch takes std instants).
    pub fn std_now(&self) -> std::time::Instant {
        self.std_t0 + Instant::now().duration_since(self.t0)
    }

    /// The rigs' wall clocks (one shared epoch: freshness is only ever judged
    /// on one rig against itself).
    pub fn wall_now(&self) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(1_700_000_000) + Instant::now().duration_since(self.t0)
    }

    pub fn log(&mut self, line: impl AsRef<str>) {
        let ms = self.now_ms();
        let text = format!("{:>7}.{:03} {}\n", ms / 1000, ms % 1000, line.as_ref());
        self.trace.bytes.extend_from_slice(text.as_bytes());
    }

    /// A uniform draw in `range` seconds.
    pub fn draw(&mut self, range: (f32, f32)) -> Duration {
        Duration::from_secs_f32(self.rng.range(range.0, range.1).max(0.0))
    }

    /// Whether the air is out right now.
    fn outage(&self) -> bool {
        let now = Instant::now();
        self.faults.outages.iter().any(|(start, len)| {
            let s = self.t0 + *start;
            now >= s && now < s + *len
        })
    }

    /// Whether a frame can cross the bind tunnels right now: both tunnels up
    /// and no outage.
    pub fn link_up(&self) -> bool {
        let now = Instant::now();
        !self.outage()
            && self
                .rigs
                .iter()
                .all(|r| r.iface_up_at.is_some_and(|t| now >= t))
    }

    /// Whether the normal radio link decodes: both normal units running on
    /// the two halves of one keypair, and no outage.
    pub fn radio_link(&self) -> bool {
        let [d, g] = &self.rigs;
        !self.outage()
            && d.unit_running(d.role.normal_unit())
            && g.unit_running(g.role.normal_unit())
            && matches!(
                (d.key().and_then(key_origin), g.key().and_then(key_origin)),
                (Some((a, 0)), Some((b, 1))) if a == b
            )
    }
}

fn rig(role: BindRole, name: &'static str) -> Rig {
    let mut units = BTreeMap::new();
    units.insert(role.normal_unit().to_string(), true);
    units.insert(role.bind_unit().to_string(), false);
    Rig {
        role,
        name,
        epoch: 0,
        units,
        iface_up_at: None,
        rx_packets: 0,
        files: BTreeMap::new(),
        config: Mapping::new(),
        setup_complete: false,
        sentinel: None,
        listening: false,
        socats: 0,
        busy: false,
        settled: true,
        last_state: None,
        jobs: BTreeMap::new(),
        auto_paired: false,
    }
}

/// A 64-byte key file of keypair `generation`: `which` 0 is the drone's half,
/// 1 the ground station's. The public half differs per generation and side.
pub fn key_blob(generation: u32, which: u8) -> Vec<u8> {
    let mut b = vec![0u8; 64];
    b[..4].copy_from_slice(&generation.to_le_bytes());
    b[4] = which;
    for (i, x) in b.iter_mut().enumerate().skip(32) {
        *x = (generation as u8)
            .wrapping_mul(31)
            .wrapping_add(which.wrapping_mul(7))
            .wrapping_add(i as u8);
    }
    b
}

/// The keypair generation and side a key file came from.
pub fn key_origin(blob: &[u8]) -> Option<(u32, u8)> {
    (blob.len() == 64).then(|| {
        (
            u32::from_le_bytes([blob[0], blob[1], blob[2], blob[3]]),
            blob[4],
        )
    })
}
