//! The live tail: a broadcast-fed Server-Sent-Events stream.
//!
//! The tail is fed by the writer's broadcast channel, never by polling the DB,
//! so a live stream never contends on the store and the writer never blocks on
//! a slow tail client. Each persisted, redacted frame the writer fans out is
//! filtered against the request's filter set and, when it matches, serialized as
//! one SSE `data:` event. A subscriber that falls behind the broadcast buffer is
//! told it lagged (and how many it missed) rather than blocking ingest; a
//! subscriber cap keeps a flood of tail clients from pinning the box.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use ados_protocol::logd::{IngestFrame, Level};

use super::params::{QueryFilters, Table};

/// The cap on concurrent tail subscribers across the whole daemon. The
/// `(MAX + 1)`th connection is refused with 429.
pub const MAX_TAIL_SUBSCRIBERS: usize = 16;

/// The cap on concurrent streamed exports across the whole daemon. Each export
/// holds a dedicated blocking thread, a read-only connection, and (for the
/// compressed format) a zstd encoder for its whole lifetime, so a bulk export
/// is far heavier than a query. A low cap keeps a flood of concurrent exports
/// from pinning a small board; the `(MAX + 1)`th is refused with 429. This is
/// the bulk-stream concurrency limit that sits alongside the per-second read
/// budget on the LAN edge.
pub const MAX_CONCURRENT_EXPORTS: usize = 2;

/// A live counter of in-flight exports, shared in the app state. A guard frees
/// the slot on drop so an export that ends (completes or the client
/// disconnects) always releases it. Mirrors [`TailSlots`].
#[derive(Debug, Default)]
pub struct ExportSlots {
    active: AtomicUsize,
}

impl ExportSlots {
    /// Try to claim an export slot. Returns a guard that frees the slot on drop,
    /// or `None` when the cap is already reached.
    pub fn try_acquire(self: &Arc<Self>) -> Option<ExportGuard> {
        loop {
            let n = self.active.load(Ordering::Relaxed);
            if n >= MAX_CONCURRENT_EXPORTS {
                return None;
            }
            if self
                .active
                .compare_exchange_weak(n, n + 1, Ordering::AcqRel, Ordering::Relaxed)
                .is_ok()
            {
                return Some(ExportGuard {
                    slots: Arc::clone(self),
                });
            }
        }
    }

    /// The number of export slots currently held (for tests).
    pub fn active(&self) -> usize {
        self.active.load(Ordering::Relaxed)
    }
}

/// Frees its export slot when dropped (when the export stream ends).
pub struct ExportGuard {
    slots: Arc<ExportSlots>,
}

impl Drop for ExportGuard {
    fn drop(&mut self) {
        self.slots.active.fetch_sub(1, Ordering::AcqRel);
    }
}

/// A live counter of active tail subscribers, shared in the app state. A guard
/// decrements it on drop so a dropped connection always frees its slot.
#[derive(Debug, Default)]
pub struct TailSlots {
    active: AtomicUsize,
}

impl TailSlots {
    /// Try to claim a tail slot. Returns a guard that frees the slot on drop, or
    /// `None` when the cap is already reached.
    pub fn try_acquire(self: &Arc<Self>) -> Option<TailGuard> {
        // Optimistic CAS loop bounded by the cap.
        loop {
            let n = self.active.load(Ordering::Relaxed);
            if n >= MAX_TAIL_SUBSCRIBERS {
                return None;
            }
            if self
                .active
                .compare_exchange_weak(n, n + 1, Ordering::AcqRel, Ordering::Relaxed)
                .is_ok()
            {
                return Some(TailGuard {
                    slots: Arc::clone(self),
                });
            }
        }
    }

    /// The number of slots currently held (for stats/tests).
    pub fn active(&self) -> usize {
        self.active.load(Ordering::Relaxed)
    }
}

