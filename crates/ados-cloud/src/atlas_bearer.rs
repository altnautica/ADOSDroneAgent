//! The cloud Atlas bearer: publish Atlas events to the MQTT broker for off-LAN
//! reach (the last rung of the bearer ladder, opt-in cloud mode).
//!
//! It lives here, not in `ados-atlas-transport`, on purpose: this crate already
//! owns the single broker connection (`RumqttcTransport` — one TLS WSS session
//! authenticated as `ados-{device_id}` with the pairing key), so the bearer
//! reuses it rather than opening a second session, which would collide on the
//! client id and get the older session kicked. The lean transport crate keeps
//! its single `ados-protocol` dependency; the layering points the heavy cloud
//! crate at the light lane crate, never the reverse.
//!
//! Honest liveness (Rule 37 / DEC-170 family): `is_available()` reads the real
//! ConnAck-driven `connected` flag, not "the task exists" — rumqttc retries a
//! down broker forever and a publish is fire-and-forget, so an optimistic
//! available-always bearer would silently swallow events on a wedged broker. A
//! down link reports unavailable so the ladder never reaches the cloud rung when
//! it cannot deliver, and a LAN-only agent never even builds this bearer.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use ados_atlas_transport::{AtlasBearer, AtlasEvent, BearerKind, TransportError};

use crate::mqtt::topic_atlas;
use crate::mqtt::transport::{MqttQos, MqttTransport, RumqttcTransport};

/// The live high-rate pose stream rides at-most-once (recency beats completeness,
/// like telemetry); every discrete artifact (keyframe / splat / mesh / occupancy
/// / capture state / offloaded pose) rides at-least-once so it is not silently
/// lost off a lossy link.
fn atlas_qos(event_topic: &str) -> MqttQos {
    if event_topic == "plugin.atlas.pose" {
        MqttQos::AtMostOnce
    } else {
        MqttQos::AtLeastOnce
    }
}

/// Publishes framed Atlas events over the shared broker connection.
pub struct CloudBearer {
    device_id: String,
    transport: Arc<dyn MqttTransport>,
    /// The transport's ConnAck-driven connectivity flag (shared handle).
    connected: Arc<AtomicBool>,
}

impl CloudBearer {
    /// Build a cloud bearer over the daemon's existing broker connection. The
    /// connectivity flag is the transport's own `connected_handle`, so the
    /// bearer's availability tracks the live session.
    pub fn new(device_id: impl Into<String>, transport: Arc<RumqttcTransport>) -> Self {
        let connected = transport.connected_handle();
        Self {
            device_id: device_id.into(),
            transport,
            connected,
        }
    }

    /// Test/dev: a bearer over an injected transport + an explicit connectivity
    /// flag, so a `FakeTransport` records the publishes with no broker.
    pub fn with_transport(
        device_id: impl Into<String>,
        transport: Arc<dyn MqttTransport>,
        connected: Arc<AtomicBool>,
    ) -> Self {
        Self {
            device_id: device_id.into(),
            transport,
            connected,
        }
    }
}

#[async_trait::async_trait]
impl AtlasBearer for CloudBearer {
    fn kind(&self) -> BearerKind {
        BearerKind::Cloud
    }

    async fn is_available(&self) -> bool {
        self.connected.load(Ordering::Relaxed)
    }

    async fn send(&self, event: &AtlasEvent) -> Result<(), TransportError> {
        // Honest gate: never claim a send against a disconnected broker.
        if !self.connected.load(Ordering::Relaxed) {
            return Err(TransportError::Unavailable);
        }
        // Publish the WHOLE envelope (event.to_msgpack()), the identical wire the
        // LAN receiver (atlas_event_router) decodes, so the off-LAN consumer uses
        // the same decode path. Never publish event.payload alone.
        let body = event
            .to_msgpack()
            .map_err(|e| TransportError::Encode(e.to_string()))?;
        let topic = topic_atlas(&self.device_id, &event.topic);
        let qos = atlas_qos(&event.topic);
        self.transport
            .publish(&topic, qos, body)
            .await
            .map_err(|e| TransportError::Request(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mqtt::transport::test_support::FakeTransport;

    fn event(topic: &str, payload: Vec<u8>) -> AtlasEvent {
        AtlasEvent {
            topic: topic.to_string(),
            payload,
        }
    }

    #[test]
    fn topic_atlas_drops_the_prefix_and_slashes_the_leaf() {
        assert_eq!(topic_atlas("d", "atlas.keyframe"), "ados/d/atlas/keyframe");
        assert_eq!(
            topic_atlas("d", "atlas.pose.offload"),
            "ados/d/atlas/pose/offload"
        );
        assert_eq!(topic_atlas("d", "plugin.atlas.pose"), "ados/d/atlas/pose");
    }

    #[test]
    fn qos_is_q0_for_the_live_pose_and_q1_for_artifacts() {
        assert_eq!(atlas_qos("plugin.atlas.pose"), MqttQos::AtMostOnce);
        assert_eq!(atlas_qos("atlas.keyframe"), MqttQos::AtLeastOnce);
        assert_eq!(atlas_qos("plugin.atlas.splat"), MqttQos::AtLeastOnce);
    }

    #[tokio::test]
    async fn a_keyframe_publishes_the_envelope_at_q1_on_the_atlas_topic() {
        let fake = Arc::new(FakeTransport::default());
        let connected = Arc::new(AtomicBool::new(true));
        let bearer =
            CloudBearer::with_transport("dev1", fake.clone() as Arc<dyn MqttTransport>, connected);

        let ev = event("atlas.keyframe", vec![1, 2, 3]);
        bearer.send(&ev).await.unwrap();

        let pubs = fake.publishes.lock().unwrap();
        assert_eq!(pubs.len(), 1);
        let (topic, qos, payload) = &pubs[0];
        assert_eq!(topic, "ados/dev1/atlas/keyframe");
        assert_eq!(*qos, MqttQos::AtLeastOnce);
        // The full envelope, decodable by the same path the LAN receiver uses.
        assert_eq!(AtlasEvent::from_msgpack(payload).unwrap(), ev);
    }

    #[tokio::test]
    async fn a_disconnected_broker_is_unavailable_and_publishes_nothing() {
        let fake = Arc::new(FakeTransport::default());
        let connected = Arc::new(AtomicBool::new(false));
        let bearer = CloudBearer::with_transport(
            "dev1",
            fake.clone() as Arc<dyn MqttTransport>,
            connected.clone(),
        );

        assert!(!bearer.is_available().await);
        let err = bearer
            .send(&event("atlas.keyframe", vec![9]))
            .await
            .unwrap_err();
        assert!(matches!(err, TransportError::Unavailable));
        assert!(err.is_retriable());
        assert!(fake.publishes.lock().unwrap().is_empty());
    }
}
