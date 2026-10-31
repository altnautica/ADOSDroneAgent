//! WFB status view: the same store-first / sidecar-fallback read /api/wfb uses.

use crate::state::AppState;
use ados_protocol::paths;
use ados_protocol::wfb_status::WfbStatusConfig;
use serde_json::Map;
use serde_json::Value;

/// The WFB-ng link status the video gate + the radio block both read.
///
/// This daemon has no in-process WFB manager, so the status is read the same way
/// `/api/wfb` does: a fresh store row (the radio ships its full status body to
/// the durable store as a `link.wfb_status` event each heartbeat), else the
/// `/run/ados/wfb-stats.json` sidecar ([`crate::routes::wfb::choose_wfb_status`]).
/// Returns the finalized `/api/wfb` body as a map, or `None` when no source has
/// a usable body.
pub(crate) async fn wfb_status_view(state: &AppState) -> Option<Map<String, Value>> {
    let cfg = WfbStatusConfig::load(&state.pairing_paths.config);
    let store = latest_wfb_status(state).await;
    match crate::routes::wfb::choose_wfb_status(
        store,
        &cfg,
        &state.run_dir.join(paths::WFB_STATS_JSON),
        ados_protocol::wfb_status::now_unix_micros(),
    ) {
        Value::Object(map) if !map.is_empty() => Some(map),
        _ => None,
    }
}

/// The most-recent full wfb-status snapshot + its emit timestamp, or `None` when the
/// store is unreachable / holds no such event / the detail is empty.
pub(crate) async fn latest_wfb_status(state: &AppState) -> Option<(Map<String, Value>, i64)> {
    let rows = state
        .logd
        .rows("events", 1, Some("link.wfb_status"))
        .await?;
    let row = rows.first()?.as_object()?;
    let detail = row.get("detail")?.as_object()?;
    if detail.is_empty() {
        return None;
    }
    let ts_us = row
        .get("ts_us")
        .and_then(Value::as_f64)
        .map(|v| v as i64)
        .unwrap_or(0);
    Some((detail.clone(), ts_us))
}

// ---------------------------------------------------------------------------
// Radio block: the forward-compatible heartbeat radio block + camelCase remap.
// ---------------------------------------------------------------------------

/// Convert the snake_case radio block to the camelCase shape the GCS reads (the
/// LAN-direct poll has no heartbeat remapper, so the conversion happens here). Each
/// `a_b_c` key becomes `aBC`. `None` stays `None`.
pub(crate) fn radio_to_camel(block: Value) -> Value {
    let Value::Object(map) = block else {
        return Value::Null;
    };
    let mut out = Map::new();
    for (key, value) in map {
        out.insert(snake_to_camel(&key), value);
    }
    Value::Object(out)
}

/// `a_b_c` → `aBC`: the head segment lower-cased verbatim, each later segment
/// title-cased and concatenated.
pub(crate) fn snake_to_camel(key: &str) -> String {
    let mut parts = key.split('_');
    let head = parts.next().unwrap_or("");
    let mut out = String::with_capacity(key.len());
    out.push_str(head);
    for part in parts {
        out.push_str(&title_case(part));
    }
    out
}

/// Title-case a single segment, matching Python `str.title()` for an ASCII word:
/// first char upper, the rest lower. An empty segment stays empty.
pub(crate) fn title_case(segment: &str) -> String {
    let mut chars = segment.chars();
    match chars.next() {
        Some(first) => {
            let mut out = String::with_capacity(segment.len());
            out.extend(first.to_uppercase());
            out.push_str(&chars.as_str().to_lowercase());
            out
        }
        None => String::new(),
    }
}
