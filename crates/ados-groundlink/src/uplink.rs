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

/// The box's uplink, shared by the command socket, the receive generation,
/// the presence watcher and every sidecar writer.
#[derive(Debug)]
pub struct Uplink {
    inner: Mutex<Inner>,
    /// What the receive generation should do: `true` run both transmitters.
    transmit: watch::Sender<bool>,
    /// What the receive generation reports: both transmitters running.
    tx_up: watch::Sender<bool>,
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
            transmit: watch::channel(true).0,
            tx_up: watch::channel(false).0,
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

    /// Whether the receive generation should run the transmitters.
    pub fn subscribe_transmit(&self) -> watch::Receiver<bool> {
        self.transmit.subscribe()
    }

    /// The generation reports its transmitters running (`true`) or exited.
    pub fn report_tx_up(&self, up: bool) {
        self.tx_up.send_replace(up);
    }

    pub fn tx_up(&self) -> bool {
        *self.tx_up.borrow()
    }

    /// Hold the op lock for one take / give-back / take-back / fallback.
    pub async fn op(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.ops.lock().await
    }

    /// Ask the generation to stop both transmitters and wait until both have
    /// exited. True once they are down (immediately when no generation runs);
    /// false when they were still up at `deadline`, in which case the ask is
    /// withdrawn so the box does not stay half-stopped.
    pub async fn stop_transmitters(&self, deadline: Duration) -> bool {
        self.transmit.send_replace(false);
        let mut up = self.tx_up.subscribe();
        let stopped = tokio::time::timeout(deadline, up.wait_for(|u| !*u))
            .await
            .is_ok_and(|r| r.is_ok());
        if !stopped {
            self.transmit.send_replace(true);
        }
        stopped
    }

    /// Ask the generation to run both transmitters and wait for them; true
    /// once they run. False when no receive generation brought them up in
    /// `deadline` (no adapter, no key): the next generation starts them.
    pub async fn start_transmitters(&self, deadline: Duration) -> bool {
        self.transmit.send_replace(true);
        let mut up = self.tx_up.subscribe();
        tokio::time::timeout(deadline, up.wait_for(|u| *u))
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