/// Frees its tail slot when dropped (when the SSE stream ends or the client
/// disconnects).
pub struct TailGuard {
    slots: Arc<TailSlots>,
}

impl Drop for TailGuard {
    fn drop(&mut self) {
        self.slots.active.fetch_sub(1, Ordering::AcqRel);
    }
}

/// True when a fanned-out frame matches the tail's filter set. The tail accepts
/// the same filters as `query`: the table selector picks the frame variant, and
/// the source/metric/event-kind/level/text restrictions apply within it.
pub fn frame_matches(frame: &IngestFrame, filters: &QueryFilters) -> bool {
    match (filters.table, frame) {
        (Table::Logs, IngestFrame::Log(l)) => {
            source_ok(&filters.sources, &l.source)
                && level_ok(filters.min_level, l.level)
                && text_ok(filters.text.as_deref(), &l.msg, l.target.as_deref())
        }
        (Table::Events, IngestFrame::Event(e)) => {
            source_ok(&filters.sources, &e.source)
                && kind_ok(&filters.event_kinds, &e.kind)
                && level_ok(filters.min_level, e.severity)
                && text_ok(filters.text.as_deref(), &e.kind, None)
        }
        (Table::Metrics, IngestFrame::Telemetry(t)) => metric_ok(&filters.metrics, &t.metric),
        (Table::Hw, IngestFrame::Hw(_)) => true,
        // A frame of a different table than the tail selected does not match.
        _ => false,
    }
}

fn source_ok(allowed: &[String], source: &str) -> bool {
    allowed.is_empty() || allowed.iter().any(|s| s == source)
}

fn metric_ok(allowed: &[String], metric: &str) -> bool {
    allowed.is_empty() || allowed.iter().any(|m| m == metric)
}

fn kind_ok(allowed: &[String], kind: &str) -> bool {
    allowed.is_empty() || allowed.iter().any(|k| k == kind)
}

fn level_ok(min: Option<Level>, level: Level) -> bool {
    match min {
        Some(floor) => level.as_u8() >= floor.as_u8(),
        None => true,
    }
}

fn text_ok(needle: Option<&str>, msg: &str, target: Option<&str>) -> bool {
    match needle {
        None => true,
        Some(n) => msg.contains(n) || target.is_some_and(|t| t.contains(n)),
    }
}

/// Serialize one matching frame into the JSON object an SSE `data:` line
/// carries. The shape mirrors the corresponding row type so a tail consumer and
/// a `query` consumer see the same fields. A frame that fails to serialize
/// yields `None` (it is simply skipped rather than killing the stream).
pub fn frame_to_json(frame: &IngestFrame) -> Option<serde_json::Value> {
    use serde_json::json;
    let value = match frame {
        IngestFrame::Log(l) => json!({
            "kind": "log",
            "ts_us": l.ts_us,
            "source": l.source,
            "level": level_name(l.level),
            "target": l.target,
            "msg": l.msg,
            "fields": serde_json::to_value(&l.fields).ok()?,
        }),
        IngestFrame::Event(e) => json!({
            "kind": "event",
            "ts_us": e.ts_us,
            "source": e.source,
            "event_kind": e.kind,
            "severity": level_name(e.severity),
            "detail": serde_json::to_value(&e.detail).ok()?,
        }),
        IngestFrame::Telemetry(t) => json!({
            "kind": "metric",
            "ts_us": t.ts_us,
            "metric": t.metric,
            "value": t.value,
            "tags": serde_json::to_value(&t.tags).ok()?,
        }),
        IngestFrame::Hw(h) => json!({
            "kind": "hw",
            "ts_us": h.ts_us,
            "signals": serde_json::to_value(&h.signals).ok()?,
        }),
    };
    Some(value)
}

