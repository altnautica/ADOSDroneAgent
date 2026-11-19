//! The ground station's uplink: whether this box transmits for its fleet, or
//! stands by while a phone holds the uplink.
//!
//! Every ground transmitter of a fleet shares one `link_id`, so exactly one
//! ground may transmit at a time: two would thrash every drone's FEC decoder.
//! A phone that takes the uplink over the LAN asks this box to stop first; the
//! box stops both ground transmitters, waits until both processes exited, and
//! only then confirms, so the phone never starts while the box still radiates.
//!
//! In standby the box keeps every receiver (video, telemetry, presence, hop
//! follow) and stops only what transmits. A presence watcher listens on the
//! box's own ground `link_id` for the holder's beacons; after
//! [`HOLDER_STALE`] without one the box transmits again by itself.
//!
//! The state lives in this process only and starts as `transmitting`: a box
//! never boots silent. Every surface reads it from the radio sidecar's
//! `uplink` field, which every sidecar write carries and every transition
//! patches at once.
//!
//! The transmitters themselves belong to the receive generation. The uplink
//! asks it to run them or not ([`TxControl`]); every ask carries a version,
//! and the generation reports the version it carried out. A stop is confirmed
//! only by a report of its own version or a later one with both transmitters
//! exited, so a generation that read "transmit" just before a take and is
//! still spawning cannot answer for the take.

use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tokio::sync::watch;

pub mod ops;
pub mod watcher;

#[cfg(test)]
mod tests;

/// How long the box stands by without a beacon from the holder before it
/// transmits again: the drone's own peer-stale threshold
/// (`ados_radio::hop::PEER_STALE_SECS`), so box and drone give up on a silent
/// phone at the same moment.
pub const HOLDER_STALE: Duration = Duration::from_secs(25);

/// How long a stop or start may take before the op reports failure. Each
/// transmitter gets 2 s after `SIGTERM` before `SIGKILL`.
pub const TX_SWITCH_DEADLINE: Duration = Duration::from_secs(8);

/// The phone that holds the uplink.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Holder {
    /// The phone's ground device id, the one its presence beacons carry.
    pub device_id: String,
    /// The operator-visible name the phone gave.
    pub name: String,
    /// Fingerprint of the approved phone key that took the uplink. Only the
    /// same phone may give it back.
    pub phone_key: String,
    /// Where the phone listens for a take-back (`http://<ip>:<port>/uplink/yield`).
    pub yield_url: Option<String>,
}

/// Transmitting, or standing by for a holder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UplinkState {
    Transmitting,
    Standby(Holder),
}

impl UplinkState {
    pub fn as_str(&self) -> &'static str {
        match self {
            UplinkState::Transmitting => "transmitting",
            UplinkState::Standby(_) => "standby",
        }
    }
}

#[derive(Debug)]
struct Inner {
    state: UplinkState,
    /// Wall-clock ms of the last transition (or of process start).
    since_ms: u64,
    /// Monotonic start of the current standby, the holder timer's floor.
    standby_at: Option<Instant>,
    /// Wall-clock ms of the last beacon decoded from the holder.
    last_beacon_ms: Option<u64>,
    /// Monotonic time of that beacon.
    last_beacon_at: Option<Instant>,
}

/// The receive generation's transmitters, as asked and as carried out.
///
/// A stop holds once the attached generation reported its version (or a
/// later one) with both transmitters exited, or at once when no generation is
/// attached: a generation attaches and reads the ask in force in one step
/// under this channel's lock, so one that attaches later reads the stop and
/// never starts them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TxControl {
    pub ask: TxAsk,
    /// The attached receive generation's report, while one runs.
    pub generation: Option<TxReport>,
}

/// One ask of the receive generation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TxAsk {
    /// Run both transmitters.
    pub transmit: bool,
    /// Bumped on every ask, so a report names the ask it answers.
    pub version: u64,
}

/// What the attached receive generation last carried out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TxReport {
    /// The version of the last ask carried out; `None` before the first.
    pub applied: Option<u64>,
    /// Both transmitters run.
    pub up: bool,
}

impl TxControl {
    fn stopped(&self, version: u64) -> bool {
        self.generation
            .is_none_or(|g| !g.up && g.applied.is_some_and(|a| a >= version))
    }

    fn started(&self, version: u64) -> bool {
        self.generation
            .is_some_and(|g| g.up && g.applied.is_some_and(|a| a >= version))
    }
}

/// A receive generation's view of the asks, from the moment it attached.
#[derive(Debug)]
pub struct TxAsks {
    rx: watch::Receiver<TxControl>,
    first: TxAsk,
    seen: u64,
}

