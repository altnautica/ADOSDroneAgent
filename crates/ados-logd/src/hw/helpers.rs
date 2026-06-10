//! Pure shaping helpers shared across the collector tick and the async loop.
//!
//! Used-memory derivation, the metric-append helper, the dotted-key sanitizer,
//! the throttle-flags fold, and the channel emit. These hold no collector state,
//! so they live apart from the tick and the run loop.

use rmpv::Value as MpVal;
use tokio::sync::mpsc;

use ados_protocol::logd::{HwSnapshot, IngestFrame, TelemetryFrame};

use super::throttle::Throttle;

/// Used memory in MiB, derived from total minus available. Zero when either is
/// absent so the metric never reports a misleading negative.
pub(super) fn used_mb(mem: &super::memory::MemInfo) -> u64 {
    match (mem.total, mem.available) {
        (Some(t), Some(a)) if t >= a => (t - a) / (1024 * 1024),
        _ => 0,
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
