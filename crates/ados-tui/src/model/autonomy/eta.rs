//! The next waypoint's ETA from the nav `ar` kind.

use serde_json::Value;

use super::super::flight_state::AR_ETA_MAX_AGE_MS;
use super::f;

/// The ETA of the next waypoint in an `ar` frame `age_ms` old, from whichever
/// camera carries it; `None` once the frame is older than
/// [`AR_ETA_MAX_AGE_MS`] or names no next waypoint.
pub(super) fn next_waypoint_eta_s(data: &Value, age_ms: Option<u64>) -> Option<f64> {
    if age_ms.is_none_or(|a| a > AR_ETA_MAX_AGE_MS) {
        return None;
    }
    data.get("cameras")?
        .as_object()?
        .values()
        .filter_map(|c| c.get("items").and_then(Value::as_array))
        .flatten()
        .find(|it| {
            it.get("t").and_then(Value::as_str) == Some("wp")
                && it.get("role").and_then(Value::as_str) == Some("next")
                && f(it, "eta_s").is_some()
        })
        .and_then(|it| f(it, "eta_s"))
}