fn level_name(level: Level) -> &'static str {
    match level {
        Level::Trace => "trace",
        Level::Debug => "debug",
        Level::Info => "info",
        Level::Warn => "warn",
        Level::Error => "error",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ados_protocol::logd::{EventFrame, HwSnapshot, LogFrame, TelemetryFrame};

    fn filters(query: &str) -> QueryFilters {
        QueryFilters::parse(&super::super::params::QueryParams::parse(query), 0).unwrap()
    }

    #[test]
    fn tail_slots_cap_and_free() {
        let slots = Arc::new(TailSlots::default());
        let mut guards = Vec::new();
        for _ in 0..MAX_TAIL_SUBSCRIBERS {
            guards.push(slots.try_acquire().expect("under the cap"));
        }
        assert_eq!(slots.active(), MAX_TAIL_SUBSCRIBERS);
        // The next one over the cap is refused.
        assert!(slots.try_acquire().is_none());
        // Dropping one frees a slot.
        guards.pop();
        assert_eq!(slots.active(), MAX_TAIL_SUBSCRIBERS - 1);
        assert!(slots.try_acquire().is_some());
    }

    #[test]
    fn export_slots_cap_and_free() {
        let slots = Arc::new(ExportSlots::default());
        let mut guards = Vec::new();
        for _ in 0..MAX_CONCURRENT_EXPORTS {
            guards.push(slots.try_acquire().expect("under the cap"));
        }
        assert_eq!(slots.active(), MAX_CONCURRENT_EXPORTS);
        // The next one over the cap is refused.
        assert!(slots.try_acquire().is_none());
        // Dropping one frees a slot.
        guards.pop();
        assert_eq!(slots.active(), MAX_CONCURRENT_EXPORTS - 1);
        assert!(slots.try_acquire().is_some());
    }

    #[test]
    fn log_filters_match_by_table_source_level_text() {
        let frame = {
            let mut l = LogFrame::new(1, "ados-video", Level::Warn, "encoder stalled");
            l.target = Some("video::encode".to_string());
            IngestFrame::Log(l)
        };
        assert!(frame_matches(&frame, &filters("kind=logs")));
        assert!(frame_matches(
            &frame,
            &filters("kind=logs&source=ados-video&level=info&text=stalled")
        ));
        // Wrong source.
        assert!(!frame_matches(&frame, &filters("kind=logs&source=api")));
        // Level floor above the frame.
        assert!(!frame_matches(&frame, &filters("kind=logs&level=error")));
        // Text not present.
        assert!(!frame_matches(&frame, &filters("kind=logs&text=nope")));
        // A log frame does not match an events tail.
        assert!(!frame_matches(&frame, &filters("kind=events")));
    }

    #[test]
    fn event_metric_and_hw_filters() {
        let evt = IngestFrame::Event(EventFrame::new(2, "radio.lock", "ados-radio", Level::Info));
        assert!(frame_matches(
            &evt,
            &filters("kind=events&event_kind=radio.lock")
        ));
        assert!(!frame_matches(
            &evt,
            &filters("kind=events&event_kind=radio.unlock")
        ));

        let m = IngestFrame::Telemetry(TelemetryFrame::new(3, "cpu.load", 0.5));
        assert!(frame_matches(&m, &filters("kind=metrics&metric=cpu.load")));
        assert!(!frame_matches(&m, &filters("kind=metrics&metric=mem.used")));

        let hw = IngestFrame::Hw(HwSnapshot::new(4));
        assert!(frame_matches(&hw, &filters("kind=hw")));
    }

    #[test]
    fn frame_to_json_carries_the_row_shape() {
        let mut l = LogFrame::new(10, "api", Level::Error, "boom");
        l.fields
            .insert("attempt".to_string(), rmpv::Value::from(2u64));
        let j = frame_to_json(&IngestFrame::Log(l)).unwrap();
        assert_eq!(j["kind"], "log");
        assert_eq!(j["level"], "error");
        assert_eq!(j["msg"], "boom");
        assert_eq!(j["fields"]["attempt"], 2);
    }
}
