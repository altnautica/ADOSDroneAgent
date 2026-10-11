//! logd query seam: HTTP-over-UDS reads of the store's metrics and events.

use super::*;

// ---------------------------------------------------------------------------
// logd query seam: HTTP-over-UDS reads of the store's /v1 metrics + events.
// ---------------------------------------------------------------------------

/// The newest value (as a JSON value) per named metric from a recent `metrics` page, newest-wins,
/// dropping any sample older than `max_age_s`. Returns `None` when the store is unreachable OR when
/// none of the named metrics has a FRESH sample in the page; a name not seen is simply absent from
/// a non-empty map. An empty result is `None`: the latency
/// route relies on that `None` to fall through to its live file read rather than reporting a
/// snapshot of nulls, or a frozen last-known reading, as a real measurement.
pub(super) async fn latest_metrics(
    state: &AppState,
    names: &[&str],
    max_age_s: f64,
) -> Option<Map<String, Value>> {
    let rows = state.logd.rows("metrics", 200, None).await?;
    // `out or None`: an empty map reads as "no data", matching the Python helper.
    collapse_empty_metrics(fresh_metric_values(&rows, names, now_micros(), max_age_s))
}

/// Epoch microseconds now, the unit the store stamps its rows in (`ts_us`). A
/// clock before the epoch reads as 0, which ages every row out rather than
/// inventing freshness.
pub(super) fn now_micros() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_micros() as i64)
        .unwrap_or(0)
}

/// Newest-wins value per named metric across a `metrics` page, keeping only
/// samples stamped within `max_age_s` of `now_us`. Pure over the rows so the
/// freshness rule is testable without a store.
///
/// Rows arrive newest-first (`ORDER BY ts_us DESC` in the store's query), so the
/// first sighting of a name is its newest sample and later ones are ignored.
///
/// A row whose `ts_us` is absent or not an integer is DROPPED, not kept: a sample
/// whose age cannot be established cannot be shown as current. That is the whole
/// point of the gate — the alternative is the frozen-number-as-live-reading this
/// route shipped with.
pub(super) fn fresh_metric_values(
    rows: &[Value],
    names: &[&str],
    now_us: i64,
    max_age_s: f64,
) -> Map<String, Value> {
    let max_age_us = (max_age_s * 1_000_000.0) as i64;
    let mut out: Map<String, Value> = Map::new();
    for row in rows {
        let Some(obj) = row.as_object() else { continue };
        let Some(metric) = obj.get("metric").and_then(Value::as_str) else {
            continue;
        };
        if !names.contains(&metric) || out.contains_key(metric) {
            continue;
        }
        let Some(ts_us) = obj.get("ts_us").and_then(Value::as_i64) else {
            continue;
        };
        // A row stamped in the future (a clock that stepped back after it was
        // written) is not stale; only a row too far in the past is.
        if now_us.saturating_sub(ts_us) > max_age_us {
            continue;
        }
        out.insert(
            metric.to_string(),
            obj.get("value").cloned().unwrap_or(Value::Null),
        );
    }
    out
}

/// Collapse an empty metric map to `None`, mirroring the Python helper's final
/// `return out or None`. A non-empty map passes through as `Some(map)`. The
/// latency route depends on the `None` so it falls through to its live file read
/// instead of reporting a snapshot of nulls as a real measurement.
pub(super) fn collapse_empty_metrics(out: Map<String, Value>) -> Option<Map<String, Value>> {
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

/// The newest numeric value for `name` from a merged metric map, or `None` if the name
/// is absent / non-numeric / a bool.
pub(super) fn metric_value(metrics: Option<&Map<String, Value>>, name: &str) -> Option<f64> {
    let value = metrics?.get(name)?;
    match value {
        // A JSON bool is not a `Number`; this rejects it the way the Python helper's
        // explicit `isinstance(value, bool)` check does.
        Value::Number(n) => n.as_f64(),
        _ => None,
    }
}

/// The `detail` object of the newest events row whose `kind` matches, or `None`.
pub(super) async fn latest_event_detail(
    state: &AppState,
    kind: &str,
) -> Option<Map<String, Value>> {
    let rows = state.logd.rows("events", 50, Some(kind)).await?;
    for row in rows {
        let Some(obj) = row.as_object() else { continue };
        if obj.get("kind").and_then(Value::as_str) == Some(kind) {
            return Some(match obj.get("detail") {
                Some(Value::Object(d)) => d.clone(),
                _ => Map::new(),
            });
        }
    }
    None
}

/// The value of `field` on the latest event of `kind`, or `None`. A convenience
/// over [`latest_event_detail`] for the latency route's `source` read.
pub(super) async fn latest_event_field(state: &AppState, kind: &str, field: &str) -> Option<Value> {
    let detail = latest_event_detail(state, kind).await?;
    detail.get(field).cloned()
}
