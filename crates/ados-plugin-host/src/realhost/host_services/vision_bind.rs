//! Vision helpers for plugin calls: binding a published detection batch to its
//! publisher, and the model files delivered to a plugin.

use super::super::RealHost;

/// Bind a plugin-published detection batch to its publisher before it reaches
/// the shared bus: the `model_id` moves under the plugin's namespace, so a
/// consumer never mistakes it for the engine's own detector or another
/// plugin's, and unless the plugin may designate tracks (`may_lock`) every
/// detection's `lock_state` and `track_id` are cleared. A follow consumer only
/// engages a locked track, and only the operator, or a plugin granted
/// designation, may create one.
pub(crate) fn bind_published_batch(
    batch: &mut ados_protocol::framebus::DetectionBatch,
    plugin_id: &str,
    may_lock: bool,
) {
    batch.model_id = ados_protocol::vision_rpc::owned_model_id(plugin_id, &batch.model_id);
    if !may_lock {
        for detection in &mut batch.detections {
            detection.lock_state = None;
            detection.track_id = None;
        }
    }
}

impl RealHost {
    /// The model files delivered to `plugin_id`: the `path` of every resolved
    /// entry in its install record's model status. Read per call, like
    /// `vision.read_model`, so a fresh delivery is seen without a restart.
    pub(super) fn delivered_model_paths(&self, plugin_id: &str) -> Vec<String> {
        let installs = crate::state::load_state(Some(&self.state_path));
        let Some(serde_json::Value::Array(entries)) =
            crate::state::find_install(&installs, plugin_id).and_then(|i| i.model_status.clone())
        else {
            return Vec::new();
        };
        entries
            .iter()
            .filter(|e| e.get("state").and_then(serde_json::Value::as_str) == Some("resolved"))
            .filter_map(|e| e.get("path").and_then(serde_json::Value::as_str))
            .filter(|p| !p.is_empty())
            .map(str::to_string)
            .collect()
    }
}
