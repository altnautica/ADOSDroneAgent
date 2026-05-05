//! Live diagnostic state for `/api/v1/health` and `/api/v1/diag`.
//!
//! [`DiagState`] holds counters and timestamps the runtime tasks update
//! cheaply (atomics, no locks on the hot path) so the diag handler can
//! snapshot a coherent JSON response without blocking the runtime.
//!
//! Wiring is intentionally narrow: the agent binary constructs one
//! `Arc<DiagState>` at startup and (a) hands a clone to the axum
//! state-extension layer for the diag handler and (b) hands clones to
//! whichever cloud-relay or MAVLink tasks track the relevant counters.
//! When a counter is not yet wired (v0.1 placeholder), the field stays at
//! its default value and the diag response surfaces it as `null` / `0` so
//! the response shape is stable for future work.
//!
//! Concrete v0.1 surface coverage:
//!
//! - `boot_time` is captured at construction and used to derive
//!   `uptime_seconds`.
//! - `mqtt.connected_recently` is derived from
//!   `mqtt_last_publish_unix_seconds`: true if a publish landed in the
//!   last 30 seconds.
//! - `cloud_relay.last_heartbeat_at` is fed by
//!   `cloud_last_heartbeat_unix_seconds`. When still 0, the diag response
//!   emits `null`.
//! - `cloud_relay.consecutive_failures` is fed by
//!   `cloud_consecutive_failures`.
//! - `mavlink.frame_rate_recent` is `null` at v0.1 (no frame-rate
//!   estimator wired yet).
//! - `rss_mb` is read live from `/proc/self/status` so a Linux operator
//!   can spot a leak without restarting the agent.

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

use serde::Serialize;

/// Window in seconds for "recent" MQTT activity. A publish older than
/// this counts as not-recent. Picked to match the heartbeat-publish
/// cadence (5 s) plus generous slack so a slow link does not flap the
/// indicator on every iteration.
pub const MQTT_RECENT_WINDOW_SECS: u64 = 30;

/// Shared diagnostic state. Cheap to clone (it is wrapped in [`Arc`] by
/// the agent binary; this struct itself is not [`Clone`] because its
/// atomics carry shared semantics).
pub struct DiagState {
    /// Process boot wall-clock anchor for `uptime_seconds`.
    boot_time: Instant,
    /// UNIX timestamp (seconds) of the most recent successful MQTT
    /// publish. 0 means "never". Updated by the cloud relay's publish
    /// loop.
    pub mqtt_last_publish_unix_seconds: AtomicU64,
    /// UNIX timestamp (seconds) of the most recent successful HTTP
    /// heartbeat to Convex. 0 means "never".
    pub cloud_last_heartbeat_unix_seconds: AtomicU64,
    /// Running count of consecutive heartbeat failures. Reset to 0 on
    /// each success.
    pub cloud_consecutive_failures: AtomicU32,
}

impl Default for DiagState {
    fn default() -> Self {
        Self {
            boot_time: Instant::now(),
            mqtt_last_publish_unix_seconds: AtomicU64::new(0),
            cloud_last_heartbeat_unix_seconds: AtomicU64::new(0),
            cloud_consecutive_failures: AtomicU32::new(0),
        }
    }
}

impl DiagState {
    /// Construct a fresh state. Equivalent to [`Default::default`]; named
    /// constructor for callsite readability.
    pub fn new() -> Self {
        Self::default()
    }

    /// Wrap in [`Arc`] so callers can hand clones to the axum layer plus
    /// the cloud / MAVLink tasks without re-typing the wrap.
    pub fn shared() -> Arc<Self> {
        Arc::new(Self::new())
    }

    /// Seconds since process boot.
    pub fn uptime_seconds(&self) -> u64 {
        self.boot_time.elapsed().as_secs()
    }

    /// True when a successful MQTT publish landed within the recent
    /// window. False otherwise (including the never-published case).
    pub fn mqtt_connected_recently(&self, now_unix: u64) -> bool {
        let last = self.mqtt_last_publish_unix_seconds.load(Ordering::Relaxed);
        if last == 0 {
            return false;
        }
        now_unix.saturating_sub(last) <= MQTT_RECENT_WINDOW_SECS
    }

    /// Snapshot the cloud-relay fields. `last_heartbeat_at` is `None` when
    /// no heartbeat has succeeded yet.
    pub fn cloud_snapshot(&self) -> CloudDiagSnapshot {
        let last = self
            .cloud_last_heartbeat_unix_seconds
            .load(Ordering::Relaxed);
        CloudDiagSnapshot {
            last_heartbeat_at: if last == 0 { None } else { Some(last) },
            consecutive_failures: self.cloud_consecutive_failures.load(Ordering::Relaxed),
        }
    }

