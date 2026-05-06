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
use std::sync::{Arc, Mutex};
use std::time::Instant;

use serde::Serialize;

/// Window in seconds for "recent" MQTT activity. A publish older than
/// this counts as not-recent. Picked to match the heartbeat-publish
/// cadence (5 s) plus generous slack so a slow link does not flap the
/// indicator on every iteration.
pub const MQTT_RECENT_WINDOW_SECS: u64 = 30;

/// Number of latency samples retained per histogram. 60 is small enough
/// that sort-on-snapshot stays cheap (microseconds, even on armv7) and
/// large enough to give a stable p95 / p99 over a five-minute window at
/// the 5 s heartbeat cadence and a typical 30 Hz MAVLink frame rate.
pub const METRIC_RING_CAPACITY: usize = 60;

/// Rolling window for the MAVLink frame-rate estimator. The cloud client
/// wakes a metrics task once per `METRIC_FRAME_RATE_WINDOW_SECS` and
/// divides the observed frame count by the window to produce a
/// frames-per-second figure. 30 s is long enough to ride out short
/// stalls and short enough that an operator hitting `/api/v1/diag` does
/// not stare at stale data.
pub const METRIC_FRAME_RATE_WINDOW_SECS: u64 = 30;

/// Lock-free-ish ring buffer of u32 samples with a fixed capacity. The
/// `Mutex` wrapping each instance is the only shared-mutability
/// primitive used; the lock is held for the duration of a single
/// `push` (one array store + one index increment) or a `snapshot_into`
/// (a memcpy-style copy out). Hold time is on the order of nanoseconds,
/// so contention with the diag handler is a non-issue.
///
/// `len` saturates at `CAP`; `write_idx` wraps modulo `CAP`. The order
/// of samples is not preserved for the caller — percentile work needs
/// sorted data anyway.
#[derive(Debug)]
pub struct RingBuffer<const CAP: usize> {
    samples: [u32; CAP],
    len: usize,
    write_idx: usize,
}

impl<const CAP: usize> RingBuffer<CAP> {
    /// Construct an empty buffer.
    pub const fn new() -> Self {
        Self {
            samples: [0u32; CAP],
            len: 0,
            write_idx: 0,
        }
    }

    /// Insert a new sample, overwriting the oldest entry once `len ==
    /// CAP`. O(1).
    pub fn push(&mut self, sample: u32) {
        if CAP == 0 {
            return;
        }
        self.samples[self.write_idx] = sample;
        self.write_idx = (self.write_idx + 1) % CAP;
        if self.len < CAP {
            self.len += 1;
        }
    }

    /// Number of valid samples in the buffer.
    pub fn len(&self) -> usize {
        self.len
    }

    /// True when no samples have been recorded.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Copy the live samples into the provided `out` slice and return
    /// the populated subslice. Caller is responsible for sorting if
    /// percentile math is needed. Returns an empty slice when
    /// `out.len() < self.len`.
    pub fn snapshot_into<'a>(&self, out: &'a mut [u32]) -> &'a mut [u32] {
        if out.len() < self.len {
            return &mut out[..0];
        }
        out[..self.len].copy_from_slice(&self.samples[..self.len]);
        &mut out[..self.len]
    }
}

impl<const CAP: usize> Default for RingBuffer<CAP> {
    fn default() -> Self {
        Self::new()
    }
}

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
    /// Round-trip-time samples (milliseconds) for the cloud heartbeat
    /// HTTPS POST. Updated by the cloud client's send_heartbeat path.
    /// `Mutex` is held for one array store; contention is a non-issue
    /// at the 5 s heartbeat cadence.
    heartbeat_rtt_ms: Mutex<RingBuffer<METRIC_RING_CAPACITY>>,
    /// Round-trip-time samples (milliseconds) for the MQTT publish
    /// awaitable. Updated on every successful publish in the publish
    /// loop. Lock is held for the duration of one push.
    mqtt_publish_ms: Mutex<RingBuffer<METRIC_RING_CAPACITY>>,
    /// Frames-per-second estimate over the last
    /// [`METRIC_FRAME_RATE_WINDOW_SECS`] seconds. Updated by a metrics
    /// task that ticks once per window.
    pub mavlink_frames_per_sec: AtomicU32,
    /// Running counter the publish loop bumps on every observed frame.
    /// Drained (swap-to-zero) by the metrics task each window. The
    /// observe path is hot, so this is an atomic add — never a Mutex.
    pub mavlink_frame_counter: AtomicU32,
    /// UNIX seconds of the last frame observation. Lets a stale-detector
    /// flag a router that has stopped delivering frames without making
    /// the metrics task perch on every observation.
    pub last_mavlink_frame_unix_seconds: AtomicU64,
}

