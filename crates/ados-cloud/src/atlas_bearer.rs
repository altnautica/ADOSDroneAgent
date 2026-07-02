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
//! **The cloud lane carries DESCRIPTORS, not multi-MB artifacts.** A full-res
//! keyframe at q1 over a thin uplink head-of-line blocks the SHARED broker
//! connection — the gateway's telemetry/status ride the same client — so a
//! framed event over [`CLOUD_MAX_PAYLOAD`] is declined with a (retriable)
//! `PayloadTooLarge`; the ladder offers it to a bearer that fits (the direct-LAN
//! or post-flight-bulk lane), ending in `NoBearer` if none does. Small events
//! (pose / occupancy / capture-state / compact descriptors) ride the cloud lane
//! so a remote operator keeps situational awareness off-LAN. A per-publish
//! timeout bounds a wedged broker so the publish never stalls the ladder caller.
//!
//! Honest liveness: `is_available()` reads the real
//! ConnAck-driven `connected` flag, not "the task exists". `send()` returning
//! `Ok` means the event was ENQUEUED for delivery against a confirmed-up session
//! (q1 hands rumqttc the PubAck retry), NOT that the broker has acked it — a
//! down link reports unavailable so the ladder never reaches the cloud rung when
//! it cannot deliver, and a LAN-only agent never even builds this bearer.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use ados_atlas_transport::{AtlasBearer, AtlasEvent, BearerKind, TransportError};

use crate::mqtt::topic_atlas;
use crate::mqtt::transport::{MqttQos, MqttTransport, RumqttcTransport};

/// The cloud lane's per-event ceiling: descriptors ride, multi-MB artifacts do
/// not (they head-of-line block the shared connection on a thin uplink).
pub const CLOUD_MAX_PAYLOAD: usize = 256 * 1024;

/// Bound on one publish so a wedged-but-connected broker cannot hang the ladder.
const PUBLISH_TIMEOUT: Duration = Duration::from_secs(10);

/// The live high-rate pose stream rides at-most-once (recency beats completeness,
/// like telemetry); every discrete descriptor (occupancy / capture state /
/// offloaded pose / small splat-or-mesh metadata) rides at-least-once so it is
/// not silently lost off a lossy link.
fn atlas_qos(event_topic: &str) -> MqttQos {
    if event_topic == "plugin.atlas.pose" {
        MqttQos::AtMostOnce
    } else {
        MqttQos::AtLeastOnce
    }
}

/// A topic safe to publish to: no MQTT wildcards/reserved chars and a non-empty
/// leaf (the event topic was not a bare prefix yielding a trailing slash).
fn is_publishable_topic(topic: &str) -> bool {
    !topic.ends_with('/') && !topic.contains(['#', '+', '$'])
}

/// Publishes framed Atlas descriptors over the shared broker connection.
pub struct CloudBearer {
    device_id: String,
    transport: Arc<dyn MqttTransport>,
    /// The transport's ConnAck-driven connectivity flag (shared handle).
    connected: Arc<AtomicBool>,
    /// Per-event size ceiling for this lane.
    max_payload: usize,
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
            max_payload: CLOUD_MAX_PAYLOAD,
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
            max_payload: CLOUD_MAX_PAYLOAD,
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
        // Publish the WHOLE envelope (event.encode()), the identical wire the
        // LAN receiver (atlas_event_router) decodes, so the off-LAN consumer uses
        // the same decode path. Never publish event.payload alone.
        let body = event
            .encode()
            .map_err(|e| TransportError::Encode(e.to_string()))?;
        // Descriptors only: decline a multi-MB artifact (retriable) so the ladder
        // offers it to a fatter lane instead of backing up the shared connection.
        if body.len() > self.max_payload {
            return Err(TransportError::PayloadTooLarge(body.len()));
        }
        let topic = topic_atlas(&self.device_id, &event.topic);
        if !is_publishable_topic(&topic) {
            return Err(TransportError::Encode(format!(
                "invalid atlas topic: {topic}"
            )));
        }
        let qos = atlas_qos(&event.topic);
        // Bound the publish: a wedged-but-connected broker must not hang the
        // ladder caller. Both elapsed and a transport error are connectivity
        // failures → Unavailable (retriable).
        match tokio::time::timeout(PUBLISH_TIMEOUT, self.transport.publish(&topic, qos, body)).await
        {
            Ok(Ok(())) => Ok(()),
            Ok(Err(_)) => Err(TransportError::Unavailable),
            Err(_) => Err(TransportError::Unavailable),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mqtt::transport::test_support::FakeTransport;

    fn event(topic: &str, payload: Vec<u8>) -> AtlasEvent {
        AtlasEvent::new(topic, None, payload)
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

    #[test]
    fn a_wildcard_or_empty_leaf_is_not_publishable() {
        assert!(is_publishable_topic("ados/d/atlas/keyframe"));
        assert!(!is_publishable_topic("ados/d/atlas/")); // empty leaf
        assert!(!is_publishable_topic("ados/d/atlas/a#b")); // wildcard
        assert!(!is_publishable_topic("ados/d/atlas/a+b"));
        assert!(!is_publishable_topic("ados/d/atlas/$sys"));
    }

    #[tokio::test]
    async fn a_descriptor_publishes_the_envelope_at_q1_on_the_atlas_topic() {
        let fake = Arc::new(FakeTransport::default());
        let connected = Arc::new(AtomicBool::new(true));
        let bearer =
            CloudBearer::with_transport("dev1", fake.clone() as Arc<dyn MqttTransport>, connected);

        let ev = event("atlas.occupancy", vec![1, 2, 3]);
        bearer.send(&ev).await.unwrap();

        let pubs = fake.publishes.lock().unwrap();
        assert_eq!(pubs.len(), 1);
        let (topic, qos, payload) = &pubs[0];
        assert_eq!(topic, "ados/dev1/atlas/occupancy");
        assert_eq!(*qos, MqttQos::AtLeastOnce);
        // The full envelope, decodable by the same path the LAN receiver uses.
        assert_eq!(AtlasEvent::decode(payload).unwrap(), ev);
    }

    #[tokio::test]
    async fn an_oversized_artifact_is_declined_retriably_and_not_published() {
        let fake = Arc::new(FakeTransport::default());
        let connected = Arc::new(AtomicBool::new(true));
        let bearer =
            CloudBearer::with_transport("dev1", fake.clone() as Arc<dyn MqttTransport>, connected);

        // A multi-MB keyframe must not ride the shared cloud connection.
        let err = bearer
            .send(&event("atlas.keyframe", vec![0u8; CLOUD_MAX_PAYLOAD + 1]))
            .await
            .unwrap_err();
        assert!(matches!(err, TransportError::PayloadTooLarge(_)));
        assert!(
            err.is_retriable(),
            "a fatter lane (LAN) carries the keyframe"
        );
        assert!(fake.publishes.lock().unwrap().is_empty());
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
            .send(&event("atlas.occupancy", vec![9]))
            .await
            .unwrap_err();
        assert!(matches!(err, TransportError::Unavailable));
        assert!(err.is_retriable());
        assert!(fake.publishes.lock().unwrap().is_empty());
    }
}
