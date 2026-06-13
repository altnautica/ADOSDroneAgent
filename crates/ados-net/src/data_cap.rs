//! Cellular data-cap tracker.
//!
//! Polls a usage source at 60-second intervals, accumulates bytes, persists to
//! `/var/lib/ados/modem-usage.json`, and emits `data_cap_threshold` events on
//! the [`UplinkEventBus`] when crossing 80, 95, and 100 percent of the cap.
//! Ports `uplink/data_cap.py`.
//!
//! In the all-Python agent the byte counters came from the modem manager's
//! `data_usage()` coroutine. Here the default [`SysfsUsageSource`] reads the
//! kernel counters at `/sys/class/net/<iface>/statistics/{rx,tx}_bytes`
//! directly, but only for the modem's CURRENT interface, resolved the same way
//! the modem manager resolves it: `wwan0` (MBIM/QMI) when present, else `usb0`
//! (RNDIS/AT) ONLY when the USB-gadget tether is not provisioned. `usb0` is
//! also the gadget tether NIC the daemon itself creates, so counting it against
//! the cellular cap would falsely throttle a board that has no modem at all.
//! The reads return 0 when the resolved iface is absent, so the tracker is
//! bench-runnable on a board with no modem.

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

/// A source of cumulative rx/tx byte counters. The default impl reads sysfs;
/// tests inject a scripted source.
#[async_trait]
pub trait UsageSource: Send + Sync {
    async fn data_usage(&self) -> UsageBytes;
}

/// Cellular MBIM/QMI iface.
const WWAN_IFACE: &str = "wwan0";
/// Cellular RNDIS/AT iface, ALSO the USB-gadget tether NIC the daemon creates.
const USB_IFACE: &str = "usb0";
/// configfs root + gadget name the daemon provisions the tether under. A
/// present gadget dir means `usb0` belongs to the tether, not the modem.
const USB_GADGET_DIR: &str = "/sys/kernel/config/usb_gadget/ados_gs";

/// Reads `/sys/class/net/<iface>/statistics/{rx,tx}_bytes` for the modem's
/// CURRENT interface only. Returns 0 when that iface is absent, so a bench
/// board with no modem reports zero usage rather than failing.
///
/// The iface is resolved on every read (interfaces appear/disappear with the
/// modem and the gadget): `wwan0` when present, else `usb0` ONLY when the
/// USB-gadget tether is not provisioned. When the gadget owns `usb0`, nothing
/// is counted — so a board with no modem accrues nothing against the cellular
/// cap even while a laptop is tethered over the gadget.
#[derive(Debug, Clone)]
pub struct SysfsUsageSource {
    /// `/sys/class/net`, overridable for tests.
    net_dir: PathBuf,
    /// The provisioned-gadget marker dir, overridable for tests.
    gadget_dir: PathBuf,
}

impl SysfsUsageSource {
    /// Default sysfs net dir + the canonical provisioned-gadget marker.
    pub fn new() -> Self {
        Self {
            net_dir: PathBuf::from("/sys/class/net"),
            gadget_dir: PathBuf::from(USB_GADGET_DIR),
        }
    }

    /// Explicit roots (tests point these at a tempdir).
    pub fn with_roots(net_dir: PathBuf, gadget_dir: PathBuf) -> Self {
        Self {
            net_dir,
            gadget_dir,
        }
    }

    /// Resolve the modem's current interface, mirroring
    /// `ModemManager::current_iface` but with the tether carve-out: `wwan0`
    /// preferred, else `usb0` only when the USB gadget is NOT provisioned.
    /// `None` means "count nothing" (no modem iface, or `usb0` is the tether).
    fn modem_iface(&self) -> Option<String> {
        if self.net_dir.join(WWAN_IFACE).exists() {
            return Some(WWAN_IFACE.to_string());
        }
        if self.net_dir.join(USB_IFACE).exists() && !self.gadget_dir.exists() {
            return Some(USB_IFACE.to_string());
        }
        None
    }

    fn read_counter(&self, iface: &str, counter: &str) -> u64 {
        let path = self.net_dir.join(iface).join("statistics").join(counter);
        std::fs::read_to_string(path)
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok())
            .unwrap_or(0)
    }
}