impl TxAsks {
    /// The ask in force when the generation attached.
    pub fn first(&self) -> TxAsk {
        self.first
    }

    /// The next ask after the last one returned; a report alone is not one.
    /// `None` only once the uplink is gone, which the process's never is.
    /// Cancel-safe: nothing is consumed until it returns.
    pub async fn next(&mut self) -> Option<TxAsk> {
        loop {
            self.rx.changed().await.ok()?;
            let ask = self.rx.borrow_and_update().ask;
            if ask.version != self.seen {
                self.seen = ask.version;
                return Some(ask);
            }
        }
    }
}

/// The box's uplink, shared by the command socket, the receive generation,
/// the presence watcher and every sidecar writer.
#[derive(Debug)]
pub struct Uplink {
    inner: Mutex<Inner>,
    /// The transmitters as asked and as carried out.
    tx: watch::Sender<TxControl>,
    /// Serialises the operations, so two requests cannot interleave a stop
    /// and a start.
    ops: tokio::sync::Mutex<()>,
    /// The radio sidecar a transition patches.
    sidecar: std::path::PathBuf,
}

/// The process's uplink.
pub fn uplink() -> &'static Uplink {
    static UPLINK: LazyLock<Uplink> = LazyLock::new(Uplink::new);
    &UPLINK
}

/// Wall-clock unix milliseconds.
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// A device id as a presence beacon carries it: at most 16 bytes.
pub fn beacon_id(device_id: &str) -> &str {
    let mut end = device_id.len().min(16);
    while !device_id.is_char_boundary(end) {
        end -= 1;
    }
    &device_id[..end]
}

impl Default for Uplink {
    fn default() -> Self {
        Self::new()
    }
}

impl Uplink {
    /// Transmitting, as a box always starts, publishing to the radio sidecar.
    pub fn new() -> Self {
        Self::with_sidecar(ados_protocol::paths::run_path("wfb-stats"))
    }

