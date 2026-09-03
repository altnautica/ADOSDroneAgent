//! The atlas bus: the capture service publishes keyframes, poses, and capture
//! state on a single Unix-socket broadcast, every message wrapped in an
//! [`AtlasEvent`] tagged with its topic so a subscriber demultiplexes one
//! connection. Heavy keyframe images (JPEG) ride the same bus; the frame cap is
//! the plugin-envelope ceiling (4 MiB), generous for a compressed keyframe.
//!
//! The bus also carries the shared-data world-model descriptors
//! (`plugin.atlas.{pointcloud,occupancy,splat,mesh}`) back onto the drone once a
//! compute node has produced a generation, so an on-board consumer reads the
//! world model as data instead of the operator reading it as a picture.

use std::sync::atomic::{AtomicU64, Ordering};

use ados_protocol::atlas::{
    AtlasEvent, CaptureStatus, Generation, KeyframeEnvelope, MeshDescriptor, OccupancyDescriptor,
    PointCloudDescriptor, PoseDescriptor, SplatDescriptor, ATLAS_CAPTURE_STATE_TOPIC,
    ATLAS_KEYFRAME_TOPIC, PLUGIN_ATLAS_MESH_TOPIC, PLUGIN_ATLAS_OCCUPANCY_TOPIC,
    PLUGIN_ATLAS_POINTCLOUD_TOPIC, PLUGIN_ATLAS_POSE_TOPIC, PLUGIN_ATLAS_SPLAT_TOPIC,
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
    // Unstamped on the local publish bus; the drone-side forwarder stamps the
    // device id on egress (the single choke point every bearer passes through).
    let ev = AtlasEvent::new(topic, None, payload);
    let body = ev
        .encode()
        .map_err(|e| anyhow::anyhow!("encode atlas event: {e}"))?;
    encode_frame(&body, PLUGIN_MAX_FRAME)
        .map_err(|e: FrameError| anyhow::anyhow!("frame atlas event ({} bytes): {e}", body.len()))
}

/// Owns the atlas bus socket and publishes typed events onto it.
pub struct AtlasPublisher {
    bus: IpcBroadcast,
    /// Keyframes published with no subscriber connected — see
    /// [`AtlasPublisher::publish_keyframe`].
    undelivered_keyframes: AtomicU64,
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
        Ok(Self {
            bus,
            undelivered_keyframes: AtomicU64::new(0),
        })
    }

    async fn publish(&self, topic: &str, payload: Vec<u8>) {
        match encode_event_frame(topic, payload) {
            Ok(frame) => self.bus.broadcast(frame).await,
            Err(e) => tracing::warn!(topic, error = %e, "atlas_publish_encode_failed"),
        }
    }

    /// Publish a selected keyframe (drone-to-compute capture artifact).
    ///
    /// Returns the number of keyframes newly known to have reached NOBODY, for
    /// the caller to fold into the capture status. Selecting a keyframe is not
    /// the same as delivering one: with no subscriber attached the frame is
    /// encoded, published and discarded, so a keyframe count on its own claims
    /// reconstruction input that never existed. A slow subscriber the bus
    /// evicted is logged separately — an eviction loses an unknown number of
    /// already-queued frames, so it cannot honestly be turned into a count.
    pub async fn publish_keyframe(&self, kf: &KeyframeEnvelope) -> u64 {
        let body = match kf.to_msgpack() {
            Ok(body) => body,
            Err(e) => {
                tracing::warn!(error = %e, "atlas_keyframe_encode_failed");
                return 0;
            }
        };
        let evicted_before = self.bus.dropped_clients();
        let subscribers = self.bus.client_count().await;
        self.publish(ATLAS_KEYFRAME_TOPIC, body).await;
        let evicted = self.bus.dropped_clients() - evicted_before;
        if evicted > 0 {
            tracing::warn!(
                session_id = %kf.session_id,
                kf_id = kf.kf_id,
                evicted,
                "atlas_keyframe_subscriber_evicted"
            );
        }
        if subscribers == 0 {
            let n = self.undelivered_keyframes.fetch_add(1, Ordering::Relaxed) + 1;
            tracing::debug!(
                session_id = %kf.session_id,
                kf_id = kf.kf_id,
                undelivered = n,
                "atlas_keyframe_reached_no_subscriber"
            );
            return 1;
        }
        0
    }

    /// Total keyframes this publisher has emitted with no subscriber attached.
    pub fn undelivered_keyframes(&self) -> u64 {
        self.undelivered_keyframes.load(Ordering::Relaxed)
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

    /// Publish a world-model point-cloud descriptor as shared plugin data.
    pub async fn publish_pointcloud(&self, d: &PointCloudDescriptor) {
        self.publish_descriptor(PLUGIN_ATLAS_POINTCLOUD_TOPIC, d.to_msgpack(), d.generation)
            .await;
    }

    /// Publish a world-model occupancy / ESDF descriptor as shared plugin data.
    /// This is the planning input: a consumer reads the buffer the descriptor
    /// names and plans against it.
    pub async fn publish_occupancy(&self, d: &OccupancyDescriptor) {
        self.publish_descriptor(PLUGIN_ATLAS_OCCUPANCY_TOPIC, d.to_msgpack(), d.generation)
            .await;
    }

    /// Publish a world-model splat descriptor as shared plugin data.
    pub async fn publish_splat(&self, d: &SplatDescriptor) {
        self.publish_descriptor(PLUGIN_ATLAS_SPLAT_TOPIC, d.to_msgpack(), d.generation)
            .await;
    }

    /// Publish a world-model mesh descriptor as shared plugin data.
    pub async fn publish_mesh(&self, d: &MeshDescriptor) {
        self.publish_descriptor(PLUGIN_ATLAS_MESH_TOPIC, d.to_msgpack(), d.generation)
            .await;
    }

    async fn publish_descriptor(
        &self,
        topic: &'static str,
        encoded: Result<Vec<u8>, rmp_serde::encode::Error>,
        generation: Generation,
    ) {
        match encoded {
            Ok(body) => {
                tracing::debug!(topic, generation, "atlas_world_descriptor_published");
                self.publish(topic, body).await;
            }
            Err(e) => tracing::warn!(topic, error = %e, "atlas_world_descriptor_encode_failed"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ados_protocol::atlas::{CaptureState, PoseSource, VioHealth};

    #[test]
    fn event_frame_round_trips_with_topic_and_payload() {
        let status = CaptureStatus {
            session_id: "s".into(),
            state: CaptureState::Capturing,
            keyframes: 1,
            vio_health: VioHealth::Good,
            camera_count: 1,
            ingest_rate_hz: 9.0,
            capped: false,
            anchored: true,
            pose_tier: PoseSource::LocalVio,
            dropped_keyframes: 0,
        };
        let frame =
            encode_event_frame(ATLAS_CAPTURE_STATE_TOPIC, status.to_msgpack().unwrap()).unwrap();
        let len = u32::from_be_bytes(frame[..4].try_into().unwrap()) as usize;
        assert_eq!(len, frame.len() - 4);
        let ev = AtlasEvent::decode(&frame[4..]).unwrap();
        assert_eq!(ev.topic, ATLAS_CAPTURE_STATE_TOPIC);
        let back = CaptureStatus::from_msgpack(&ev.payload).unwrap();
        assert_eq!(back, status);
    }
}
