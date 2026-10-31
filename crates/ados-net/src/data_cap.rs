//! Cellular data-cap tracker.
//!
//! Polls a usage source at 60-second intervals, accumulates bytes, persists to
//! `/var/lib/ados/modem-usage.json`, and emits `data_cap_threshold` events on
//! the [`UplinkEventBus`] when crossing 80, 95, and 100 percent of the cap.
//!
//! The default [`SysfsUsageSource`] reads the
//! kernel counters at `/sys/class/net/<iface>/statistics/{rx,tx}_bytes`
//! directly, but only for the modem's CURRENT interface, resolved the same way
//! the modem manager resolves it: `wwan0` (MBIM/QMI) when present, else `usb0`
//! (RNDIS/AT) ONLY when the USB-gadget tether is not provisioned. `usb0` is
//! also the gadget tether NIC the daemon itself creates, so counting it against
//! the cellular cap would falsely throttle a board that has no modem at all.
//! A poll with no modem iface, or with unreadable counters, counts nothing and
//! keeps the baseline, so the tracker is bench-runnable on a board with no
//! modem. The baseline is tagged with its interface: a switch between `wwan0`
//! and `usb0` re-baselines instead of counting an unrelated counter.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ados_protocol::logd::emitter::IngestEmitter;
use ados_protocol::logd::Level;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;
use tracing::{debug, info, warn};

use crate::router::events::{DataCapState, UplinkEvent, UplinkEventBus, UplinkEventKind};
use crate::sidecar;
use crate::sidecar::json_object_to_fields;

mod clock;
mod source;
mod tracker;
use clock::{current_month, now_ms, now_secs, round2};
pub use tracker::classify;
#[cfg(test)]
use tracker::{cap_to_bytes, load_state};

/// Poll cadence.
pub const DATA_CAP_INTERVAL: Duration = Duration::from_secs(60);
/// Default monthly cap.
pub const DEFAULT_CAP_GB: f64 = 5.0;
/// Persisted counter path.
pub const USAGE_STATE_PATH: &str = "/var/lib/ados/modem-usage.json";

/// rx/tx byte counters from a usage source.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct UsageBytes {
    pub rx_bytes: u64,
    pub tx_bytes: u64,
}

/// One reading of the modem interface's cumulative counters, tagged with the
/// kernel interface they were read from so a baseline is only ever compared
/// against the same netdev.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsageSample {
    pub iface: String,
    pub bytes: UsageBytes,
}

/// A source of cumulative rx/tx byte counters. The default impl reads sysfs;
/// tests inject a scripted source. `None` means nothing can be counted this
/// poll (no modem interface, or its counters could not be read), so the
/// tracker skips the sample and keeps its baseline.
#[async_trait]
pub trait UsageSource: Send + Sync {
    async fn data_usage(&self) -> Option<UsageSample>;
}

/// configfs root + gadget name the daemon provisions the tether under. A
/// present gadget dir means `usb0` belongs to the tether, not the modem.
const USB_GADGET_DIR: &str = "/sys/kernel/config/usb_gadget/ados_gs";

/// Reads `/sys/class/net/<iface>/statistics/{rx,tx}_bytes` for the modem's
/// CURRENT interface only. Returns `None` when that iface is absent or a
/// counter cannot be read, so a bench board with no modem counts nothing.
///
/// The iface is resolved on every read (interfaces appear/disappear with the
/// modem and the gadget) by driver and bus, through the same resolver the
/// modem manager uses. When the gadget owns `usb0`, nothing is counted — so a board with no modem accrues nothing against the cellular
/// cap even while a laptop is tethered over the gadget.
#[derive(Debug, Clone)]
pub struct SysfsUsageSource {
    /// `/sys/class/net`, overridable for tests.
    net_dir: PathBuf,
    /// The provisioned-gadget marker dir, overridable for tests.
    gadget_dir: PathBuf,
}

/// Persisted cumulative-usage window. `last_rx`/`last_tx` are the kernel
/// counters of `last_iface` at the previous sample; they are a baseline for
/// that interface only.
#[derive(Debug, Clone, PartialEq)]
pub struct UsageState {
    pub window_started_at: f64,
    pub cumulative_bytes: u64,
    pub last_rx: u64,
    pub last_tx: u64,
    pub last_iface: Option<String>,
    pub last_reset_month: String,
}

/// Lenient loader (missing fields default).
#[derive(Debug, Default, Deserialize)]
struct RawUsageState {
    window_started_at: Option<f64>,
    cumulative_bytes: Option<u64>,
    last_rx: Option<u64>,
    last_tx: Option<u64>,
    last_iface: Option<String>,
    last_reset_month: Option<String>,
}

/// Tracks cellular data usage across month windows.
pub struct DataCapTracker {
    source: Arc<dyn UsageSource>,
    bus: Arc<UplinkEventBus>,
    cap_bytes: u64,
    state_path: PathBuf,
    state: UsageState,
    last_threshold: Option<DataCapState>,
    /// Best-effort durable-store emitter. When set, each poll's persisted usage
    /// snapshot also ships to the logging store as a `net.modem_usage` event, so
    /// a store-first reader sees the daemon's cumulative figures even when the
    /// FastAPI box cannot see the modem iface. Absent in tests and on a board
    /// with no logd; a saturated channel drops the event without disturbing the
    /// poll loop.
    emitter: Option<IngestEmitter>,
}

#[cfg(test)]
mod tests;
