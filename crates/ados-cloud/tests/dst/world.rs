//! Everything the simulation shares: the queue, the fault plan, the node's
//! life, what the node ran and acked, the trace and the invariant breaks. One
//! thread owns it (`Rc<RefCell<…>>`); no borrow is held across an await.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::rc::Rc;
use std::time::Duration;

use ados_cloud::loops::heartbeat::CloudLinkTracker;
use tokio::time::Instant;

use crate::queue::{Queue, EPOCH_MS};
use crate::rng::Rng;

pub type Shared = Rc<RefCell<World>>;

/// The daemon's HTTPS client timeout: a request with no answer fails after it.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// A window the cloud backend is away.
#[derive(Clone, Copy, Debug)]
pub struct Outage {
    pub start_ms: u64,
    pub end_ms: u64,
    /// Requests vanish until the client times out (else they are refused at
    /// once).
    pub blackhole: bool,
}

/// The fault plan, drawn from the seed before the run.
#[derive(Clone, Debug, Default)]
pub struct Faults {
    /// One-way request latency, ms.
    pub latency_ms: (u64, u64),
    /// A request lost on the way up (never processed).
    pub p_req_lost: f32,
    /// A response lost on the way down (processed, never answered).
    pub p_resp_lost: f32,
    /// A claimed row repeated in the claim response.
    pub p_dup_row: f32,
    /// A claim response's rows shuffled.
    pub p_reorder: f32,
    /// An ack POST delivered to the backend twice.
    pub p_dup_ack: f32,
    pub outages: Vec<Outage>,
    /// Node kills (sim ms), beside the ones its own restart command causes.
    pub kills: Vec<u64>,
    /// How long a killed node takes to come back, ms.
    pub restart_ms: (u64, u64),
    /// How long a plugin install runs, ms.
    pub install_ms: (u64, u64),
    /// A local handler that fails.
    pub p_handler_fails: f32,
}

/// One command an operator queues.
#[derive(Clone, Debug)]
pub struct Enqueue {
    pub at_ms: u64,
    pub command: &'static str,
    pub args: serde_json::Value,
    pub ttl_ms: Option<u64>,
}

/// The verdict the node put in an ack.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Verdict {
    pub status: String,
    pub message: String,
}

/// The run's byte trace: one line per event.
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

pub struct World {
    pub rng: Rng,
    pub t0: Instant,
    pub faults: Faults,
    pub queue: Queue,
    pub trace: Trace,
    pub violations: Vec<String>,
    /// The node's process generation (1 at boot, +1 per restart).
    pub gen: u32,
    pub alive: bool,
    /// A kill the node's own command asked for, by generation.
    pub kill_gen: Option<u32>,
    pub kills: u32,
    /// Times each command id's local effect happened.
    pub effects: BTreeMap<String, u32>,
    /// Ids whose handler started.
    pub started: BTreeSet<String>,
    /// The verdict the node acked per id (the first it sent).
    pub verdicts: BTreeMap<String, Verdict>,
    /// Ids an ack of reached the backend.
    pub landed: BTreeSet<String>,
    /// The node's cloud-link tracker (process memory: a restart resets it).
    pub link: CloudLinkTracker,
    /// When the live process last sent a heartbeat.
    pub hb_sent_ms: Option<u64>,
    /// When the live process came up.
    pub up_ms: u64,
}

impl World {
    pub fn new(seed: u64, faults: Faults) -> Self {
        Self {
            rng: Rng::new(seed),
            t0: Instant::now(),
            faults,
            queue: Queue::default(),
            trace: Trace::default(),
            violations: Vec::new(),
            gen: 0,
            alive: false,
            kill_gen: None,
            kills: 0,
            effects: BTreeMap::new(),
            started: BTreeSet::new(),
            verdicts: BTreeMap::new(),
            landed: BTreeSet::new(),
            link: CloudLinkTracker::default(),
            hb_sent_ms: None,
            up_ms: 0,
        }
    }

    pub fn now_ms(&self) -> u64 {
        self.t0.elapsed().as_millis() as u64
    }

    /// The backend's clock (ms since the Unix epoch).
    pub fn server_ms(&self) -> u64 {
        EPOCH_MS + self.now_ms()
    }

    pub fn log(&mut self, line: impl AsRef<str>) {
        let t = self.now_ms();
        let s = format!("[{:>7}.{:03}] {}\n", t / 1000, t % 1000, line.as_ref());
        self.trace.bytes.extend_from_slice(s.as_bytes());
    }

    /// Record an invariant break (once per message).
    pub fn violate(&mut self, msg: String) {
        if !self.violations.contains(&msg) {
            self.log(format!("VIOLATION {msg}"));
            self.violations.push(msg);
        }
    }

    /// The outage covering `t_ms`, if the backend is away then.
    pub fn outage_at(&self, t_ms: u64) -> Option<Outage> {
        self.faults
            .outages
            .iter()
            .copied()
            .find(|o| (o.start_ms..o.end_ms).contains(&t_ms))
    }

    /// Uniform ms in `[lo, hi)`.
    pub fn draw_ms(&mut self, (lo, hi): (u64, u64)) -> u64 {
        if hi <= lo {
            return lo;
        }
        lo + self.rng.next_u64() % (hi - lo)
    }

    pub fn chance(&mut self, p: f32) -> bool {
        p > 0.0 && self.rng.unit() < p
    }

    /// Acks the node holds that never reached the backend.
    pub fn held(&self) -> Vec<String> {
        self.verdicts
            .keys()
            .filter(|id| !self.landed.contains(*id))
            .cloned()
            .collect()
    }
}