impl Default for DiagState {
    fn default() -> Self {
        Self {
            boot_time: Instant::now(),
            mqtt_last_publish_unix_seconds: AtomicU64::new(0),
            cloud_last_heartbeat_unix_seconds: AtomicU64::new(0),
            cloud_consecutive_failures: AtomicU32::new(0),
            heartbeat_rtt_ms: Mutex::new(RingBuffer::new()),
            mqtt_publish_ms: Mutex::new(RingBuffer::new()),
            mavlink_frames_per_sec: AtomicU32::new(0),
            mavlink_frame_counter: AtomicU32::new(0),
            last_mavlink_frame_unix_seconds: AtomicU64::new(0),
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

    /// Record a heartbeat round-trip-time sample. Cheap on the heartbeat
    /// path (one mutex acquire + one array store).
    pub fn record_heartbeat_rtt(&self, ms: u32) {
        if let Ok(mut buf) = self.heartbeat_rtt_ms.lock() {
            buf.push(ms);
        }
    }

    /// Record a single MQTT publish round-trip-time sample. Hot-path
    /// callsite; the lock is held for ~one cache line of work.
    pub fn record_mqtt_publish_ms(&self, ms: u32) {
        if let Ok(mut buf) = self.mqtt_publish_ms.lock() {
            buf.push(ms);
        }
    }

    /// Bump the MAVLink frame counter and stamp the last-seen
    /// wall-clock. Drained by the metrics task once per
    /// [`METRIC_FRAME_RATE_WINDOW_SECS`] window. Pure atomics — safe to
    /// call from the broadcast subscriber inside the cloud publish loop.
    pub fn observe_mavlink_frame(&self) {
        // saturating add: in the unlikely event a misconfigured FC
        // blasts us past u32::MAX in 30 s the counter caps rather than
        // wrapping.
        let prev = self.mavlink_frame_counter.fetch_add(1, Ordering::Relaxed);
        if prev == u32::MAX {
            self.mavlink_frame_counter
                .store(u32::MAX, Ordering::Relaxed);
        }
        self.last_mavlink_frame_unix_seconds
            .store(now_unix_seconds(), Ordering::Relaxed);
    }

    /// Drain the frame counter and store the resulting frames-per-second
    /// value. Returns the rate that was published. Intended for the
    /// dedicated metrics task — call once per window.
    pub fn drain_mavlink_frame_rate(&self, window_secs: u64) -> u32 {
        let count = self.mavlink_frame_counter.swap(0, Ordering::Relaxed);
        let rate = if window_secs == 0 {
            0
        } else {
            (count as u64 / window_secs).min(u32::MAX as u64) as u32
        };
        self.mavlink_frames_per_sec.store(rate, Ordering::Relaxed);
        rate
    }

    /// Snapshot the metrics block for the diag handler. Allocates two
    /// short Vecs (≤60 u32 each) and sorts them in place to compute
    /// percentiles. Cheap (~microseconds on armv7).
    pub fn metrics_snapshot(&self) -> MetricsDiagSnapshot {
        let heartbeat = self.summarize_buffer(&self.heartbeat_rtt_ms);
        let mqtt = self.summarize_buffer(&self.mqtt_publish_ms);
        MetricsDiagSnapshot {
            heartbeat_rtt_ms: heartbeat,
            mqtt_publish_ms: mqtt,
            mavlink: MavlinkRateSnapshot {
                frames_per_sec: self.mavlink_frames_per_sec.load(Ordering::Relaxed),
            },
            // The supervisor publishes its own snapshot via a separate
            // accessor; the diag handler is free to merge that in. Keep
            // the metrics-only path null-safe.
            rkmpi: None,
        }
    }

    fn summarize_buffer(
        &self,
        buf: &Mutex<RingBuffer<METRIC_RING_CAPACITY>>,
    ) -> LatencyHistogramSnapshot {
        let mut scratch = [0u32; METRIC_RING_CAPACITY];
        let live: Vec<u32> = match buf.lock() {
            Ok(guard) => guard.snapshot_into(&mut scratch).to_vec(),
            Err(_) => Vec::new(),
        };
        if live.is_empty() {
            return LatencyHistogramSnapshot::default();
        }
        let mut sorted = live;
        sorted.sort_unstable();
        LatencyHistogramSnapshot {
            p50: percentile(&sorted, 0.50),
            p95: percentile(&sorted, 0.95),
            p99: percentile(&sorted, 0.99),
            count: sorted.len() as u32,
        }
    }
}

/// Compute the requested percentile from a pre-sorted slice using
/// nearest-rank. `p` is clamped to `[0.0, 1.0]`. Returns `None` for an
/// empty slice. Suitable for tiny sample sets (≤ 60) where exact
/// interpolation gains nothing.
pub fn percentile(sorted: &[u32], p: f32) -> Option<u32> {
    if sorted.is_empty() {
        return None;
    }
    let p = p.clamp(0.0, 1.0);
    // Nearest-rank: rank = ceil(p * n). For n=60 and p=0.95 the rank is
    // 57; index 56 (zero-based). Saturating sub keeps us in-bounds for
    // p=0.0 where rank rounds to 0.
    let n = sorted.len();
    let rank = (p * n as f32).ceil() as usize;
    let idx = rank.saturating_sub(1).min(n - 1);
    Some(sorted[idx])
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

/// Latency-histogram snapshot for one of the timed code paths.
/// Percentile fields are `None` until at least one sample has been
/// recorded; `count` is always present and starts at 0.
#[derive(Debug, Clone, Serialize, PartialEq, Eq, Default)]
pub struct LatencyHistogramSnapshot {
    /// 50th-percentile latency in milliseconds, or `None` when the
    /// underlying ring buffer is empty.
    pub p50: Option<u32>,
    /// 95th-percentile latency in milliseconds.
    pub p95: Option<u32>,
    /// 99th-percentile latency in milliseconds.
    pub p99: Option<u32>,
    /// Number of samples currently retained.
    pub count: u32,
}

/// Mavlink frame-rate snapshot. Single field today; structured so the
/// shape can grow (lag, dropped frames) without a JSON breaking change.
#[derive(Debug, Clone, Serialize, PartialEq, Eq, Default)]
pub struct MavlinkRateSnapshot {
    /// Frames per second observed over the most recent window. 0 when
    /// no frames have been seen yet.
    pub frames_per_sec: u32,
}

/// RKMPI subprocess supervisor snapshot exposed on the diag surface.
/// Every field is optional / nullable so the JSON shape stays stable
/// for boards that do not run the rkmpi backend at all (`running ==
/// false`, every other field defaulted / `None`).
#[derive(Debug, Clone, Serialize, PartialEq, Eq, Default)]
pub struct RkmpiSupervisorSnapshot {
    /// True while a wrapper child is alive and producing frames.
    pub running: bool,
    /// Wall-clock seconds since the current child started. 0 when
    /// `running == false`.
    pub uptime_secs: u64,
    /// Number of respawns since program start (or the most recent
    /// circuit-breaker reset).
    pub restart_count: u32,
    /// Exit code of the most recently reaped child, or `None` when the
    /// child was killed by signal / no child has exited yet.
    pub last_exit_code: Option<i32>,
    /// Symbolic name of the signal that killed the most recently reaped
    /// child (e.g. `"SIGKILL"`), or `None`.
    pub last_exit_signal: Option<String>,
    /// UNIX seconds of the most recent respawn.
    pub last_restart_unix: Option<u64>,
    /// Resident-set-size of the current child in kilobytes, read live
    /// from `/proc/<pid>/status`.
    pub rss_kb: Option<u64>,
    /// True when the circuit breaker is in its holdoff window.
    pub circuit_breaker_open: bool,
    /// UNIX seconds of the most recent `Frame` response.
    pub last_frame_unix: Option<u64>,
}

/// Top-level metrics block for `/api/v1/diag`. Mirrors the `metrics`
/// section of `proto/setup/setup-api.yaml::DiagResponse`.
#[derive(Debug, Clone, Serialize, PartialEq, Eq, Default)]
pub struct MetricsDiagSnapshot {
    pub heartbeat_rtt_ms: LatencyHistogramSnapshot,
    pub mqtt_publish_ms: LatencyHistogramSnapshot,
    pub mavlink: MavlinkRateSnapshot,
    /// RKMPI supervisor state. `None` on boards / agents that never
    /// register a supervisor; the JSON serialiser emits the field as
    /// `null` so the response shape stays stable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rkmpi: Option<RkmpiSupervisorSnapshot>,
}

/// Read VmRSS for either the calling process (`pid = None`) or an
/// arbitrary pid from `/proc/<pid>/status`. The default unit is
/// megabytes — matching what operators see in `top` / `htop` — so the
/// existing `/api/v1/diag` shape stays directly comparable to system
/// tools.
///
/// `/proc/self/status` and `/proc/<pid>/status` report VmRSS in
/// kilobytes per the Linux proc(5) man page (the suffix is " kB" but
/// the value is binary KiB). The function divides by 1024 for the MB
/// conversion. Callers that need raw kilobytes (e.g. the rkmpi
/// supervisor's child snapshot) should use [`read_rss_kb`] instead.
///
/// Returns `None` on non-Linux hosts, when the pid has gone away, or
/// when the VmRSS line is absent.
pub fn read_rss_mb(pid: Option<u32>) -> Option<u64> {
    let kb = read_rss_kb(pid)?;
    Some(kb / 1024)
}

/// Read VmRSS in kilobytes for either the calling process (`pid =
/// None`) or an arbitrary pid. Same semantics as [`read_rss_mb`] but
/// without the unit conversion, so the rkmpi supervisor can publish a
/// kilobyte value for its child wrapper without doing a round-trip
/// multiply.
pub fn read_rss_kb(pid: Option<u32>) -> Option<u64> {
    let path = match pid {
        None => "/proc/self/status".to_string(),
        Some(p) => format!("/proc/{p}/status"),
    };
    // On macOS / dev hosts /proc/<pid>/status does not exist. Bail
    // silently; the diag response surfaces the absence as `null`.
    let raw = std::fs::read_to_string(&path).ok()?;
    for line in raw.lines() {
        if let Some(rest) = line.strip_prefix("VmRSS:") {
            // Format: "VmRSS:\t   12345 kB"
            let trimmed = rest.trim();
            let kb_str = trimmed.split_whitespace().next()?;
            let kb: u64 = kb_str.parse().ok()?;
            return Some(kb);
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
    fn ring_buffer_wraps_around_on_overflow() {
        // Push 1.5x the capacity; the buffer must hold the last `CAP`
        // samples and report `len() == CAP`.
        let mut rb: RingBuffer<3> = RingBuffer::new();
        assert!(rb.is_empty());
        rb.push(1);
        rb.push(2);
        rb.push(3);
        rb.push(4);
        rb.push(5);
        assert_eq!(rb.len(), 3);
        let mut scratch = [0u32; 3];
        let live = rb.snapshot_into(&mut scratch);
        let mut sorted = live.to_vec();
        sorted.sort_unstable();
        assert_eq!(sorted, vec![3, 4, 5]);
    }

    #[test]
    fn ring_buffer_zero_capacity_is_inert() {
        // A zero-capacity ring is degenerate but must not panic. Pushes
        // are no-ops; len stays 0.
        let mut rb: RingBuffer<0> = RingBuffer::new();
        rb.push(42);
        assert_eq!(rb.len(), 0);
        assert!(rb.is_empty());
    }

    #[test]
    fn percentile_handles_edge_cases() {
        // Empty -> None.
        assert_eq!(percentile(&[], 0.5), None);
        // Single sample -> always returns it regardless of percentile.
        assert_eq!(percentile(&[42], 0.0), Some(42));
        assert_eq!(percentile(&[42], 0.5), Some(42));
        assert_eq!(percentile(&[42], 1.0), Some(42));
        // Out-of-range p clamps; 1.5 == 1.0 (the maximum); -1.0 == 0.0.
        let sorted: Vec<u32> = (1..=100).collect();
        assert_eq!(percentile(&sorted, 1.5), Some(100));
        assert_eq!(percentile(&sorted, -1.0), Some(1));
        // p50 of 1..=100 with nearest-rank: rank = ceil(0.5 * 100) = 50,
        // index 49 -> value 50.
        assert_eq!(percentile(&sorted, 0.50), Some(50));
        assert_eq!(percentile(&sorted, 0.95), Some(95));
        assert_eq!(percentile(&sorted, 0.99), Some(99));
    }

    #[test]
    fn metrics_snapshot_empty_returns_zero_count_and_null_percentiles() {
        let s = DiagState::new();
        let snap = s.metrics_snapshot();
        assert_eq!(snap.heartbeat_rtt_ms.count, 0);
        assert_eq!(snap.heartbeat_rtt_ms.p50, None);
        assert_eq!(snap.heartbeat_rtt_ms.p95, None);
        assert_eq!(snap.heartbeat_rtt_ms.p99, None);
        assert_eq!(snap.mqtt_publish_ms.count, 0);
        assert_eq!(snap.mqtt_publish_ms.p50, None);
        assert_eq!(snap.mavlink.frames_per_sec, 0);
    }

    #[test]
    fn record_heartbeat_rtt_populates_snapshot() {
        let s = DiagState::new();
        // Record a known distribution: 1..=100 ms. Buffer holds 60
        // samples so only the last 60 (41..=100) survive.
        for ms in 1u32..=100 {
            s.record_heartbeat_rtt(ms);
        }
        let snap = s.metrics_snapshot();
        assert_eq!(snap.heartbeat_rtt_ms.count, 60);
        // Retained set 41..=100. p50 -> rank ceil(0.5 * 60) = 30, index
        // 29 -> 70. p95 -> rank 57, index 56 -> 97. p99 -> rank 60,
        // index 59 -> 100.
        assert_eq!(snap.heartbeat_rtt_ms.p50, Some(70));
        assert_eq!(snap.heartbeat_rtt_ms.p95, Some(97));
        assert_eq!(snap.heartbeat_rtt_ms.p99, Some(100));
    }

    #[test]
    fn record_mqtt_publish_ms_populates_snapshot() {
        let s = DiagState::new();
        for ms in [3u32, 1, 4, 1, 5, 9, 2, 6, 5, 3, 5, 8, 9, 7, 9, 3, 2, 3, 8, 4] {
            s.record_mqtt_publish_ms(ms);
        }
        let snap = s.metrics_snapshot();
        assert_eq!(snap.mqtt_publish_ms.count, 20);
        // Sorted: [1,1,2,2,3,3,3,3,4,4,5,5,5,6,7,8,8,9,9,9].
        // p50 -> rank 10, index 9 -> 4. p95 -> rank 19, index 18 -> 9.
        // p99 -> rank 20, index 19 -> 9.
        assert_eq!(snap.mqtt_publish_ms.p50, Some(4));
        assert_eq!(snap.mqtt_publish_ms.p95, Some(9));
        assert_eq!(snap.mqtt_publish_ms.p99, Some(9));
    }

    #[test]
    fn observe_mavlink_frame_drains_to_rate() {
        let s = DiagState::new();
        for _ in 0..900 {
            s.observe_mavlink_frame();
        }
        // Window 30 s with 900 frames -> 30 fps.
        let rate = s.drain_mavlink_frame_rate(30);
        assert_eq!(rate, 30);
        let snap = s.metrics_snapshot();
        assert_eq!(snap.mavlink.frames_per_sec, 30);
        // Counter must be drained — the next drain returns 0.
        let next = s.drain_mavlink_frame_rate(30);
        assert_eq!(next, 0);
    }

    #[test]
    fn drain_mavlink_frame_rate_zero_window_is_safe() {
        // Defensive: zero-second windows must not panic with a divide-
        // by-zero. We treat the rate as 0 in that case.
        let s = DiagState::new();
        s.observe_mavlink_frame();
        let rate = s.drain_mavlink_frame_rate(0);
        assert_eq!(rate, 0);
    }

    #[test]
    fn read_rss_mb_returns_some_on_linux() {
        // On macOS this returns None; on Linux it should produce a
        // positive value. Either is correct — the helper just must not
        // panic and must not return Some(0) on Linux (that would imply
        // a parse bug).
        if std::path::Path::new("/proc/self/status").exists() {
            let rss = read_rss_mb(None).expect("VmRSS should be parsable on Linux");
            assert!(rss > 0, "VmRSS should be a positive MB value");
        }
    }

    #[test]
    fn read_rss_kb_self_matches_mb_within_unit_rounding() {
        if std::path::Path::new("/proc/self/status").exists() {
            let kb = read_rss_kb(None).expect("VmRSS kb should be parsable on Linux");
            let mb = read_rss_mb(None).expect("VmRSS mb should be parsable on Linux");
            // mb is kb / 1024 with floor; the two readings can fall on
            // either side of a kilobyte boundary, so we just check that
            // the floor relationship holds within one MB of slack.
            assert!(kb / 1024 >= mb.saturating_sub(1));
            assert!(kb / 1024 <= mb.saturating_add(1));
        }
    }

    #[test]
    fn read_rss_kb_returns_none_for_bogus_pid() {
        // u32::MAX is reserved by the kernel and never used as a pid.
        assert_eq!(read_rss_kb(Some(u32::MAX)), None);
        assert_eq!(read_rss_mb(Some(u32::MAX)), None);
    }
}