impl Default for SysfsUsageSource {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl UsageSource for SysfsUsageSource {
    async fn data_usage(&self) -> UsageBytes {
        match self.modem_iface() {
            Some(iface) => UsageBytes {
                rx_bytes: self.read_counter(&iface, "rx_bytes"),
                tx_bytes: self.read_counter(&iface, "tx_bytes"),
            },
            None => UsageBytes::default(),
        }
    }
}

/// Persisted cumulative-usage window. Mirrors `_UsageState`.
#[derive(Debug, Clone, PartialEq)]
pub struct UsageState {
    pub window_started_at: f64,
    pub cumulative_bytes: u64,
    pub last_rx: u64,
    pub last_tx: u64,
    pub last_reset_month: String,
}

impl UsageState {
    fn fresh(month: String) -> Self {
        Self {
            window_started_at: now_secs(),
            cumulative_bytes: 0,
            last_rx: 0,
            last_tx: 0,
            last_reset_month: month,
        }
    }

    /// Render byte-identically to Python `json.dumps(state.to_json())`: fixed
    /// key order, `", "` / `": "` separators with spaces, floats rendered the
    /// same way serde and Python both render them, no trailing newline.
    pub fn render_json(&self) -> String {
        // serde_json renders f64 identically to Python json.dumps for these
        // values; the integers are exact. Only the separator spacing differs
        // from a compact `to_string`, so the body is assembled by hand.
        let w = serde_json::to_string(&self.window_started_at).unwrap_or_else(|_| "0.0".into());
        let m = serde_json::to_string(&self.last_reset_month).unwrap_or_else(|_| "\"\"".into());
        format!(
            "{{\"window_started_at\": {w}, \"cumulative_bytes\": {}, \"last_rx\": {}, \"last_tx\": {}, \"last_reset_month\": {m}}}",
            self.cumulative_bytes, self.last_rx, self.last_tx
        )
    }
}

/// Lenient loader mirror of `_UsageState.from_json` (missing fields default).
#[derive(Debug, Default, Deserialize)]
struct RawUsageState {
    window_started_at: Option<f64>,
    cumulative_bytes: Option<u64>,
    last_rx: Option<u64>,
    last_tx: Option<u64>,
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

impl DataCapTracker {
    /// Build a tracker with the default cap and state path.
    pub fn new(source: Arc<dyn UsageSource>, bus: Arc<UplinkEventBus>) -> Self {
        Self::with_config(source, bus, DEFAULT_CAP_GB, PathBuf::from(USAGE_STATE_PATH))
    }

    /// Full constructor (tests).
    pub fn with_config(
        source: Arc<dyn UsageSource>,
        bus: Arc<UplinkEventBus>,
        cap_gb: f64,
        state_path: PathBuf,
    ) -> Self {
        let cap_bytes = cap_to_bytes(cap_gb);
        let state = load_state(&state_path);
        Self {
            source,
            bus,
            cap_bytes,
            state_path,
            state,
            last_threshold: None,
            emitter: None,
        }
    }

    /// Attach a durable-store emitter. Builder-style so the daemon can opt in to
    /// shipping each poll's usage block as a `net.modem_usage` event while tests
    /// keep the default (no emitter). The emitter is best-effort and never gates
    /// the poll's persist.
    pub fn with_emitter(mut self, emitter: IngestEmitter) -> Self {
        self.emitter = Some(emitter);
        self
    }

    /// Reset the monthly cap.
    pub fn set_cap(&mut self, gb: f64) {
        self.cap_bytes = cap_to_bytes(gb);
        info!(cap_gb = gb, "uplink.datacap_set");
    }

    /// Classify the current usage. Mirrors `_classify`: cap<=0 → ok; >=100
    /// blocked_100; >=95 throttle_95; >=80 warn_80; else ok.
    pub fn classify(&self) -> DataCapState {
        classify(self.cap_bytes, self.state.cumulative_bytes)
    }

    /// Month-window reset check. Returns `true` when a reset happened. Mirrors
    /// `_check_month_reset` (compares against the local-time `%Y-%m`).
    pub fn check_month_reset(&mut self) -> bool {
        let now_month = current_month();
        if self.state.last_reset_month != now_month {
            info!(
                from_month = %self.state.last_reset_month,
                to_month = %now_month,
                bytes_used = self.state.cumulative_bytes,
                "uplink.datacap_month_reset"
            );
            self.state = UsageState::fresh(now_month);
            self.last_threshold = None;
            self.save_state();
            true
        } else {
            false
        }
    }

