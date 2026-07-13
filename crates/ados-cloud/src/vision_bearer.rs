//! The cloud vision-detection publisher: tee offloaded detection batches to the
//! MQTT broker so a hosted / off-LAN GCS renders the same live boxes a LAN GCS
//! gets over the vision-detection WebSocket.
//!
//! An NPU-less drone offloads detection to a paired workstation node and
//! republishes the returned batches onto its local `vision.detection` bus; the
//! local WebSocket (`/api/vision/detections/ws`) forwards them to a LAN browser.
//! A remote drone reached only over the cloud relay has no LAN path to that bus,
//! so this publisher tees each returned batch onto `ados/{device_id}/vision/
//! detections` for the GCS to subscribe to. The batch is serialized as JSON in
//! the SAME shape the local WebSocket emits (the `DetectionBatch` named-map), so
//! the GCS parses one shape for both LAN and cloud.
//!
//! Local-first (Rule 39): a LAN-only agent never builds this publisher (the
//! reconciler wires no tee when cloud relay is off), and even when built, a batch
//! is dropped while the broker session is not confirmed up — no cloud round-trip
//! for a local drone. The publish is fire-and-forget at q0 (a lossy live stream,
//! like the MAVLink telemetry topic): a full outgoing queue or a down link drops
//! the batch rather than blocking the offload return path.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use ados_compute::DetectionTee;
use ados_protocol::framebus::DetectionBatch;

use crate::mqtt::topic_vision_detections;
use crate::mqtt::transport::{MqttQos, MqttTransport, RumqttcTransport};

/// Publishes returned detection batches to the cloud relay over the shared broker
/// connection, for a hosted / off-LAN GCS.
pub struct CloudDetectionPublisher {
    transport: Arc<dyn MqttTransport>,
    /// The transport's ConnAck-driven connectivity flag (shared handle): a batch
    /// is only published against a confirmed-up session.
    connected: Arc<AtomicBool>,
    /// The precomputed `ados/{device_id}/vision/detections` topic.
    topic: String,
}

impl CloudDetectionPublisher {
    /// Build a publisher over the daemon's broker connection. The connectivity
    /// flag is the transport's own `connected_handle`, so availability tracks the
    /// live session.
    pub fn new(device_id: &str, transport: Arc<RumqttcTransport>) -> Self {
        let connected = transport.connected_handle();
        Self {
            transport,
            connected,
            topic: topic_vision_detections(device_id),
        }
    }

    /// Test/dev: a publisher over an injected transport + an explicit connectivity
    /// flag, so a `FakeTransport` records the publishes with no broker.
    #[cfg(test)]
    fn with_transport(
        device_id: &str,
        transport: Arc<dyn MqttTransport>,
        connected: Arc<AtomicBool>,
    ) -> Self {
        Self {
            transport,
            connected,
            topic: topic_vision_detections(device_id),
        }
    }
}

impl DetectionTee for CloudDetectionPublisher {
    fn publish(&self, batch: &DetectionBatch) {
        // Rule 39: never a cloud round-trip against a down / unconfirmed session.
        // A LAN-only or disconnected agent drops the batch (not an error).
        if !self.connected.load(Ordering::Relaxed) {
            return;
        }
        // The SAME JSON shape the local detections WebSocket emits (the decoded
        // `DetectionBatch` named-map), so the GCS parses one shape for LAN + cloud.
        let body = match serde_json::to_vec(batch) {
            Ok(b) => b,
            Err(e) => {
                tracing::debug!(error = %e, "encode detection batch for the cloud lane");
                return;
            }
        };
        // Fire-and-forget at q0: a full outgoing queue / down link drops the batch
        // rather than stalling the offload return path (recency beats completeness
        // for a live stream).
        if let Err(e) = self
            .transport
            .try_publish(&self.topic, MqttQos::AtMostOnce, body)
        {
            tracing::trace!(error = %e, "dropped a cloud detection batch (publisher busy)");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mqtt::transport::test_support::FakeTransport;
    use ados_protocol::framebus::{
        BoundingBox, Detection, DetectionBatch, VISION_DETECTION_VERSION,
    };

    /// A batch shaped exactly like one the offload return bridge produces (a
    /// pixel-space box with a track id, tagged with the source frame's size).
    fn sample_batch() -> DetectionBatch {
        DetectionBatch {
            v: VISION_DETECTION_VERSION,
            model_id: "offload".into(),
            camera_id: "front".into(),
            frame_id: 7,
            ts_ms: 1_700_000_000_000,
            frame_width: 1280,
            frame_height: 720,
            detections: vec![Detection {
                bbox: BoundingBox {
                    x: 320.0,
                    y: 180.0,
                    width: 640.0,
                    height: 360.0,
                },
                class_label: "person".into(),
                confidence: 0.8,
                track_id: Some(3),
                assoc_confidence: None,
                lock_state: None,
                attributes: None,
            }],
        }
    }

    #[test]
    fn a_connected_publisher_tees_the_batch_as_json_on_the_vision_topic() {
        let fake = Arc::new(FakeTransport::default());
        let connected = Arc::new(AtomicBool::new(true));
        let publisher = CloudDetectionPublisher::with_transport(
            "dev1",
            fake.clone() as Arc<dyn MqttTransport>,
            connected,
        );

        publisher.publish(&sample_batch());

        let pubs = fake.publishes.lock().unwrap();
        assert_eq!(pubs.len(), 1);
        let (topic, qos, payload) = &pubs[0];
        // The exact topic the GCS subscribes to, at q0 (lossy live stream).
        assert_eq!(topic, "ados/dev1/vision/detections");
        assert_eq!(*qos, MqttQos::AtMostOnce);

        // The payload is JSON in the DetectionBatch shape the local WebSocket
        // emits: same field names, so the GCS parses one shape for LAN + cloud.
        let json: serde_json::Value = serde_json::from_slice(payload).unwrap();
        assert_eq!(json["v"], VISION_DETECTION_VERSION);
        assert_eq!(json["model_id"], "offload");
        assert_eq!(json["camera_id"], "front");
        assert_eq!(json["frame_id"], 7);
        assert_eq!(json["frame_width"], 1280);
        assert_eq!(json["frame_height"], 720);
        let det = &json["detections"][0];
        assert_eq!(det["class_label"], "person");
        assert_eq!(det["confidence"], 0.8);
        assert_eq!(det["track_id"], 3);
        assert_eq!(det["bbox"]["x"], 320.0);
        assert_eq!(det["bbox"]["width"], 640.0);

        // And it decodes straight back to the identical typed batch (full parity
        // with the type the local WebSocket serializes).
        let back: DetectionBatch = serde_json::from_slice(payload).unwrap();
        assert_eq!(back, sample_batch());
    }

    #[test]
    fn a_disconnected_publisher_tees_nothing() {
        let fake = Arc::new(FakeTransport::default());
        let connected = Arc::new(AtomicBool::new(false));
        let publisher = CloudDetectionPublisher::with_transport(
            "dev1",
            fake.clone() as Arc<dyn MqttTransport>,
            connected,
        );

        // Local-first (Rule 39): a down session takes no cloud round-trip.
        publisher.publish(&sample_batch());
        assert!(fake.publishes.lock().unwrap().is_empty());
    }
}
