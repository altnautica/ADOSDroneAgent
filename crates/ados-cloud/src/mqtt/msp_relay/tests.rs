use super::*;
use std::time::Duration;

#[test]
fn the_relay_takes_its_own_client_id_off_the_mavlink_relays_config() {
    // Both spawn sites hand this relay a CLONE of the MAVLink relay's dial
    // config. Left as-is, the two would present the same MQTT ClientID and
    // the broker would evict whichever connected first, forever.
    let mavlink_config = TransportConfig {
        client_id: "ados-dev1".to_string(),
        host: "mqtt.example".to_string(),
        port: 443,
        wire: crate::mqtt::BrokerWire::Wss,
        ws_path: "/mqtt".to_string(),
        username: "ados-dev1".to_string(),
        password: "k".to_string(),
        inflight: 1000,
        keep_alive: Duration::from_secs(30),
    };
    let relay = MspMqttRelay::new("dev1", mavlink_config.clone());
    assert_eq!(relay.client_id(), "ados-dev1-msp");
    assert_ne!(relay.client_id(), mavlink_config.client_id);
    // The broker ACL keys on the username, which is unchanged: only the
    // session identity differs between the two lanes.
    assert_eq!(relay.username(), "ados-dev1");
    // The rest of the dial config rides through untouched.
    assert_eq!(relay.transport_config.host, "mqtt.example");
    assert_eq!(relay.transport_config.password, "k");
}
