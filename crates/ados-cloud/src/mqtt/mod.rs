//! MQTT layer: the broker transport seam, the MAVLink and MSP frame relays,
//! and the WebRTC SDP signaling relay.
//!
//! Topics + QoS:
//! * `ados/{id}/mavlink/tx` q0 (relay publishes FC->GCS frames)
//! * `ados/{id}/mavlink/rx` q0 (relay subscribes GCS->FC frames)
//! * `ados/{id}/webrtc/offer`  q1 (signaling subscribes browser offers)
//! * `ados/{id}/webrtc/answer` q1 (signaling publishes the SDP answer)
//!
//! Telemetry and status (`ados/{id}/telemetry`, `ados/{id}/status`) are
//! published by the Python gateway, which authenticates as the bare
//! `device_id`.
//!
//! The broker is `mqtt.altnautica.com:443` over WSS (`/mqtt`), TLS via the
//! shared ring-backed rustls config. The relays authenticate as
//! `ados-{device_id}`.
//!
//! ## One ClientID per lane, never per device
//!
//! MQTT requires a broker to DISCONNECT the existing session when a second
//! client presents the same ClientID, so every process/lane that dials the
//! broker for one device must carry its own id: `ados-{id}` (MAVLink relay),
//! `ados-{id}-msp` (MSP byte plane), `ados-{id}-atlas`, `ados-{id}-vision`,
//! `ados-{id}-gw` (the Python telemetry gateway). Two lanes sharing an id do not
//! degrade — they evict each other in a sub-second loop forever, which reads as
//! a flapping `mqttConnected` with no cloud telemetry and no cloud command
//! authority.

pub mod mavlink_relay;
pub mod msp_relay;
pub mod transport;
pub mod webrtc_signaling;

pub use mavlink_relay::{BoundedPublishQueue, MavlinkMqttRelay, INFLIGHT_LIMIT, QUEUE_MAXSIZE};
pub use msp_relay::MspMqttRelay;
pub use transport::{
    IncomingMessage, MqttQos, MqttTransport, RumqttcTransport, TransportConfig, TransportError,
};
pub use webrtc_signaling::WebrtcSignalingRelay;

/// The MQTT broker host the cloud relay dials. Mirrors the Python
/// `CloudConfig.mqtt_broker` default.
pub const DEFAULT_BROKER_HOST: &str = "mqtt.altnautica.com";

/// The broker port. Mirrors the Python `CloudConfig.mqtt_port` default (443,
/// WSS through the tunnel).
pub const DEFAULT_BROKER_PORT: u16 = 443;

/// The WebSocket path the broker serves MQTT on. Mirrors
/// `ws_set_options(path="/mqtt")`.
pub const WS_PATH: &str = "/mqtt";

/// Build the canonical topic strings for a device id.
pub fn topic_mavlink_tx(device_id: &str) -> String {
    format!("ados/{device_id}/mavlink/tx")
}
pub fn topic_mavlink_rx(device_id: &str) -> String {
    format!("ados/{device_id}/mavlink/rx")
}
/// The MSP byte plane topics (FC->GCS tx / GCS->FC rx) for an MSP FC. The sibling
/// of the `mavlink/{tx,rx}` frame plane; the relay bridges raw MSP bytes here.
pub fn topic_msp_tx(device_id: &str) -> String {
    format!("ados/{device_id}/msp/tx")
}
pub fn topic_msp_rx(device_id: &str) -> String {
    format!("ados/{device_id}/msp/rx")
}
/// The live vision-detection topic: offloaded detection batches published for a
/// hosted / off-LAN GCS, matching the LAN vision-detection WebSocket's shape so
/// the GCS parses one shape for both paths. A lossy live stream, published q0.
pub fn topic_vision_detections(device_id: &str) -> String {
    format!("ados/{device_id}/vision/detections")
}
pub fn topic_webrtc_offer(device_id: &str) -> String {
    format!("ados/{device_id}/webrtc/offer")
}
pub fn topic_webrtc_answer(device_id: &str) -> String {
    format!("ados/{device_id}/webrtc/answer")
}
/// Map an Atlas event topic to its cloud topic under `ados/{id}/atlas/...`.
/// The `plugin.atlas.` / `atlas.` prefix is dropped and dots become slashes, so
/// `atlas.keyframe`->`ados/{id}/atlas/keyframe`, `atlas.pose.offload`->
/// `ados/{id}/atlas/pose/offload`, `plugin.atlas.pose`->`ados/{id}/atlas/pose`.
pub fn topic_atlas(device_id: &str, event_topic: &str) -> String {
    let leaf = event_topic
        .trim_start_matches("plugin.atlas.")
        .trim_start_matches("atlas.")
        .replace('.', "/");
    format!("ados/{device_id}/atlas/{leaf}")
}

/// The per-relay MQTT username: `ados-{device_id}`.
pub fn relay_username(device_id: &str) -> String {
    format!("ados-{device_id}")
}

/// The MSP byte-plane relay's MQTT ClientID: `ados-{device_id}-msp`.
///
/// Its own broker principal, distinct from the MAVLink relay's `ados-{device_id}`
/// — see the ClientID rule in the module docs. The two relays run side by side
/// on an MSP rig against the same broker, so sharing the id disconnected both in
/// a permanent loop.
pub fn msp_client_id(device_id: &str) -> String {
    format!("ados-{device_id}-msp")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn topics_match_the_python_strings() {
        assert_eq!(topic_mavlink_tx("d"), "ados/d/mavlink/tx");
        assert_eq!(topic_mavlink_rx("d"), "ados/d/mavlink/rx");
        assert_eq!(topic_msp_tx("d"), "ados/d/msp/tx");
        assert_eq!(topic_msp_rx("d"), "ados/d/msp/rx");
        assert_eq!(topic_vision_detections("d"), "ados/d/vision/detections");
        assert_eq!(topic_webrtc_offer("d"), "ados/d/webrtc/offer");
        assert_eq!(topic_webrtc_answer("d"), "ados/d/webrtc/answer");
    }

    #[test]
    fn every_broker_lane_for_one_device_has_its_own_client_id() {
        // A duplicate ClientID makes the broker evict the sibling session, so
        // the lanes kick each other in a permanent loop rather than degrading.
        // The MAVLink relay holds the bare `ados-{id}`; every other lane suffixes.
        let ids = [
            relay_username("dev1"),
            msp_client_id("dev1"),
            format!("ados-{}-atlas", "dev1"),
            format!("ados-{}-vision", "dev1"),
        ];
        assert_eq!(msp_client_id("dev1"), "ados-dev1-msp");
        let unique: std::collections::BTreeSet<&String> = ids.iter().collect();
        assert_eq!(
            unique.len(),
            ids.len(),
            "client ids must be distinct: {ids:?}"
        );
    }
}