    /// fsync-before-rename persist. Mirrors `_save_state`: the fsync defends
    /// against power loss between write and the kernel flushing dirty pages,
    /// which would otherwise roll the cap counter back.
    fn save_state(&self) {
        let body = self.state.render_json();
        if let Err(exc) = sidecar::write_atomic_fsync(&self.state_path, body.as_bytes()) {
            warn!(error = %exc, "uplink.datacap_save_failed");
        }
    }

    /// One poll. Reads the source, applies counter-reset handling, accumulates,
    /// persists, and emits a `data_cap_threshold` event ONLY on a state
    /// transition. Mirrors `_poll_once`.
    pub async fn poll_once(&mut self) {
        let usage = self.source.data_usage().await;
        let rx = usage.rx_bytes;
        let tx = usage.tx_bytes;

        // Counter-reset handling: a new value smaller than the last sample
        // means the modem (or kernel iface) re-counted from zero, so the delta
        // for that sample is dropped rather than counted as a huge spike.
        let (drx, dtx) = if rx < self.state.last_rx || tx < self.state.last_tx {
            (0, 0)
        } else {
            (rx - self.state.last_rx, tx - self.state.last_tx)
        };

        self.state.last_rx = rx;
        self.state.last_tx = tx;
        self.state.cumulative_bytes += drx + dtx;
        self.save_state();

        // Ship the just-persisted usage snapshot to the store. The body is the
        // exact `get_usage()` block the modem view serves, so a store-first
        // reader gets byte-identical cumulative figures. Best-effort.
        self.emit_usage();

        let new_state = self.classify();
        if Some(new_state) != self.last_threshold {
            self.last_threshold = Some(new_state);
            info!(
                state = ?new_state,
                used_mb = self.state.cumulative_bytes / (1024 * 1024),
                cap_mb = self.cap_bytes / (1024 * 1024),
                "uplink.datacap_threshold"
            );
            self.bus.publish(UplinkEvent {
                kind: UplinkEventKind::DataCapThreshold,
                active_uplink: None,
                available: Vec::new(),
                internet_reachable: true,
                data_cap_state: Some(new_state),
                timestamp_ms: now_ms(),
            });
        }
    }

    /// Usage snapshot. Key names + rounding match the Python `get_usage`.
    pub fn get_usage(&self) -> Value {
        let used_mb = self.state.cumulative_bytes / (1024 * 1024);
        let cap_mb = self.cap_bytes / (1024 * 1024);
        let pct = if self.cap_bytes > 0 {
            round2((self.state.cumulative_bytes as f64 / self.cap_bytes as f64) * 100.0)
        } else {
            0.0
        };
        serde_json::json!({
            "data_used_mb": used_mb,
            "cap_mb": cap_mb,
            "percent": pct,
            "state": self.classify(),
            "window_reset_at": self.state.window_started_at,
            "last_reset_month": self.state.last_reset_month,
        })
    }

    /// Emit the current usage snapshot to the store as a `net.modem_usage`
    /// event. Best-effort: an absent or saturated emitter drops it silently and
    /// never disturbs the poll. The body is `get_usage()` so the stored row is a
    /// faithful copy of what the modem view serves.
    fn emit_usage(&self) {
        let Some(emitter) = self.emitter.as_ref() else {
            return;
        };
        let body = self.get_usage();
        emitter.emit_event("net.modem_usage", Level::Info, json_object_to_fields(&body));
    }

