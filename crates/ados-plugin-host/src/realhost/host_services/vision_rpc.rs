//! Vision model, inference and detection host methods.

use super::*;

impl RealHost {
    pub(super) async fn serve_vision_register_model(
        &self,
        plugin_id: &str,
        args: &Value,
    ) -> Result<HostResult, HostError> {
        let Some(client) = self.vision.as_ref() else {
            return Ok(not_implemented("vision.register_model"));
        };
        // The engine (root) opens an engine-run model's file itself, so the
        // plugin may only name a model delivered to it, never an arbitrary path.
        let meta = ados_protocol::vision_rpc::decode_register_model(args)
            .map_err(|e| HostError::Rpc(e.to_string()))?;
        if meta.execution == ados_protocol::framebus::ModelExecution::EngineRun {
            if let Some(path) = meta.model_path.as_deref().filter(|p| !p.is_empty()) {
                if !self
                    .delivered_model_paths(plugin_id)
                    .iter()
                    .any(|p| p == path)
                {
                    return Err(HostError::Rpc(format!(
                        "model_path {path} is not a model delivered to this plugin"
                    )));
                }
            }
        }
        // The engine files the model under the plugin it is stamped with. A
        // transport / engine error surfaces as the response envelope `error` (a
        // soft failure), exactly like the engine's own reply error would.
        client
            .register_model(&ados_protocol::vision_rpc::with_owner(args, plugin_id))
            .await
            .map_err(|e| HostError::Rpc(e.0))
    }

    pub(super) async fn serve_vision_read_model(
        &self,
        plugin_id: &str,
        _args: &Value,
    ) -> Result<HostResult, HostError> {
        // The model-delivery last mile: return the CALLING plugin's resolved
        // model status off the install record, so it can find where its
        // delivered model was cached. Read per call so an operator sideload that
        // flips `needs_model`→`resolved` is picked up without a restart. Unlike
        // the other vision methods this touches no engine — a missing state file
        // or an unresolved plugin yields `{models: []}`, never an error.
        let installs = crate::state::load_state(Some(&self.state_path));
        let models = crate::state::find_install(&installs, plugin_id)
            .and_then(|i| i.model_status.clone())
            .unwrap_or_else(|| serde_json::Value::Array(Vec::new()));
        Ok(json_to_mpv(&serde_json::json!({ "models": models })))
    }

    pub(super) async fn serve_vision_infer(
        &self,
        plugin_id: &str,
        args: &Value,
    ) -> Result<HostResult, HostError> {
        let Some(client) = self.vision.as_ref() else {
            return Ok(not_implemented("vision.infer"));
        };
        client
            .infer(&ados_protocol::vision_rpc::with_owner(args, plugin_id))
            .await
            .map_err(|e| HostError::Rpc(e.0))
    }

    pub(super) async fn serve_vision_publish_detection(
        &self,
        plugin_id: &str,
        args: &Value,
        granted_caps: &std::collections::BTreeSet<String>,
    ) -> Result<HostResult, HostError> {
        let Some(client) = self.vision.as_ref() else {
            return Ok(not_implemented("vision.publish_detection"));
        };
        let mut batch = ados_protocol::vision_rpc::decode_publish_detection(args)
            .map_err(|e| HostError::Rpc(e.to_string()))?;
        vision_bind::bind_published_batch(
            &mut batch,
            plugin_id,
            granted_caps.contains("vision.track.designate"),
        );
        let args = ados_protocol::vision_rpc::publish_detection_args(&batch)
            .map_err(|e| HostError::Rpc(e.to_string()))?;
        client
            .publish_detection(&args)
            .await
            .map_err(|e| HostError::Rpc(e.0))
    }

    pub(super) async fn serve_vision_designate_track(
        &self,
        plugin_id: &str,
        args: &Value,
    ) -> Result<HostResult, HostError> {
        let Some(client) = self.vision.as_ref() else {
            return Ok(not_implemented("vision.designate_track"));
        };
        client
            .designate_track(&ados_protocol::vision_rpc::with_owner(args, plugin_id))
            .await
            .map_err(|e| HostError::Rpc(e.0))
    }
}
