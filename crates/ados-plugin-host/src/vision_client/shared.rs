//! Push arming and request routing over the shared connection state.

use super::*;

impl Push {
    pub(super) fn method(self) -> &'static str {
        match self {
            Self::Frames => methods::SUBSCRIBE_FRAMES,
            Self::Detections => methods::SUBSCRIBE_DETECTIONS,
        }
    }

    pub(super) fn capability(self) -> &'static str {
        match self {
            Self::Frames => "vision.frame.read",
            Self::Detections => "vision.detection.subscribe",
        }
    }

    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Frames => "frame",
            Self::Detections => "detection",
        }
    }
}

impl Shared {
    pub(super) fn push_state(&self, push: Push) -> &Mutex<PushState> {
        match push {
            Push::Frames => &self.frame_push,
            Push::Detections => &self.detection_push,
        }
    }

    /// A plugin subscribed: record the want and arm the current connection.
    pub(super) async fn arm(&self, push: Push) {
        let mut state = self.push_state(push).lock().await;
        state.wanted = true;
        self.arm_locked(push, &mut state).await;
    }

    /// A new connection is up: re-arm every push a plugin has asked for.
    pub(super) async fn rearm(&self, push: Push) {
        let mut state = self.push_state(push).lock().await;
        if state.wanted {
            self.arm_locked(push, &mut state).await;
        }
    }

    pub(super) async fn arm_locked(&self, push: Push, state: &mut PushState) {
        if state.armed {
            return;
        }
        if !self.connected.load(Ordering::Acquire) {
            tracing::debug!(
                push = push.label(),
                "vision engine not connected; push arms when it connects"
            );
            return;
        }
        match self
            .request(push.method(), push.capability(), &Value::Map(vec![]))
            .await
        {
            Ok(_) => {
                state.armed = true;
                tracing::info!(
                    push = push.label(),
                    "vision push armed on the engine connection"
                );
            }
            Err(e) => {
                tracing::warn!(push = push.label(), error = %e, "vision push could not be armed");
            }
        }
    }

    /// Install or clear the connection. Holds both push locks (lock order:
    /// push states, then `request`) so no arm can straddle the swap, and resets
    /// `armed`: the engine's push subscription is per connection.
    pub(super) async fn set_connection(&self, conn: Option<RequestChannel>) {
        let mut frames = self.frame_push.lock().await;
        let mut detections = self.detection_push.lock().await;
        let mut request = self.request.lock().await;
        self.connected.store(conn.is_some(), Ordering::Release);
        *request = conn;
        frames.armed = false;
        detections.armed = false;
    }

    /// Write one request envelope toward the engine and await the response. The
    /// connection mutex serializes the write+await so the engine's in-order
    /// responses match the requests. No connection, a transport failure, or an
    /// engine `error` becomes a [`VisionRpcError`] the host surfaces to the
    /// plugin verbatim.
    pub(super) async fn request(
        &self,
        method: &str,
        capability: &str,
        args: &Value,
    ) -> Result<Value, VisionRpcError> {
        let env = Envelope {
            version: PROTOCOL_VERSION,
            kind: "request".to_string(),
            method: method.to_string(),
            capability: capability.to_string(),
            args: args.clone(),
            request_id: format!("vis-{}", now_ms()),
            token: String::new(),
            error: None,
        };
        let frame = env
            .encode_frame()
            .map_err(|e| VisionRpcError(format!("encode failed: {e}")))?;

        let mut guard = self.request.lock().await;
        let Some(chan) = guard.as_mut() else {
            return Err(VisionRpcError(VISION_ENGINE_UNAVAILABLE.to_string()));
        };
        chan.write_half
            .write_all(&frame)
            .await
            .map_err(|e| VisionRpcError(format!("vision engine unavailable: {e}")))?;
        chan.write_half
            .flush()
            .await
            .map_err(|e| VisionRpcError(format!("vision engine unavailable: {e}")))?;
        match chan.responses.recv().await {
            Some(Ok(args)) => Ok(args),
            Some(Err(msg)) => Err(VisionRpcError(msg)),
            None => Err(VisionRpcError("vision engine closed".to_string())),
        }
    }
}