    /// Flush the latest counter to disk (clean-shutdown hook).
    pub fn flush(&self) {
        self.save_state();
    }
}

fn cap_to_bytes(gb: f64) -> u64 {
    (gb * 1024.0 * 1024.0 * 1024.0) as u64
}

/// Shared classifier so the tracker and any external caller agree.
pub fn classify(cap_bytes: u64, cumulative_bytes: u64) -> DataCapState {
    if cap_bytes == 0 {
        return DataCapState::Ok;
    }
    let pct = (cumulative_bytes as f64 / cap_bytes as f64) * 100.0;
    if pct >= 100.0 {
        DataCapState::Blocked100
    } else if pct >= 95.0 {
        DataCapState::Throttle95
    } else if pct >= 80.0 {
        DataCapState::Warn80
    } else {
        DataCapState::Ok
    }
}

fn load_state(path: &Path) -> UsageState {
    match std::fs::read(path) {
        Ok(bytes) => match serde_json::from_slice::<RawUsageState>(&bytes) {
            Ok(raw) => UsageState {
                window_started_at: raw.window_started_at.unwrap_or_else(now_secs),
                cumulative_bytes: raw.cumulative_bytes.unwrap_or(0),
                last_rx: raw.last_rx.unwrap_or(0),
                last_tx: raw.last_tx.unwrap_or(0),
                last_reset_month: raw.last_reset_month.unwrap_or_default(),
            },
            Err(exc) => {
                warn!(error = %exc, "uplink.datacap_load_failed");
                UsageState::fresh(current_month())
            }
        },
        Err(exc) if exc.kind() == std::io::ErrorKind::NotFound => {
            UsageState::fresh(current_month())
        }
        Err(exc) => {
            debug!(error = %exc, "uplink.datacap_load_failed");
            UsageState::fresh(current_month())
        }
    }
}

/// Local-time `%Y-%m`, matching Python `time.strftime("%Y-%m")`.
fn current_month() -> String {
    // Derive year-month from local time without a chrono dependency. We read
    // the local broken-down time via libc on unix; off-unix we fall back to a
    // UTC computation. Both render `YYYY-MM`.
    local_year_month().unwrap_or_else(utc_year_month)
}

#[cfg(unix)]
fn local_year_month() -> Option<String> {
    // SAFETY: localtime_r writes into a caller-owned tm; time() needs no args.
    unsafe {
        let t = libc_time();
        let mut tm: LibcTm = std::mem::zeroed();
        if localtime_r(&t, &mut tm).is_null() {
            return None;
        }
        Some(format!("{:04}-{:02}", tm.tm_year + 1900, tm.tm_mon + 1))
    }
}

#[cfg(not(unix))]
fn local_year_month() -> Option<String> {
    None
}

// Minimal libc FFI for localtime_r so the crate does not pull a date library
// for a single `%Y-%m` format. tm layout matches the C `struct tm` prefix we
// read (year + month); the remaining fields are present so the struct size is
// correct for the FFI call.
#[cfg(unix)]
#[repr(C)]
struct LibcTm {
    tm_sec: i32,
    tm_min: i32,
    tm_hour: i32,
    tm_mday: i32,
    tm_mon: i32,
    tm_year: i32,
    tm_wday: i32,
    tm_yday: i32,
    tm_isdst: i32,
    tm_gmtoff: i64,
    tm_zone: *const i8,
}

#[cfg(unix)]
extern "C" {
    #[link_name = "time"]
    fn c_time(tloc: *mut i64) -> i64;
    fn localtime_r(timep: *const i64, result: *mut LibcTm) -> *mut LibcTm;
}

#[cfg(unix)]
unsafe fn libc_time() -> i64 {
    c_time(std::ptr::null_mut())
}

/// UTC fallback year-month from the unix epoch (no leap-second handling; only
/// used off-unix or if localtime_r fails).
fn utc_year_month() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0) as i64;
    let days = secs.div_euclid(86_400);
    // Civil-from-days (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if m <= 2 { y + 1 } else { y };
    format!("{year:04}-{m:02}")
}

