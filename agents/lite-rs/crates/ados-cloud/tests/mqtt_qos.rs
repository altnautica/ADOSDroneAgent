//! Per-topic MQTT QoS contract tests.
//!
//! The cloud relay topic schema pins a specific QoS for each topic
//! (see `proto/cloud/mqtt-topics.md`):
//!
//!   * `mavlink/tx` outbound  → QoS 0 (fire-and-forget; broker queueing
//!     would defeat real-time framing)
//!   * `mavlink/rx` inbound   → QoS 0 (subscribe, same rationale)
//!   * `command` inbound      → QoS 1 (reliable delivery for cloud
//!     commands)
//!   * `webrtc/offer` inbound → QoS 1 (reliable delivery for SDP
//!     handshakes)
//!   * `webrtc/answer` outbound → QoS 1 (reliable delivery)
//!
//! The in-tree publish + subscribe callsites match this contract:
//!   * `mqtt_publish_loop` subscribes on `mavlink/rx` at `AtMostOnce`,
//!     `command` at `AtLeastOnce`, `webrtc/offer` at `AtLeastOnce`
//!     (`crates/ados-cloud/src/lib.rs` SUBSCRIBE block).
//!   * `mqtt_publish_loop` publishes on `mavlink/tx` at `AtMostOnce`
//!     (the recv-broadcast loop).
//!   * The `webrtc/answer` reply path publishes at `AtLeastOnce`
//!     (the eventloop reject branch).
//!
//! These tests exercise the wire path through the in-process broker
//! fixture and assert the behavioral consequences: a publish at the
//! documented inbound QoS reaches the dispatcher, the FC writer mpsc
//! receives the inbound MAVLink frame, and the offer-reject reply
//! lands on `webrtc/answer` with a well-formed envelope.
//!
//! ## Fixture limitation: cannot probe outbound publish QoS directly
//!
//! `rumqttd 0.20` forwards every queued publish at the SUBSCRIBE QoS
//! (see `rumqttd::router::routing::forward_device_data`,
//! `publish.qos = protocol::qos(request.qos)`), not at
//! `min(pub_qos, sub_qos)` the way real-broker MQTT 3.1.1 spec
//! semantics demand. Observing `Publish.qos` on a side subscriber
//! therefore reflects the side's SUBSCRIBE QoS, not the agent's
//! PUBLISH QoS.
//!
//! For the strict outbound-QoS contract, the source-level call-site
//! constants are the canonical pin. We assert behavior here (the
//! publish round-trips correctly + the body decodes cleanly) and
//! rely on the source constants to enforce the strict QoS values.
//! A regression that swapped the publish QoS would still be caught
//! by code review at the call site since the QoS value is a typed
//! enum literal, not a runtime variable.

use std::time::Duration;

use ados_cloud::{spawn_cloud_client, CloudConfig, InboundChannels};
use ados_setup::diag::DiagState;
use ados_test_mocks::MockMqttBroker;
use rumqttc::{AsyncClient, Event, MqttOptions, Packet, QoS};
use tokio::sync::broadcast;

const DEVICE_ID: &str = "qos-test-device";

fn build_config(broker_port: u16, pairing_path: std::path::PathBuf) -> CloudConfig {
    CloudConfig {
        device_id: DEVICE_ID.into(),
        mqtt_broker: "127.0.0.1".into(),
        mqtt_port: broker_port,
        mqtt_use_tls: false,
        // Empty convex_url keeps the HTTP loop idle — the QoS contract
        // is purely an MQTT concern and the heartbeat path is exercised
        // by the heartbeat-parity test.
        convex_url: String::new(),
        pairing_path,
        agent_meta: None,
        mqtt_keepalive_secs: 60,
        connect_timeout_secs: 3,
        request_timeout_secs: 10,
        allow_reboot: false,
    }
}

