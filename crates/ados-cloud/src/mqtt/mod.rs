//! MQTT layer: the broker transport seam, the telemetry/status gateway, the
//! MAVLink frame relay, and the WebRTC SDP signaling relay.
//!
//! Topics + QoS mirror the Python cloud relay exactly:
//! * `ados/{id}/telemetry` q0  (gateway publishes vehicle state ~2 Hz)
//! * `ados/{id}/status`    q1  (gateway publishes a small status doc)
//! * `ados/{id}/mavlink/tx` q0 (relay publishes FC->GCS frames)
//! * `ados/{id}/mavlink/rx` q0 (relay subscribes GCS->FC frames)
//! * `ados/{id}/webrtc/offer`  q1 (signaling subscribes browser offers)
//! * `ados/{id}/webrtc/answer` q1 (signaling publishes the SDP answer)
//!
//! The broker is `mqtt.altnautica.com:443` over WSS (`/mqtt`), TLS via the
//! shared RustCrypto rustls config. The gateway authenticates as the bare
//! `device_id`; the relays authenticate as `ados-{device_id}` — that per-relay
//! username inconsistency is preserved exactly from the Python source (the
//! broker ACL pattern keys on the bare-id form for the gateway's own topic
//! subtree).

pub mod gateway;
pub mod mavlink_relay;
pub mod msp_relay;
pub mod transport;
pub mod webrtc_signaling;

pub use gateway::{MqttGateway, StatusDoc};
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
pub fn topic_telemetry(device_id: &str) -> String {
    format!("ados/{device_id}/telemetry")
}
pub fn topic_status(device_id: &str) -> String {
    format!("ados/{device_id}/status")
}
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

/// The gateway's MQTT username: the bare device id (so the broker ACL pattern
/// `ados/%u/#` substitutes to the agent's own topic subtree). Mirrors the
/// gateway's `mqtt_user = self._device_id`.
pub fn gateway_username(device_id: &str) -> String {
    device_id.to_string()
}

/// The per-relay MQTT username: `ados-{device_id}`. Preserved verbatim from the
/// Python relay construction (`username=f"ados-{device_id}"`), which differs
/// from the gateway's bare-id form.
pub fn relay_username(device_id: &str) -> String {
    format!("ados-{device_id}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn topics_match_the_python_strings() {
        assert_eq!(topic_telemetry("d"), "ados/d/telemetry");
        assert_eq!(topic_status("d"), "ados/d/status");
        assert_eq!(topic_mavlink_tx("d"), "ados/d/mavlink/tx");
        assert_eq!(topic_mavlink_rx("d"), "ados/d/mavlink/rx");
        assert_eq!(topic_msp_tx("d"), "ados/d/msp/tx");
        assert_eq!(topic_msp_rx("d"), "ados/d/msp/rx");
        assert_eq!(topic_vision_detections("d"), "ados/d/vision/detections");
        assert_eq!(topic_webrtc_offer("d"), "ados/d/webrtc/offer");
        assert_eq!(topic_webrtc_answer("d"), "ados/d/webrtc/answer");
    }

    #[test]
    fn username_inconsistency_is_preserved() {
        // The gateway uses the bare device id; the relays prefix `ados-`.
        assert_eq!(gateway_username("dev1"), "dev1");
        assert_eq!(relay_username("dev1"), "ados-dev1");
    }
}
