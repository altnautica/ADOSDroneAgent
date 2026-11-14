//! The cameras USB recovery watches, read off the `camera-state.json` sidecar.
//!
//! The video pipeline publishes each local camera's own presence under
//! `cameras` (the primary, and every captured or secondary camera it does not
//! discover itself). A sidecar from a pipeline that predates that list carries
//! only the primary's discovery state, which is read as the one camera.

use serde_json::Value;

/// Treat a camera-state snapshot older than this as unknown (do not act).
pub(crate) const STATE_FRESHNESS_S: u64 = 120;

/// One camera recovery can act on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Target {
    /// The roster id (`main` for a single-camera node).
    pub(crate) id: String,
    /// The transport primary: its location record is the node's original
    /// last-good file, so `auto` expectation survives an upgrade.
    pub(crate) primary: bool,
    /// `ready` | `missing` | `error`.
    pub(crate) state: String,
    /// The V4L2 node (or the configured locator while it is gone).
    pub(crate) device_path: Option<String>,
}

/// Whether the sidecar was stamped within [`STATE_FRESHNESS_S`] of `now_unix`.
pub(crate) fn fresh(doc: &Value, now_unix: u64) -> bool {
    let updated = doc
        .get("updated_at_unix")
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    updated > 0.0 && now_unix.saturating_sub(updated as u64) <= STATE_FRESHNESS_S
}

/// The cameras in a camera-state document, in the order the pipeline lists
/// them. Entries without an id or a state are skipped.
pub(crate) fn targets_from(doc: &Value) -> Vec<Target> {
    let text = |v: &Value, key: &str| {
        v.get(key)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    if let Some(list) = doc.get("cameras").and_then(Value::as_array) {
        return list
            .iter()
            .filter_map(|c| {
                Some(Target {
                    id: text(c, "id")?,
                    primary: c.get("primary").and_then(Value::as_bool).unwrap_or(false),
                    state: text(c, "state")?,
                    device_path: text(c, "device_path"),
                })
            })
            .collect();
    }
    let Some(state) = text(doc, "state") else {
        return Vec::new();
    };
    vec![Target {
        id: "main".to_string(),
        primary: true,
        state,
        device_path: text(doc, "primary_path"),
    }]
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn every_listed_camera_is_a_target() {
        let doc = json!({
            "state": "ready",
            "primary_path": "framebus://front",
            "cameras": [
                {"id": "front", "primary": true, "captured": true, "state": "missing",
                 "device_path": "/dev/video0", "reason": "camera device /dev/video0 is gone"},
                {"id": "belly", "primary": false, "state": "ready", "device_path": "/dev/video2"},
                {"id": "broken"}
            ]
        });
        let t = targets_from(&doc);
        assert_eq!(t.len(), 2);
        assert_eq!(t[0].id, "front");
        assert!(t[0].primary);
        assert_eq!(t[0].state, "missing");
        assert_eq!(t[0].device_path.as_deref(), Some("/dev/video0"));
        assert_eq!(t[1].id, "belly");
        assert!(!t[1].primary);
    }

    #[test]
    fn a_sidecar_without_the_list_is_its_primary() {
        let doc = json!({"state": "ready", "primary_path": "/dev/video0"});
        assert_eq!(
            targets_from(&doc),
            vec![Target {
                id: "main".into(),
                primary: true,
                state: "ready".into(),
                device_path: Some("/dev/video0".into()),
            }]
        );
        let missing = json!({"state": "missing", "primary_path": null});
        assert_eq!(targets_from(&missing)[0].device_path, None);
        assert!(targets_from(&json!({})).is_empty());
    }

    #[test]
    fn freshness_follows_the_stamp() {
        assert!(fresh(&json!({"updated_at_unix": 1000.0}), 1100));
        assert!(!fresh(&json!({"updated_at_unix": 1000.0}), 1121));
        assert!(!fresh(&json!({}), 1000));
    }
}