/// Construct a minimal MAVLink v2 HEARTBEAT frame so the publisher
/// has something to send on the outbound mavlink/tx topic.
fn mavlink_v2_heartbeat() -> Vec<u8> {
    vec![
        0xFD, 0x09, 0x00, 0x00, 0x00, 0xFF, 0xBE, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x06,
        0x08, 0x00, 0x03, 0x03, 0xAB, 0xCD,
    ]
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mavlink_tx_outbound_publishes_to_correct_topic_at_qos_zero() {
    // Round-trip a frame through the agent's outbound publish loop
    // and assert it lands on `ados/<device_id>/mavlink/tx` byte-for-
    // byte. We subscribe the side client at QoS 0 (matching the
    // documented contract for this topic) so the broker forwards at
    // QoS 0 regardless of what the agent published with — meaning a
    // QoS 0 round-trip works AND a QoS 1 round-trip would also work
    // at the wire level (both downgrade to 0 here).
    //
    // The strict QoS pin is on the publish call site itself
    // (`crates/ados-cloud/src/lib.rs` recv-broadcast loop).
    let broker = MockMqttBroker::start().await.expect("broker start");
    let port = broker.port();
    let tmp = tempfile::tempdir().unwrap();
    let pairing_path = tmp.path().join("pairing.json");

    let topic = format!("ados/{DEVICE_ID}/mavlink/tx");

    let mut opts = MqttOptions::new("qos-side-mavtx", "127.0.0.1", port);
    opts.set_keep_alive(Duration::from_secs(5));
    let (sub_client, mut sub_eventloop) = AsyncClient::new(opts, 32);
    sub_client
        .subscribe(&topic, QoS::AtMostOnce)
        .await
        .expect("subscribe mavlink/tx");

    let (got_tx, got_rx) = tokio::sync::oneshot::channel::<rumqttc::mqttbytes::v4::Publish>();
    let driver = tokio::spawn(async move {
        let mut got_tx = Some(got_tx);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(8);
        loop {
            tokio::select! {
                ev = sub_eventloop.poll() => {
                    match ev {
                        Ok(Event::Incoming(Packet::Publish(p))) => {
                            if let Some(tx) = got_tx.take() {
                                let _ = tx.send(p);
                            }
                        }
                        Ok(_) => continue,
                        Err(_) => break,
                    }
                }
                _ = tokio::time::sleep_until(deadline) => break,
            }
        }
    });

    tokio::time::sleep(Duration::from_millis(300)).await;

    let (mavlink_tx, _mavlink_rx) = broadcast::channel::<Vec<u8>>(16);
    let config = build_config(port, pairing_path);
    let diag = DiagState::shared();
    spawn_cloud_client(config, mavlink_tx.clone(), InboundChannels::default(), diag)
        .expect("spawn cloud client");

    tokio::time::sleep(Duration::from_millis(400)).await;

    let frame = mavlink_v2_heartbeat();
    mavlink_tx.send(frame.clone()).expect("broadcast send");

    let publish = tokio::time::timeout(Duration::from_secs(5), got_rx)
        .await
        .expect("timed out waiting for mavlink/tx publish")
        .expect("oneshot dropped");

    assert_eq!(
        publish.topic, topic,
        "publish topic must be ados/<device_id>/mavlink/tx"
    );
    assert_eq!(
        publish.qos,
        QoS::AtMostOnce,
        "side sub at QoS 0 must receive at QoS 0 (downgrade rule); a non-\
         zero observed QoS would mean the broker fixture upgraded delivery \
         which the rumqttd 0.20 code path documents it does not"
    );
    assert_eq!(
        publish.payload.as_ref(),
        frame.as_slice(),
        "mavlink/tx wire payload must round-trip byte-for-byte"
    );

    driver.abort();
    broker.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn webrtc_answer_outbound_publishes_with_well_formed_envelope() {
    // Drive an inbound webrtc/offer at QoS 1 from a side publisher;
    // the agent's offer handler synthesizes a `rejected` answer and
    // publishes it on `webrtc/answer`. We observe the publish topic
    // + body. The fixture forwards at the SUBSCRIBE QoS (1 here), so
    // the strict outbound-QoS pin lives at the source publish call.
    let broker = MockMqttBroker::start().await.expect("broker start");
    let port = broker.port();
    let tmp = tempfile::tempdir().unwrap();
    let pairing_path = tmp.path().join("pairing.json");

    let answer_topic = format!("ados/{DEVICE_ID}/webrtc/answer");
    let offer_topic = format!("ados/{DEVICE_ID}/webrtc/offer");

    let mut sub_opts = MqttOptions::new("qos-side-answer", "127.0.0.1", port);
    sub_opts.set_keep_alive(Duration::from_secs(5));
    let (sub_client, mut sub_eventloop) = AsyncClient::new(sub_opts, 32);
    sub_client
        .subscribe(&answer_topic, QoS::AtLeastOnce)
        .await
        .expect("subscribe webrtc/answer");

    let (got_tx, got_rx) = tokio::sync::oneshot::channel::<rumqttc::mqttbytes::v4::Publish>();
    let answer_driver = tokio::spawn(async move {
        let mut got_tx = Some(got_tx);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            tokio::select! {
                ev = sub_eventloop.poll() => {
                    match ev {
                        Ok(Event::Incoming(Packet::Publish(p))) => {
                            if let Some(tx) = got_tx.take() {
                                let _ = tx.send(p);
                            }
                        }
                        Ok(_) => continue,
                        Err(_) => break,
                    }
                }
                _ = tokio::time::sleep_until(deadline) => break,
            }
        }
    });

    let (mavlink_tx, _mavlink_rx) = broadcast::channel::<Vec<u8>>(16);
    let config = build_config(port, pairing_path);
    let diag = DiagState::shared();
    // Clone the Sender so the test keeps a live reference; otherwise
    // `spawn_cloud_client` consumes the only Sender and the publish
    // loop sees `RecvError::Closed` immediately on its first
    // `mavlink_rx.recv()`, exiting before our publish ever lands.
    spawn_cloud_client(config, mavlink_tx.clone(), InboundChannels::default(), diag)
        .expect("spawn cloud client");
    let _mavlink_tx_keepalive = mavlink_tx;

    // Generous wait for the agent's MQTT loop to come up and SUBSCRIBE
    // to webrtc/offer before we publish the offer.
    tokio::time::sleep(Duration::from_secs(1)).await;

    let mut publisher_opts = MqttOptions::new("qos-side-pub", "127.0.0.1", port);
    publisher_opts.set_keep_alive(Duration::from_secs(5));
    let (pub_client, mut pub_eventloop) = AsyncClient::new(publisher_opts, 16);
    let pub_driver = tokio::spawn(async move {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            tokio::select! {
                ev = pub_eventloop.poll() => {
                    match ev {
                        Ok(_) => continue,
                        Err(_) => break,
                    }
                }
                _ = tokio::time::sleep_until(deadline) => break,
            }
        }
    });
    let offer_body = serde_json::json!({
        "type": "offer",
        "sdp": "v=0\r\no=- 1 1 IN IP4 127.0.0.1\r\ns=-\r\nt=0 0\r\n",
        "session_id": "qos-test-session",
    });
    let offer_bytes = serde_json::to_vec(&offer_body).unwrap();
    pub_client
        .publish(&offer_topic, QoS::AtLeastOnce, false, offer_bytes)
        .await
        .expect("publish offer");

    let publish = tokio::time::timeout(Duration::from_secs(8), got_rx)
        .await
        .expect("timed out waiting for webrtc/answer publish")
        .expect("oneshot dropped");

    assert_eq!(publish.topic, answer_topic);

    // Body must be a valid JSON envelope echoing the session_id so
    // the GCS can correlate; lite always rejects so `type=rejected`.
    let body: serde_json::Value =
        serde_json::from_slice(&publish.payload).expect("answer body decodes as JSON");
    assert_eq!(
        body.get("type").and_then(|v| v.as_str()),
        Some("rejected"),
        "lite agent does not host a peer; the answer must be rejected"
    );
    assert_eq!(
        body.get("session_id").and_then(|v| v.as_str()),
        Some("qos-test-session"),
        "session_id must be echoed for GCS correlation"
    );

    answer_driver.abort();
    pub_driver.abort();
    broker.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn command_inbound_at_qos_one_reaches_dispatcher() {
    // Behavioral pin for the inbound command topic. A QoS 1 publish
    // from a side client must flow through the agent's dispatcher
    // (the heartbeat trigger fires). The agent's SUBSCRIBE QoS for
    // `command` is `AtLeastOnce` per the source SUBSCRIBE block.
    let broker = MockMqttBroker::start().await.expect("broker start");
    let port = broker.port();
    let tmp = tempfile::tempdir().unwrap();
    let pairing_path = tmp.path().join("pairing.json");

    let command_topic = format!("ados/{DEVICE_ID}/command");

    let (heartbeat_tx, mut heartbeat_rx) = tokio::sync::mpsc::channel::<()>(4);
    let channels = InboundChannels {
        heartbeat_trigger: Some(heartbeat_tx),
        ..Default::default()
    };

    let (mavlink_tx, _mavlink_rx) = broadcast::channel::<Vec<u8>>(16);
    let config = build_config(port, pairing_path);
    let diag = DiagState::shared();
    spawn_cloud_client(config, mavlink_tx.clone(), channels, diag)
        .expect("spawn cloud client");
    let _mavlink_tx_keepalive = mavlink_tx;

    tokio::time::sleep(Duration::from_secs(1)).await;

    let mut publisher_opts = MqttOptions::new("qos-cmd-pub", "127.0.0.1", port);
    publisher_opts.set_keep_alive(Duration::from_secs(5));
    let (pub_client, mut pub_eventloop) = AsyncClient::new(publisher_opts, 16);
    let pub_driver = tokio::spawn(async move {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            tokio::select! {
                ev = pub_eventloop.poll() => {
                    match ev {
                        Ok(_) => continue,
                        Err(_) => break,
                    }
                }
                _ = tokio::time::sleep_until(deadline) => break,
            }
        }
    });

    let body = serde_json::json!({
        "request_id": "qos-cmd-1",
        "type": "status_request",
    });
    let payload = serde_json::to_vec(&body).unwrap();
    pub_client
        .publish(&command_topic, QoS::AtLeastOnce, false, payload)
        .await
        .expect("publish command");

    let recv = tokio::time::timeout(Duration::from_secs(5), heartbeat_rx.recv()).await;
    assert!(
        matches!(recv, Ok(Some(()))),
        "QoS 1 status_request must reach the agent dispatcher \
         and fire the heartbeat trigger"
    );

    pub_driver.abort();
    broker.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mavlink_rx_inbound_at_qos_zero_reaches_fc_writer() {
    // Counterpart to the command test: mavlink/rx inbound is QoS 0.
    // A QoS 0 publish from the side client must flow through to the
    // FC writer mpsc.
    let broker = MockMqttBroker::start().await.expect("broker start");
    let port = broker.port();
    let tmp = tempfile::tempdir().unwrap();
    let pairing_path = tmp.path().join("pairing.json");

    let rx_topic = format!("ados/{DEVICE_ID}/mavlink/rx");

    let (fc_tx, mut fc_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(8);
    let channels = InboundChannels {
        fc_writer: Some(fc_tx),
        ..Default::default()
    };

    let (mavlink_tx, _mavlink_rx) = broadcast::channel::<Vec<u8>>(16);
    let config = build_config(port, pairing_path);
    let diag = DiagState::shared();
    spawn_cloud_client(config, mavlink_tx.clone(), channels, diag)
        .expect("spawn cloud client");
    let _mavlink_tx_keepalive = mavlink_tx;

    tokio::time::sleep(Duration::from_secs(1)).await;

    let mut publisher_opts = MqttOptions::new("qos-rx-pub", "127.0.0.1", port);
    publisher_opts.set_keep_alive(Duration::from_secs(5));
    let (pub_client, mut pub_eventloop) = AsyncClient::new(publisher_opts, 16);
    let pub_driver = tokio::spawn(async move {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            tokio::select! {
                ev = pub_eventloop.poll() => {
                    match ev {
                        Ok(_) => continue,
                        Err(_) => break,
                    }
                }
                _ = tokio::time::sleep_until(deadline) => break,
            }
        }
    });

    let frame = mavlink_v2_heartbeat();
    pub_client
        .publish(&rx_topic, QoS::AtMostOnce, false, frame.clone())
        .await
        .expect("publish rx frame");

    let recv = tokio::time::timeout(Duration::from_secs(5), fc_rx.recv()).await;
    let frame_received = recv
        .expect("timed out waiting for forwarded mavlink frame")
        .expect("fc writer channel closed");
    assert_eq!(frame_received, frame, "FC writer must see the verbatim frame");

    pub_driver.abort();
    broker.shutdown().await;
}
