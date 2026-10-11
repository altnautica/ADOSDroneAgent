//! Vision subscription, model, inference and detection calls.

use super::*;

impl PluginIpcClient {
    /// Register a callback for `vision.deliver` frame pushes, keyed by the
    /// camera id the subscriber filtered on (`None` ⇒ every camera, the
    /// wildcard key). The reader loop decodes each delivered descriptor and
    /// invokes the callbacks whose key is the descriptor's `camera_id` or the
    /// wildcard, passing the envelope `args` map (which carries the
    /// `descriptor` binary). Unlike events, `vision.deliver` carries no `topic`,
    /// so it routes on the decoded descriptor's camera id, not the event map.
    pub fn register_vision_callback(&self, camera_id: Option<&str>, callback: EventCallback) {
        let key = camera_id.unwrap_or(VISION_ANY_CAMERA);
        register_callback(&self.vision_callbacks, key, callback);
    }

    /// Register a callback for `vision.deliver_detection` pushes.
    ///
    /// `camera_id` of `None` receives every camera's batches. The engine fans
    /// all cameras out on one stream, so the filter is applied here rather than
    /// at the engine — the same posture the Python client takes.
    pub fn register_detection_callback(&self, camera_id: Option<&str>, callback: EventCallback) {
        let key = camera_id.unwrap_or(VISION_ANY_CAMERA);
        register_callback(&self.detection_callbacks, key, callback);
    }
}

impl PluginIpcClient {
    /// Arm the host's detection-batch push stream for this connection.
    pub async fn vision_subscribe_detections(
        &self,
        camera_id: Option<&str>,
    ) -> Result<Value, ClientError> {
        let args = Value::Map(vec![(
            Value::from("camera_id"),
            Value::from(camera_id.unwrap_or("")),
        )]);
        Ok(self
            .send_request(
                ados_protocol::framebus::methods::SUBSCRIBE_DETECTIONS,
                "vision.detection.subscribe",
                args,
            )
            .await?
            .args)
    }

    /// Designate the tracker's locked target, overriding its auto-lock. Only
    /// the target's box, label and confidence cross; the box is required. The
    /// required cap is `vision.track.designate`.
    pub async fn vision_designate_track(
        &self,
        camera_id: &str,
        target: &ados_protocol::framebus::Detection,
    ) -> Result<Value, ClientError> {
        let args =
            vision_rpc::designate_track_args(camera_id, target).map_err(vision_args_error)?;
        Ok(self
            .send_request(
                ados_protocol::framebus::methods::DESIGNATE_TRACK,
                "vision.track.designate",
                args,
            )
            .await?
            .args)
    }

    /// Read this plugin's resolved model-delivery status. Sends
    /// [`framebus::methods::READ_MODEL`] (gated on `vision.model.read`); the host
    /// returns `{models: [ {state, model_id, runtime, path, reason}, ... ]}` off
    /// this plugin's install record. No args.
    pub async fn vision_read_model(&self) -> Result<Value, ClientError> {
        Ok(self
            .send_request(
                ados_protocol::framebus::methods::READ_MODEL,
                "vision.model.read",
                Value::Map(vec![]),
            )
            .await?
            .args)
    }
}

impl PluginIpcClient {
    /// Subscribe to vision frame descriptors. The host starts (or widens) the
    /// engine's frame stream toward this plugin and then delivers descriptors
    /// as `vision.deliver` events. `camera_id` of `None` requests every
    /// camera's frames; `Some(id)` narrows the stream at the engine. The
    /// required cap is `vision.frame.read`. The caller registers the matching
    /// frame callback via [`register_vision_callback`](Self::register_vision_callback);
    /// `vision.deliver` pushes carry the descriptor directly (no topic), so they
    /// do not ride the `event.subscribe` path.
    pub async fn vision_subscribe_frames(
        &self,
        camera_id: Option<&str>,
    ) -> Result<Value, ClientError> {
        let mut entries = Vec::new();
        if let Some(id) = camera_id {
            entries.push((Value::from("camera_id"), Value::from(id)));
        }
        Ok(self
            .send_request(
                ados_protocol::framebus::methods::SUBSCRIBE_FRAMES,
                "vision.frame.read",
                Value::Map(entries),
            )
            .await?
            .args)
    }

    /// Register an inference model. The required cap is
    /// `vision.model.register`.
    pub async fn vision_register_model(
        &self,
        model: &ados_protocol::framebus::ModelMetadata,
    ) -> Result<Value, ClientError> {
        let args = vision_rpc::register_model_args(model).map_err(vision_args_error)?;
        Ok(self
            .send_request(
                ados_protocol::framebus::methods::REGISTER_MODEL,
                "vision.model.register",
                args,
            )
            .await?
            .args)
    }

    /// Run a registered model against one frame, named by the descriptor the
    /// engine published for it, on the shared backend and return the engine's
    /// reply (decode it with [`vision_rpc::decode_infer_reply`]). The required
    /// cap is `vision.model.register`.
    pub async fn vision_infer(
        &self,
        model_id: &str,
        frame: &ados_protocol::framebus::FrameDescriptor,
    ) -> Result<Value, ClientError> {
        let args = vision_rpc::infer_args(model_id, frame).map_err(vision_args_error)?;
        Ok(self
            .send_request(
                ados_protocol::framebus::methods::INFER,
                "vision.model.register",
                args,
            )
            .await?
            .args)
    }

    /// Publish a detection batch on `vision.detection`. The required cap is
    /// `vision.detection.publish`.
    pub async fn vision_publish_detection(
        &self,
        batch: &ados_protocol::framebus::DetectionBatch,
    ) -> Result<Value, ClientError> {
        let args = vision_rpc::publish_detection_args(batch).map_err(vision_args_error)?;
        Ok(self
            .send_request(
                ados_protocol::framebus::methods::PUBLISH_DETECTION,
                "vision.detection.publish",
                args,
            )
            .await?
            .args)
    }
}
