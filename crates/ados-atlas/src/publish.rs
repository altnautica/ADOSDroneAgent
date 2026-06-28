//! The atlas bus: the capture service publishes keyframes, poses, and capture
//! state on a single Unix-socket broadcast, every message wrapped in an
//! [`AtlasEvent`] tagged with its topic so a subscriber demultiplexes one
//! connection. Heavy keyframe images (JPEG) ride the same bus; the frame cap is
//! the plugin-envelope ceiling (4 MiB), generous for a compressed keyframe.

use ados_protocol::atlas::{
    AtlasEvent, CaptureStatus, KeyframeEnvelope, PoseDescriptor, ATLAS_CAPTURE_STATE_TOPIC,
    ATLAS_KEYFRAME_TOPIC, PLUGIN_ATLAS_POSE_TOPIC,
};
use ados_protocol::frame::{encode_frame, FrameError, PLUGIN_MAX_FRAME};
use ados_protocol::ipc::IpcBroadcast;

/// Per-client outbound queue depth. Keyframes are large and arrive at the
/// selection rate (a few per second at most); 16 frames bounds memory while
/// giving a transient stall room before the slow subscriber is pruned.
const ATLAS_QUEUE_DEPTH: usize = 16;

/// Encode an [`AtlasEvent`] as a complete broadcast frame: a 4-byte big-endian
/// length prefix followed by the msgpack body.
pub fn encode_event_frame(topic: &str, payload: Vec<u8>) -> anyhow::Result<Vec<u8>> {
    let ev = AtlasEvent {
        topic: topic.to_string(),
        payload,
    };
    let body = ev
        .to_msgpack()
        .map_err(|e| anyhow::anyhow!("encode atlas event: {e}"))?;
    encode_frame(&body, PLUGIN_MAX_FRAME)
        .map_err(|e: FrameError| anyhow::anyhow!("frame atlas event ({} bytes): {e}", body.len()))
}

/// Owns the atlas bus socket and publishes typed events onto it.
pub struct AtlasPublisher {
    bus: IpcBroadcast,
}

impl AtlasPublisher {
    /// Bind the atlas bus at `socket_path`. `keep_last = false`: the bus mixes
    /// topics, so replaying only the single most-recent frame to a new
    /// subscriber would be misleading; subscribers receive events from the point
    /// they connect (capture state is re-published on every change).
    pub async fn bind(socket_path: &str) -> anyhow::Result<Self> {
        let (bus, _no_inbound) =
            IpcBroadcast::bind(socket_path, ATLAS_QUEUE_DEPTH, false, None).await?;
        tracing::info!(path = %socket_path, "atlas_bus_listening");
        Ok(Self { bus })
    }

    async fn publish(&self, topic: &str, payload: Vec<u8>) {
        match encode_event_frame(topic, payload) {
            Ok(frame) => self.bus.broadcast(frame).await,
            Err(e) => tracing::warn!(topic, error = %e, "atlas_publish_encode_failed"),
        }
    }

    /// Publish a selected keyframe (drone-to-compute capture artifact).
    pub async fn publish_keyframe(&self, kf: &KeyframeEnvelope) {
        match kf.to_msgpack() {
            Ok(body) => self.publish(ATLAS_KEYFRAME_TOPIC, body).await,
            Err(e) => tracing::warn!(error = %e, "atlas_keyframe_encode_failed"),
        }
    }

    /// Publish the live pose descriptor (~10 Hz shared-data pose).
    pub async fn publish_pose(&self, pose: &PoseDescriptor) {
        match pose.to_msgpack() {
            Ok(body) => self.publish(PLUGIN_ATLAS_POSE_TOPIC, body).await,
            Err(e) => tracing::warn!(error = %e, "atlas_pose_encode_failed"),
        }
    }

    /// Publish the capture-session state (on change). Also persists the slice to
    /// the plugin-state sidecar so the cloud heartbeat surfaces it under
    /// `pluginState.atlas` and the on-box state route serves it locally.
    pub async fn publish_capture_state(&self, status: &CaptureStatus) {
        crate::state_sidecar::write_atlas_state_sidecar(status);
        match status.to_msgpack() {
            Ok(body) => self.publish(ATLAS_CAPTURE_STATE_TOPIC, body).await,
            Err(e) => tracing::warn!(error = %e, "atlas_state_encode_failed"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ados_protocol::atlas::{CaptureState, VioHealth};

    #[test]
    fn event_frame_round_trips_with_topic_and_payload() {
        let status = CaptureStatus {
            session_id: "s".into(),
            state: CaptureState::Capturing,
            keyframes: 1,
            vio_health: VioHealth::Good,
            camera_count: 1,
            ingest_rate_hz: 9.0,
        };
        let frame =
            encode_event_frame(ATLAS_CAPTURE_STATE_TOPIC, status.to_msgpack().unwrap()).unwrap();
        let len = u32::from_be_bytes(frame[..4].try_into().unwrap()) as usize;
        assert_eq!(len, frame.len() - 4);
        let ev = AtlasEvent::from_msgpack(&frame[4..]).unwrap();
        assert_eq!(ev.topic, ATLAS_CAPTURE_STATE_TOPIC);
        let back = CaptureStatus::from_msgpack(&ev.payload).unwrap();
        assert_eq!(back, status);
    }
}