fn now_secs() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Round to 2 decimals the same way Python `round(x, 2)` does for the percent
/// field (banker's rounding is not observable at the precision we emit).
fn round2(x: f64) -> f64 {
    (x * 100.0).round() / 100.0
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FixedSource(UsageBytes);
    #[async_trait]
    impl UsageSource for FixedSource {
        async fn data_usage(&self) -> UsageBytes {
            self.0
        }
    }

    fn tracker(
        cap_gb: f64,
        state_path: PathBuf,
        rx: u64,
        tx: u64,
    ) -> (DataCapTracker, Arc<UplinkEventBus>) {
        let bus = Arc::new(UplinkEventBus::new());
        let src = Arc::new(FixedSource(UsageBytes {
            rx_bytes: rx,
            tx_bytes: tx,
        }));
        let t = DataCapTracker::with_config(src, Arc::clone(&bus), cap_gb, state_path);
        (t, bus)
    }

    #[test]
    fn classify_thresholds_match_python() {
        let cap = cap_to_bytes(1.0); // 1 GiB
                                     // Pick values comfortably inside each band (rounding `as u64` down can
                                     // pull an exact-boundary value back under the threshold, so nudge up).
        let at = |frac: f64| ((cap as f64 * frac) as u64) + 1;
        assert_eq!(classify(cap, 0), DataCapState::Ok);
        assert_eq!(classify(cap, at(0.79)), DataCapState::Ok);
        assert_eq!(classify(cap, at(0.80)), DataCapState::Warn80);
        assert_eq!(classify(cap, at(0.85)), DataCapState::Warn80);
        assert_eq!(classify(cap, at(0.95)), DataCapState::Throttle95);
        assert_eq!(classify(cap, at(0.97)), DataCapState::Throttle95);
        assert_eq!(classify(cap, cap), DataCapState::Blocked100);
        assert_eq!(classify(cap, cap + 1), DataCapState::Blocked100);
        // cap <= 0 → ok regardless.
        assert_eq!(classify(0, 999_999), DataCapState::Ok);
    }

    #[test]
    fn render_json_is_byte_exact_to_python_json_dumps() {
        let st = UsageState {
            window_started_at: 1_700_000_000.0,
            cumulative_bytes: 123_456,
            last_rx: 1000,
            last_tx: 2000,
            last_reset_month: "2026-05".to_string(),
        };
        assert_eq!(
            st.render_json(),
            r#"{"window_started_at": 1700000000.0, "cumulative_bytes": 123456, "last_rx": 1000, "last_tx": 2000, "last_reset_month": "2026-05"}"#
        );
    }

    #[test]
    fn save_then_load_round_trips_and_fsync_leaves_no_tmp() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("modem-usage.json");
        let (mut t, _bus) = tracker(5.0, path.clone(), 0, 0);
        t.state.cumulative_bytes = 42;
        t.state.last_reset_month = "2026-05".to_string();
        t.save_state();
        // No torn tmp (with_suffix(".json.tmp")).
        assert!(!dir.path().join("modem-usage.json.tmp").exists());
        let loaded = load_state(&path);
        assert_eq!(loaded.cumulative_bytes, 42);
        assert_eq!(loaded.last_reset_month, "2026-05");
    }

    #[tokio::test]
    async fn poll_accumulates_and_handles_counter_reset() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("modem-usage.json");
        // Start at rx=1000 tx=500.
        let bus = Arc::new(UplinkEventBus::new());
        let src = Arc::new(FixedSource(UsageBytes {
            rx_bytes: 1000,
            tx_bytes: 500,
        }));
        let mut t = DataCapTracker::with_config(src, Arc::clone(&bus), 5.0, path);
        // First poll baselines: delta from last_rx/tx=0 → +1500.
        t.poll_once().await;
        assert_eq!(t.state.cumulative_bytes, 1500);
        assert_eq!(t.state.last_rx, 1000);
        // Simulate a counter reset: new sample smaller than last → delta 0.
        let bus2 = Arc::new(UplinkEventBus::new());
        let src2 = Arc::new(FixedSource(UsageBytes {
            rx_bytes: 10,
            tx_bytes: 5,
        }));
        t.source = src2 as Arc<dyn UsageSource>;
        let _ = bus2;
        t.poll_once().await;
        // cumulative unchanged (reset dropped the delta), baseline re-set.
        assert_eq!(t.state.cumulative_bytes, 1500);
        assert_eq!(t.state.last_rx, 10);
    }

    #[tokio::test]
    async fn poll_emits_threshold_event_only_on_transition() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("modem-usage.json");
        // cap 1 KiB so a small counter trips blocked_100 immediately.
        let bus = Arc::new(UplinkEventBus::new());
        let mut rx = bus.subscribe();
        let src = Arc::new(FixedSource(UsageBytes {
            rx_bytes: 4096,
            tx_bytes: 0,
        }));
        let mut t = DataCapTracker::with_config(
            src,
            Arc::clone(&bus),
            // 1 KiB cap.
            1.0 / (1024.0 * 1024.0),
            path,
        );
        t.poll_once().await;
        let evt = rx
            .try_recv()
            .expect("a threshold event on the first crossing");
        assert_eq!(evt.kind, UplinkEventKind::DataCapThreshold);
        assert_eq!(evt.data_cap_state, Some(DataCapState::Blocked100));
        // Second poll, same state → NO new event.
        t.poll_once().await;
        assert!(rx.try_recv().is_err(), "no event when state is unchanged");
    }

    #[test]
    fn get_usage_keys_match_python() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("modem-usage.json");
        let (t, _bus) = tracker(5.0, path, 0, 0);
        let u = t.get_usage();
        for k in [
            "data_used_mb",
            "cap_mb",
            "percent",
            "state",
            "window_reset_at",
            "last_reset_month",
        ] {
            assert!(u.get(k).is_some(), "missing key {k}");
        }
        assert_eq!(u["state"], "ok");
    }

    #[test]
    fn get_usage_payload_values_match_python() {
        // 1 GiB cap, 100 MiB used → 100 MB used, 1024 MB cap, ~10 percent, ok.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("modem-usage.json");
        let (mut t, _bus) = tracker(1.0, path, 0, 0);
        t.state.cumulative_bytes = 100 * 1024 * 1024;
        let u = t.get_usage();
        assert_eq!(u["data_used_mb"], 100);
        assert_eq!(u["cap_mb"], 1024);
        let pct = u["percent"].as_f64().unwrap();
        assert!((9.0..11.0).contains(&pct), "percent out of band: {pct}");
        assert_eq!(u["state"], "ok");
    }

    #[test]
    fn set_cap_updates_cap_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("modem-usage.json");
        let (mut t, _bus) = tracker(1.0, path, 0, 0);
        assert_eq!(t.cap_bytes, cap_to_bytes(1.0));
        t.set_cap(2.0);
        assert_eq!(t.cap_bytes, (2.0 * 1024.0 * 1024.0 * 1024.0) as u64);
    }

    #[test]
    fn load_state_tolerates_missing_fields() {
        // An empty JSON object loads as a fresh window: zeros + empty month,
        // never a parse error. Mirrors `_UsageState.from_json({})`.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("modem-usage.json");
        std::fs::write(&path, b"{}").unwrap();
        let st = load_state(&path);
        assert_eq!(st.cumulative_bytes, 0);
        assert_eq!(st.last_rx, 0);
        assert_eq!(st.last_tx, 0);
        assert_eq!(st.last_reset_month, "");
    }

    #[test]
    fn load_state_round_trips_all_fields() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("modem-usage.json");
        let written = UsageState {
            window_started_at: 1_700_000_000.0,
            cumulative_bytes: 12345,
            last_rx: 100,
            last_tx: 200,
            last_reset_month: "2026-04".to_string(),
        };
        std::fs::write(&path, written.render_json()).unwrap();
        let st = load_state(&path);
        assert_eq!(st.window_started_at, 1_700_000_000.0);
        assert_eq!(st.cumulative_bytes, 12345);
        assert_eq!(st.last_rx, 100);
        assert_eq!(st.last_tx, 200);
        assert_eq!(st.last_reset_month, "2026-04");
    }

    #[tokio::test]
    async fn two_polls_accumulate_against_the_previous_sample() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("modem-usage.json");
        // First poll baselines against zero → +1500.
        let bus = Arc::new(UplinkEventBus::new());
        let mut t = DataCapTracker::with_config(
            Arc::new(FixedSource(UsageBytes {
                rx_bytes: 1000,
                tx_bytes: 500,
            })),
            Arc::clone(&bus),
            1.0,
            path,
        );
        t.poll_once().await;
        assert_eq!(t.state.cumulative_bytes, 1500);
        // Second poll: rx 1000→1700 (+700), tx 500→800 (+300) → +1000.
        t.source = Arc::new(FixedSource(UsageBytes {
            rx_bytes: 1700,
            tx_bytes: 800,
        }));
        t.poll_once().await;
        assert_eq!(t.state.cumulative_bytes, 2500);
    }

    #[tokio::test]
    async fn poll_warn_80_emits_warn_threshold_event() {
        // A sample at 81 percent of the cap trips warn_80 on the first crossing.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("modem-usage.json");
        let bus = Arc::new(UplinkEventBus::new());
        let mut rx = bus.subscribe();
        let cap = cap_to_bytes(1.0);
        let at_81 = ((cap as f64) * 0.81) as u64;
        let mut t = DataCapTracker::with_config(
            Arc::new(FixedSource(UsageBytes {
                rx_bytes: at_81,
                tx_bytes: 0,
            })),
            Arc::clone(&bus),
            1.0,
            path,
        );
        t.poll_once().await;
        let evt = rx.try_recv().expect("a warn_80 threshold event");
        assert_eq!(evt.kind, UplinkEventKind::DataCapThreshold);
        assert_eq!(evt.data_cap_state, Some(DataCapState::Warn80));
    }

    #[test]
    fn flush_persists_bytes_accumulated_since_the_last_poll() {
        // A clean stop must persist the latest counter, not lose the bytes
        // observed mid-window. Mutate state without going through save_state,
        // then flush() must write them to disk.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("modem-usage.json");
        let (mut t, _bus) = tracker(1.0, path.clone(), 0, 0);
        t.state.cumulative_bytes = 9_999_999;
        // The on-disk file does not yet reflect this (no save happened).
        assert!(!path.exists());
        t.flush();
        let persisted = load_state(&path);
        assert_eq!(persisted.cumulative_bytes, 9_999_999);
    }

    #[test]
    fn flush_is_safe_before_any_poll() {
        // flush() must be callable even when poll_once was never run (a service
        // that crashes before its task is scheduled still flushes its counter).
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("modem-usage.json");
        let (mut t, _bus) = tracker(1.0, path.clone(), 0, 0);
        t.state.cumulative_bytes = 7;
        t.flush();
        assert_eq!(load_state(&path).cumulative_bytes, 7);
    }

    #[test]
    fn flush_failure_is_swallowed_and_does_not_panic() {
        // A flush failure during shutdown must never propagate: the save logs
        // and returns. Force the failure by rooting the state file under a
        // PARENT that is a regular file, so create_dir_all errors and the
        // atomic write cannot proceed.
        let dir = tempfile::tempdir().unwrap();
        let blocker = dir.path().join("blocker");
        std::fs::write(&blocker, b"i am a file, not a directory").unwrap();
        // `blocker/sub/modem-usage.json` cannot be created: `blocker` is a file.
        let path = blocker.join("sub").join("modem-usage.json");
        let (mut t, _bus) = tracker(1.0, path.clone(), 0, 0);
        t.state.cumulative_bytes = 1234;
        // Must not panic; the error is logged inside save_state.
        t.flush();
        // And the unwritable target was indeed never created.
        assert!(!path.exists());
    }

    #[test]
    fn current_month_is_year_dash_month() {
        let m = current_month();
        // YYYY-MM shape.
        assert_eq!(m.len(), 7);
        assert_eq!(&m[4..5], "-");
        assert!(m[0..4].chars().all(|c| c.is_ascii_digit()));
        assert!(m[5..7].chars().all(|c| c.is_ascii_digit()));
    }

    #[tokio::test]
    async fn poll_emits_a_modem_usage_event_when_an_emitter_is_attached() {
        // Each poll ships one `net.modem_usage` event carrying the same usage
        // body the modem view serves, regardless of whether the threshold
        // changed. The emitter counts every enqueue, so the stats are the proof.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("modem-usage.json");
        let bus = Arc::new(UplinkEventBus::new());
        let emitter = IngestEmitter::with_socket("ados-net", dir.path().join("ingest.sock"));
        let stats = emitter.stats();
        let mut t = DataCapTracker::with_config(
            Arc::new(FixedSource(UsageBytes {
                rx_bytes: 1000,
                tx_bytes: 500,
            })),
            Arc::clone(&bus),
            5.0,
            path,
        )
        .with_emitter(emitter);

        t.poll_once().await;
        assert_eq!(stats.enqueued(), 1);
        // A second poll (same state, no threshold transition) still emits the
        // fresh usage snapshot.
        t.poll_once().await;
        assert_eq!(stats.enqueued(), 2);
    }

    #[tokio::test]
    async fn sysfs_source_counts_only_wwan0_when_present() {
        // wwan0 present → count wwan0, never usb0 (even if usb0 also exists).
        let dir = tempfile::tempdir().unwrap();
        let net = dir.path().join("net");
        for (iface, rx, tx) in [("wwan0", "100", "200"), ("usb0", "9000", "9000")] {
            let stats = net.join(iface).join("statistics");
            std::fs::create_dir_all(&stats).unwrap();
            std::fs::write(stats.join("rx_bytes"), rx).unwrap();
            std::fs::write(stats.join("tx_bytes"), tx).unwrap();
        }
        let src = SysfsUsageSource::with_roots(net, dir.path().join("no-gadget"));
        let u = src.data_usage().await;
        assert_eq!(u.rx_bytes, 100);
        assert_eq!(u.tx_bytes, 200);
    }

    #[tokio::test]
    async fn sysfs_source_counts_usb0_as_modem_only_when_no_gadget() {
        // No wwan0, usb0 present, no provisioned gadget → usb0 IS the modem
        // RNDIS iface, so its counters are the cellular usage.
        let dir = tempfile::tempdir().unwrap();
        let net = dir.path().join("net");
        let stats = net.join("usb0").join("statistics");
        std::fs::create_dir_all(&stats).unwrap();
        std::fs::write(stats.join("rx_bytes"), "300").unwrap();
        std::fs::write(stats.join("tx_bytes"), "400").unwrap();
        let src = SysfsUsageSource::with_roots(net, dir.path().join("no-gadget"));
        let u = src.data_usage().await;
        assert_eq!(u.rx_bytes, 300);
        assert_eq!(u.tx_bytes, 400);
    }

    #[tokio::test]
    async fn usb0_only_board_with_no_modem_accrues_nothing_against_the_cap() {
        // The headline bug: usb0 is the USB-gadget tether NIC (a provisioned
        // gadget dir exists) and there is no modem. usb0 carries gigabytes of
        // a tethered laptop's traffic, but NONE of it is cellular, so the cap
        // tracker must see zero usage and never falsely throttle.
        let dir = tempfile::tempdir().unwrap();
        let net = dir.path().join("net");
        let stats = net.join("usb0").join("statistics");
        std::fs::create_dir_all(&stats).unwrap();
        std::fs::write(stats.join("rx_bytes"), "5000000000").unwrap();
        std::fs::write(stats.join("tx_bytes"), "5000000000").unwrap();
        // A provisioned gadget marker → usb0 belongs to the tether, not a modem.
        let gadget = dir.path().join("gadget");
        std::fs::create_dir_all(&gadget).unwrap();
        let src = SysfsUsageSource::with_roots(net, gadget);
        let u = src.data_usage().await;
        assert_eq!(u, UsageBytes::default(), "tether traffic must not count");

        // And drive it through a poll: a 1 KiB cap stays OK because nothing
        // accrued, so the cap never crosses a throttle threshold.
        let bus = Arc::new(UplinkEventBus::new());
        let mut rx = bus.subscribe();
        let mut t = DataCapTracker::with_config(
            Arc::new(src),
            Arc::clone(&bus),
            1.0 / (1024.0 * 1024.0), // 1 KiB cap
            dir.path().join("modem-usage.json"),
        );
        t.poll_once().await;
        assert_eq!(t.state.cumulative_bytes, 0, "no cellular bytes accrued");
        assert_eq!(t.classify(), DataCapState::Ok);
        // The only state the cap ever reaches is `ok` (the first-poll baseline
        // transition), never a throttle/block, because nothing was counted.
        if let Ok(evt) = rx.try_recv() {
            assert_eq!(
                evt.data_cap_state,
                Some(DataCapState::Ok),
                "a no-modem box must never throttle"
            );
        }
        // A second poll at the same (still-zero) usage emits no further event.
        t.poll_once().await;
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn sysfs_source_with_no_ifaces_reads_zero() {
        let dir = tempfile::tempdir().unwrap();
        let src = SysfsUsageSource::with_roots(dir.path().join("net"), dir.path().join("gadget"));
        assert_eq!(src.data_usage().await, UsageBytes::default());
    }

    #[tokio::test]
    async fn poll_without_an_emitter_enqueues_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("modem-usage.json");
        let bus = Arc::new(UplinkEventBus::new());
        let probe = IngestEmitter::with_socket("ados-net", dir.path().join("probe.sock"));
        let stats = probe.stats();
        let mut t = DataCapTracker::with_config(
            Arc::new(FixedSource(UsageBytes {
                rx_bytes: 1000,
                tx_bytes: 500,
            })),
            bus,
            5.0,
            path,
        );
        t.poll_once().await;
        assert_eq!(stats.enqueued(), 0);
    }
}
