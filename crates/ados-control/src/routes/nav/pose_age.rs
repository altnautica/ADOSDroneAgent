//! `pose_age_ms` on the `nav` kind.
//!
//! vision-nav reports how old a pose is when its `ODOMETRY` goes out (the
//! median over its last poses, `poseAgeMs`) on its `navigation` telemetry
//! channel, not on `nav.state`. The plugin host mirrors that channel into the
//! plugin's state sidecar as `telemetry.navigation`; the `nav` value the hub
//! serves carries it as `pose_age_ms`, so every operator surface reads it from
//! the nav state.

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Map, Value};

use super::hub::VISION_NAV_ID;
use crate::routes::plugins_state::read_plugin_sidecar;

/// The sidecar entry vision-nav's `navigation` telemetry channel lands in.
const NAVIGATION_CHANNEL: &str = "telemetry.navigation";

/// How old the channel's last report may be before its pose age is dropped:
/// the plugin-state window.
const MAX_AGE_MS: i64 = ados_protocol::paths::PLUGIN_STATE_MAX_AGE_MS as i64;

/// The pose age (ms, one decimal) vision-nav last reported in its sidecar
/// `doc`, when that report is no older than [`MAX_AGE_MS`] at `now_ms`.
/// `None` before the first pose, when the channel is absent or stale, or when
/// the value is not a finite non-negative number.
pub(crate) fn pose_age_from_sidecar(doc: &Map<String, Value>, now_ms: i64) -> Option<f64> {
    let entry = doc.get(NAVIGATION_CHANNEL)?;
    let age = now_ms.checked_sub(entry.get("ts_ms")?.as_i64()?)?;
    if !(0..=MAX_AGE_MS).contains(&age) {
        return None;
    }
    entry
        .get("payload")?
        .get("poseAgeMs")?
        .as_f64()
        .filter(|v| v.is_finite() && *v >= 0.0)
        .map(|v| (v * 10.0).round() / 10.0)
}

/// `payload` with `pose_age_ms` set (`null` when unknown). A payload that
/// already carries the key keeps its own value.
pub(crate) fn with_pose_age(mut payload: Value, pose_age_ms: Option<f64>) -> Value {
    if let Value::Object(o) = &mut payload {
        o.entry("pose_age_ms")
            .or_insert_with(|| pose_age_ms.map_or(Value::Null, Value::from));
    }
    payload
}

/// A vision-nav `nav.state` payload with the pose age from its state sidecar
/// in `sidecar_dir`, read at `now`.
pub(crate) fn enrich_nav_state(payload: Value, sidecar_dir: &Path, now: SystemTime) -> Value {
    let now_ms = now
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX));
    let pose_age = read_plugin_sidecar(sidecar_dir, VISION_NAV_ID, now)
        .and_then(|doc| pose_age_from_sidecar(&doc, now_ms));
    with_pose_age(payload, pose_age)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn doc(v: Value) -> Map<String, Value> {
        v.as_object().unwrap().clone()
    }

    #[test]
    fn reads_a_fresh_navigation_report() {
        // An f32 median widened to f64 on the way through msgpack.
        let d = doc(json!({"telemetry.navigation": {"ts_ms": 10_000,
            "payload": {"poseAgeMs": f64::from(118.4_f32)}}}));
        assert_eq!(pose_age_from_sidecar(&d, 10_500), Some(118.4));
        assert_eq!(pose_age_from_sidecar(&d, 10_000 + MAX_AGE_MS), Some(118.4));
        assert_eq!(
            pose_age_from_sidecar(&d, 10_001 + MAX_AGE_MS),
            None,
            "stale"
        );
        assert_eq!(pose_age_from_sidecar(&d, 9_999), None, "future-dated");
    }

    #[test]
    fn absent_or_null_is_none() {
        for d in [
            json!({}),
            json!({"telemetry.navigation": {"ts_ms": 1, "payload": {"poseAgeMs": null}}}),
            json!({"telemetry.navigation": {"ts_ms": 1, "payload": {}}}),
            json!({"telemetry.navigation": {"payload": {"poseAgeMs": 5.0}}}),
            json!({"nav.state": {"ts_ms": 1, "payload": {"poseAgeMs": 5.0}}}),
        ] {
            assert_eq!(pose_age_from_sidecar(&doc(d.clone()), 1), None, "{d}");
        }
    }

    #[test]
    fn the_nav_payload_gains_the_key_and_keeps_its_own() {
        let p = with_pose_age(json!({"mode": "external_nav"}), Some(42.0));
        assert_eq!(p, json!({"mode": "external_nav", "pose_age_ms": 42.0}));
        let p = with_pose_age(json!({"mode": "flow"}), None);
        assert_eq!(p["pose_age_ms"], Value::Null);
        assert!(p.as_object().unwrap().contains_key("pose_age_ms"));
        let p = with_pose_age(json!({"pose_age_ms": 7.0}), Some(42.0));
        assert_eq!(p["pose_age_ms"], 7.0);
    }

    #[test]
    fn enriches_from_the_sidecar_file() {
        let dir = tempfile::tempdir().unwrap();
        let written_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;
        let body = json!({"telemetry.navigation": {"ts_ms": written_ms - 200,
            "payload": {"poseAgeMs": 96.0}}});
        std::fs::write(
            dir.path().join(format!("{VISION_NAV_ID}-state.json")),
            body.to_string(),
        )
        .unwrap();
        // After the write: the sidecar's mtime must not be in the future.
        let now = SystemTime::now();
        let p = enrich_nav_state(json!({"mode": "external_nav"}), dir.path(), now);
        assert_eq!(p["pose_age_ms"], 96.0);
        let empty = tempfile::tempdir().unwrap();
        let p = enrich_nav_state(json!({"mode": "external_nav"}), empty.path(), now);
        assert_eq!(p["pose_age_ms"], Value::Null);
    }
}