    /// Record a successful MQTT publish at `now_unix`. Cheap; intended for
    /// the publish hot path.
    pub fn record_mqtt_publish(&self, now_unix: u64) {
        self.mqtt_last_publish_unix_seconds
            .store(now_unix, Ordering::Relaxed);
    }

    /// Record a successful cloud heartbeat at `now_unix`. Resets the
    /// consecutive-failure counter.
    pub fn record_cloud_heartbeat(&self, now_unix: u64) {
        self.cloud_last_heartbeat_unix_seconds
            .store(now_unix, Ordering::Relaxed);
        self.cloud_consecutive_failures.store(0, Ordering::Relaxed);
    }

    /// Record a failed cloud heartbeat. Increments the consecutive
    /// counter; saturating so a long outage cannot wrap to 0.
    pub fn record_cloud_failure(&self) {
        let prev = self
            .cloud_consecutive_failures
            .fetch_add(1, Ordering::Relaxed);
        if prev == u32::MAX {
            self.cloud_consecutive_failures
                .store(u32::MAX, Ordering::Relaxed);
        }
    }
}

/// Cloud-relay slice of the diag snapshot.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct CloudDiagSnapshot {
    /// UNIX seconds of the last heartbeat that returned 2xx. `None` when
    /// no heartbeat has ever succeeded.
    pub last_heartbeat_at: Option<u64>,
    /// Heartbeat failures since the last success.
    pub consecutive_failures: u32,
}

/// Read RSS in megabytes from `/proc/self/status`. Returns `None` on
/// non-Linux hosts or when the field is absent. The MB unit is the same
/// one operators see in `top` / `htop`, which keeps the diag surface
/// directly comparable to system tools.
///
/// `/proc/self/status` `VmRSS` is reported in kilobytes per the Linux
/// proc(5) man page (the suffix is " kB" but the value is binary KiB).
/// We divide by 1024 for the MB conversion.
pub fn read_rss_mb() -> Option<u64> {
    // On macOS / dev hosts /proc/self/status does not exist. Bail
    // silently; the diag response surfaces the absence as `null`.
    let raw = std::fs::read_to_string("/proc/self/status").ok()?;
    for line in raw.lines() {
        if let Some(rest) = line.strip_prefix("VmRSS:") {
            // Format: "VmRSS:\t   12345 kB"
            let trimmed = rest.trim();
            let kb_str = trimmed.split_whitespace().next()?;
            let kb: u64 = kb_str.parse().ok()?;
            return Some(kb / 1024);
        }
    }
    None
}

/// Return the current UNIX time in seconds. Uses the system clock; this
/// is the same source the cloud relay uses, so the "recent publish"
/// comparison stays internally consistent.
pub fn now_unix_seconds() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_state_reports_no_publish() {
        let s = DiagState::new();
        assert!(!s.mqtt_connected_recently(now_unix_seconds()));
        let snap = s.cloud_snapshot();
        assert_eq!(snap.last_heartbeat_at, None);
        assert_eq!(snap.consecutive_failures, 0);
    }

    #[test]
    fn record_mqtt_publish_marks_recent() {
        let s = DiagState::new();
        let now = now_unix_seconds();
        s.record_mqtt_publish(now);
        assert!(s.mqtt_connected_recently(now));
        // Far in the future = stale.
        assert!(!s.mqtt_connected_recently(now + MQTT_RECENT_WINDOW_SECS + 1));
    }

    #[test]
    fn record_cloud_heartbeat_resets_failures() {
        let s = DiagState::new();
        s.record_cloud_failure();
        s.record_cloud_failure();
        assert_eq!(s.cloud_snapshot().consecutive_failures, 2);
        let now = now_unix_seconds();
        s.record_cloud_heartbeat(now);
        let snap = s.cloud_snapshot();
        assert_eq!(snap.last_heartbeat_at, Some(now));
        assert_eq!(snap.consecutive_failures, 0);
    }

    #[test]
    fn cloud_failure_saturates_at_max() {
        let s = DiagState::new();
        s.cloud_consecutive_failures
            .store(u32::MAX, Ordering::Relaxed);
        s.record_cloud_failure();
        assert_eq!(s.cloud_snapshot().consecutive_failures, u32::MAX);
    }

    #[test]
    fn uptime_is_monotonic_nonnegative() {
        let s = DiagState::new();
        let a = s.uptime_seconds();
        std::thread::sleep(std::time::Duration::from_millis(5));
        let b = s.uptime_seconds();
        assert!(b >= a);
    }

    #[test]
    fn read_rss_mb_returns_some_on_linux() {
        // On macOS this returns None; on Linux it should produce a
        // positive value. Either is correct — the helper just must not
        // panic and must not return Some(0) on Linux (that would imply
        // a parse bug).
        if std::path::Path::new("/proc/self/status").exists() {
            let rss = read_rss_mb().expect("VmRSS should be parsable on Linux");
            assert!(rss > 0, "VmRSS should be a positive MB value");
        }
    }
}
