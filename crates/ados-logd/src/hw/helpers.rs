//! Pure shaping helpers shared across the collector tick and the async loop.
//!
//! Used-memory derivation, the metric-append helper, the dotted-key sanitizer,
//! the throttle-flags fold, and the channel emit. These hold no collector state,
//! so they live apart from the tick and the run loop.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use rmpv::Value as MpVal;
use tokio::sync::mpsc;

use ados_protocol::logd::{HwSnapshot, IngestFrame, TelemetryFrame};

use super::pss::ServiceMemory;
use super::throttle::Throttle;

/// Used memory in MiB, derived from total minus available. Zero when either is
/// absent so the metric never reports a misleading negative.
pub(super) fn used_mb(mem: &super::memory::MemInfo) -> u64 {
    match (mem.total, mem.available) {
        (Some(t), Some(a)) if t >= a => (t - a) / (1024 * 1024),
        _ => 0,
    }
}

/// How long a signal may go unrecorded while it sits still.
///
/// A gated signal still lands at least this often, so a flat value is never
/// mistaken for a dead producer and every one-minute rollup bucket contains at
/// least one sample. Half the bucket width, so a bucket cannot be missed by
/// phase alone.
pub(super) const METRIC_HEARTBEAT: Duration = Duration::from_secs(30);

/// Per-signal record of what was last written, for the change gate.
#[derive(Debug, Default)]
pub(super) struct EmitGate {
    last: HashMap<String, (f64, Instant)>,
}

impl EmitGate {
    /// Whether `value` is worth a row, and remember it when it is.
    ///
    /// The fast classes are sampled fast on purpose — a thermal transient is the
    /// canary for a throttle — but sampling fast and *storing* every sample are
    /// different things, and only the second costs flash. On a real node the
    /// hardware collector was writing 165 rows/sec of which the top sixteen keys
    /// were all thermal, on a board whose own rollups are minute-grained.
    ///
    /// So: store a sample when it actually moved (beyond `deadband`, since a
    /// millidegree sensor never reads exactly the same twice), or when the
    /// signal has been quiet for [`METRIC_HEARTBEAT`]. A transient still lands
    /// immediately, which is the property the fast cadence exists for.
    pub(super) fn should_emit(
        &mut self,
        key: &str,
        value: f64,
        deadband: f64,
        now: Instant,
    ) -> bool {
        match self.last.get(key) {
            Some(&(prev, at))
                if (value - prev).abs() < deadband && now.duration_since(at) < METRIC_HEARTBEAT =>
            {
                false
            }
            _ => {
                self.last.insert(key.to_string(), (value, now));
                true
            }
        }
    }
}

/// Append one telemetry metric with optional string tags.
pub(super) fn push_metric(
    out: &mut Vec<TelemetryFrame>,
    ts_us: i64,
    metric: &str,
    value: f64,
    tags: &[(&str, &str)],
) {
    let mut frame = TelemetryFrame::new(ts_us, metric, value);
    for (k, v) in tags {
        frame.tags.insert((*k).to_string(), MpVal::from(*v));
    }
    out.push(frame);
}

/// Sanitize a name fragment for use inside a dotted signal/metric key: lower-case
/// it and replace any character that is not `[a-z0-9]` with `_`, so a chip /
/// zone / iface name with spaces or punctuation cannot break the dotted-key
/// convention.
pub(super) fn sanitize(name: &str) -> String {
    name.chars()
        .map(|c| {
            let c = c.to_ascii_lowercase();
            if c.is_ascii_alphanumeric() {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Fold the decoded throttle flags into a snapshot + the `throttle.flags` metric.
pub(super) fn fold_throttle(
    t: Throttle,
    ts: i64,
    snap: &mut HwSnapshot,
    metrics: &mut Vec<TelemetryFrame>,
) {
    snap.signals
        .insert("throttle.raw".to_string(), MpVal::from(t.raw));
    snap.signals.insert(
        "throttle.under_voltage".to_string(),
        MpVal::from(t.under_voltage),
    );
    snap.signals.insert(
        "throttle.freq_capped".to_string(),
        MpVal::from(t.freq_capped),
    );
    snap.signals
        .insert("throttle.throttled".to_string(), MpVal::from(t.throttled));
    snap.signals.insert(
        "throttle.soft_temp_limit".to_string(),
        MpVal::from(t.soft_temp_limit),
    );
    push_metric(metrics, ts, "throttle.flags", t.raw as f64, &[]);
}

/// Fold per-service proportional memory into the snapshot + per-service metrics.
///
/// Each service contributes a `mem.service.<name>.pss_kib` signal in the snapshot
/// blob and a `mem.service.pss_kib` metric tagged with the service name, mirroring
/// the per-entity shape the net / USB classes use (a blob key per entity plus a
/// tagged time series). The service name is sanitized for the dotted signal key so
/// a unit name cannot break the key convention; the untouched name rides in the
/// metric tag. An empty service list contributes nothing.
pub(super) fn fold_service_memory(
    services: &[ServiceMemory],
    ts: i64,
    snap: &mut HwSnapshot,
    metrics: &mut Vec<TelemetryFrame>,
) {
    for svc in services {
        let key = format!("mem.service.{}.pss_kib", sanitize(&svc.name));
        snap.signals.insert(key, MpVal::from(svc.pss_kib));
        push_metric(
            metrics,
            ts,
            "mem.service.pss_kib",
            svc.pss_kib as f64,
            &[("service", &svc.name)],
        );
    }
}

/// Send a snapshot and its metric frames into the ingest channel.
///
/// A snapshot that carries no signals is not emitted: a board where nothing was
/// readable on a tick (no `/sys`, no `/proc`) produces no row rather than a
/// stream of empty snapshots. When at least one signal was read, the snapshot
/// and every metric are pushed.
///
/// The hardware stream is low-severity: on a full channel the snapshot and the
/// metrics are dropped by the channel (the daemon's drop policy sheds them), so
/// the collector never blocks the runtime waiting for capacity. `try_send` is
/// used precisely so a saturated writer cannot stall sampling.
pub(super) fn emit(
    tx: &mpsc::Sender<IngestFrame>,
    snapshot: HwSnapshot,
    metrics: Vec<IngestFrame>,
) {
    if snapshot.signals.is_empty() {
        return;
    }
    let _ = tx.try_send(IngestFrame::Hw(snapshot));
    for frame in metrics {
        let _ = tx.try_send(frame);
    }
}