    /// Transmitting, publishing to `sidecar`.
    pub fn with_sidecar(sidecar: std::path::PathBuf) -> Self {
        Self {
            inner: Mutex::new(Inner {
                state: UplinkState::Transmitting,
                since_ms: now_ms(),
                standby_at: None,
                last_beacon_ms: None,
                last_beacon_at: None,
            }),
            tx: watch::channel(TxControl {
                ask: TxAsk {
                    transmit: true,
                    version: 0,
                },
                generation: None,
            })
            .0,
            ops: tokio::sync::Mutex::new(()),
            sidecar,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn state(&self) -> UplinkState {
        self.lock().state.clone()
    }

    /// The holder while standing by.
    pub fn holder(&self) -> Option<Holder> {
        match &self.lock().state {
            UplinkState::Standby(h) => Some(h.clone()),
            UplinkState::Transmitting => None,
        }
    }

    /// The sidecar `uplink` field.
    pub fn sidecar_json(&self) -> Value {
        let inner = self.lock();
        let (holder_id, holder_name) = match &inner.state {
            UplinkState::Standby(h) => (json!(h.device_id), json!(h.name)),
            UplinkState::Transmitting => (Value::Null, Value::Null),
        };
        json!({
            "state": inner.state.as_str(),
            "holder_device_id": holder_id,
            "holder_name": holder_name,
            "since_ms": inner.since_ms,
            "watcher": {"last_beacon_ms": inner.last_beacon_ms},
        })
    }

    /// Whether the box asks for its transmitters to run.
    pub fn wants_transmit(&self) -> bool {
        self.tx.borrow().ask.transmit
    }

    /// Whether the attached receive generation reports both transmitters
    /// running.
    pub fn transmitters_up(&self) -> bool {
        self.tx.borrow().generation.is_some_and(|g| g.up)
    }

    /// A receive generation starts following the asks. Registering it and
    /// reading the ask in force happen in one step, so a stop asked before
    /// is read here and one asked after waits for this generation's report.
    pub fn attach_generation(&self) -> TxAsks {
        let rx = self.tx.subscribe();
        // Overwritten under the channel's lock below.
        let mut first = TxAsk {
            transmit: true,
            version: 0,
        };
        self.tx.send_modify(|c| {
            c.generation = Some(TxReport {
                applied: None,
                up: false,
            });
            first = c.ask;
        });
        TxAsks {
            rx,
            first,
            seen: first.version,
        }
    }

    /// The attached generation carried out ask `version`; `up` is whether
    /// both transmitters run.
    pub fn report_applied(&self, version: u64, up: bool) {
        self.tx.send_if_modified(|c| match &mut c.generation {
            Some(g) => {
                g.applied = Some(version);
                g.up = up;
                true
            }
            None => false,
        });
    }

    /// The generation ended, and nothing it ran transmits any more.
    pub fn detach_generation(&self) {
        self.tx.send_modify(|c| c.generation = None);
    }

    /// Ask the generation to run both transmitters or not; the ask's version.
    fn ask(&self, transmit: bool) -> u64 {
        let mut version = 0;
        self.tx.send_modify(|c| {
            c.ask = TxAsk {
                transmit,
                version: c.ask.version + 1,
            };
            version = c.ask.version;
        });
        version
    }

    /// Hold the op lock for one take / give-back / take-back / fallback.
    pub async fn op(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.ops.lock().await
    }

    /// Ask the generation to stop both transmitters and wait until it
    /// reported this stop with both exited. True once they are down
    /// (immediately when no generation is attached); false when they were
    /// not at `deadline`, in which case the ask is withdrawn so the box does
    /// not stay half-stopped.
    pub async fn stop_transmitters(&self, deadline: Duration) -> bool {
        let mut rx = self.tx.subscribe();
        let version = self.ask(false);
        let stopped = tokio::time::timeout(deadline, rx.wait_for(|c| c.stopped(version)))
            .await
            .is_ok_and(|r| r.is_ok());
        if !stopped {
            self.ask(true);
        }
        stopped
    }

    /// Ask the generation to run both transmitters and wait until it
    /// reported this start with both running. False when no receive
    /// generation brought them up in `deadline` (no adapter, no key): the
    /// next generation reads the ask and starts them.
    pub async fn start_transmitters(&self, deadline: Duration) -> bool {
        let mut rx = self.tx.subscribe();
        let version = self.ask(true);
        tokio::time::timeout(deadline, rx.wait_for(|c| c.started(version)))
            .await
            .is_ok_and(|r| r.is_ok())
    }

    /// Record the standby once the transmitters are down.
    pub fn enter_standby(&self, holder: Holder) {
        {
            let mut inner = self.lock();
            inner.state = UplinkState::Standby(holder);
            inner.since_ms = now_ms();
            inner.standby_at = Some(Instant::now());
            inner.last_beacon_ms = None;
            inner.last_beacon_at = None;
        }
        self.publish();
    }

    /// Record a holder that took again (a retried take): its yield address
    /// may have moved; the standby clock keeps running.
    pub fn refresh_holder(&self, holder: Holder) {
        if let UplinkState::Standby(h) = &mut self.lock().state {
            *h = holder;
        }
        self.publish();
    }

    /// Record the return to transmitting. The caller starts the transmitters.
    pub fn enter_transmitting(&self) {
        {
            let mut inner = self.lock();
            inner.state = UplinkState::Transmitting;
            inner.since_ms = now_ms();
            inner.standby_at = None;
            inner.last_beacon_ms = None;
            inner.last_beacon_at = None;
        }
        self.publish();
    }

    /// A presence beacon from `device_id` decoded on the ground link while
    /// standing by. True when it was the holder's.
    pub fn note_beacon(&self, device_id: &str, now: Instant) -> bool {
        let matched = {
            let mut inner = self.lock();
            let is_holder = matches!(&inner.state,
                UplinkState::Standby(h) if beacon_id(&h.device_id) == device_id);
            if is_holder {
                inner.last_beacon_ms = Some(now_ms());
                inner.last_beacon_at = Some(now);
            }
            is_holder
        };
        if matched {
            self.publish();
        }
        matched
    }

    /// The holder has been silent for [`HOLDER_STALE`]: no beacon since the
    /// later of the standby start and its last beacon.
    pub fn holder_absent(&self, now: Instant) -> bool {
        let inner = self.lock();
        let floor = match (inner.standby_at, inner.last_beacon_at) {
            (Some(a), Some(b)) => a.max(b),
            (Some(a), None) => a,
            _ => return false,
        };
        now.saturating_duration_since(floor) >= HOLDER_STALE
    }

    /// Patch the radio sidecar's `uplink` field at once, so a transition is
    /// visible without waiting for the next stats write. Every other sidecar
    /// write reads [`Uplink::sidecar_json`] itself, so a race between the two
    /// settles on the next write.
    pub fn publish(&self) {
        let path = &self.sidecar;
        let mut doc = std::fs::read_to_string(path)
            .ok()
            .and_then(|t| serde_json::from_str::<Value>(&t).ok())
            .filter(Value::is_object)
            .unwrap_or_else(|| json!({}));
        if let Some(obj) = doc.as_object_mut() {
            obj.insert("uplink".to_string(), self.sidecar_json());
        }
        if let Err(e) = ados_protocol::sidecar::write_json_atomic(path, &doc, 0o644) {
            tracing::debug!(error = %e, "uplink_sidecar_write_failed");
        }
    }
}
