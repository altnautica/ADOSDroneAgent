//! Wire-format codec parity for the four canonical MQTT topics.
//!
//! `proto/cloud/mqtt-topics.md` documents the payload shape on each
//! topic. The lite agent's cloud client and any side observer
//! (Mission Control's relay services, the MQTT bridge container,
//! a CLI debugger) MUST agree on those wire formats. This test runs
//! a thin contract check: spin the in-process MQTT broker, drive
//! representative payloads through each topic in both directions,
//! and verify every payload decodes cleanly back to its source
//! envelope.
//!
//! Drift on either side (the agent emits a non-JSON envelope for
//! `webrtc/answer`, the relay accidentally forwards a non-MAVLink
//! frame on `mavlink/rx`, a future refactor switches `command` to
//! msgpack without updating the spec) produces a parse error.
//!
//! The two MAVLink topics carry raw binary frames per the spec, so
//! the "decode" check there is a byte-for-byte equality assertion.
//! The two WebRTC topics carry JSON envelopes; we round-trip via
//! `serde_json::from_slice`.

use std::time::Duration;

use ados_cloud::{spawn_cloud_client, CloudConfig, InboundChannels};
use ados_setup::diag::DiagState;
use ados_test_mocks::MockMqttBroker;
use rumqttc::{AsyncClient, Event, MqttOptions, Packet, QoS};
use tokio::sync::broadcast;

const DEVICE_ID: &str = "drift-device";

fn make_config(broker_port: u16, pairing_path: std::path::PathBuf) -> CloudConfig {
    CloudConfig {
        device_id: DEVICE_ID.into(),
        mqtt_broker: "127.0.0.1".into(),
        mqtt_port: broker_port,
        mqtt_use_tls: false,
        // HTTP idle for this MQTT-only contract test.
        convex_url: String::new(),
        pairing_path,
        agent_meta: None,
        mqtt_keepalive_secs: 60,
        connect_timeout_secs: 3,
        request_timeout_secs: 10,
        allow_reboot: false,
    }
}

/// MAVLink v2 HEARTBEAT frame. The lite agent treats inbound and
/// outbound frames as opaque blobs; the wire-format contract is just
/// "the raw bytes pass through unchanged".
fn mavlink_v2_heartbeat() -> Vec<u8> {
    vec![
        0xFD, 0x09, 0x00, 0x00, 0x00, 0xFF, 0xBE, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x06,
        0x08, 0x00, 0x03, 0x03, 0xAB, 0xCD,
    ]
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn outbound_mavlink_tx_round_trips_raw_bytes() {
    let broker = MockMqttBroker::start().await.expect("broker start");
    let port = broker.port();
    let tmp = tempfile::tempdir().unwrap();
    let pairing_path = tmp.path().join("pairing.json");

    let topic = format!("ados/{DEVICE_ID}/mavlink/tx");

    // Side subscriber observes the agent's outbound publish.
    let mut opts = MqttOptions::new("drift-mavtx-sub", "127.0.0.1", port);
    opts.set_keep_alive(Duration::from_secs(5));
    let (sub_client, mut sub_eventloop) = AsyncClient::new(opts, 32);
    sub_client
        .subscribe(&topic, QoS::AtMostOnce)
        .await
        .expect("subscribe mavlink/tx");

    let (got_tx, got_rx) = tokio::sync::oneshot::channel::<Vec<u8>>();
    let driver = tokio::spawn(async move {
        let mut got_tx = Some(got_tx);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(8);
        loop {
            tokio::select! {
                ev = sub_eventloop.poll() => {
                    match ev {
                        Ok(Event::Incoming(Packet::Publish(p))) => {
                            if let Some(tx) = got_tx.take() {
                                let _ = tx.send(p.payload.to_vec());
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
    let config = make_config(port, pairing_path);
    let diag = DiagState::shared();
    spawn_cloud_client(config, mavlink_tx.clone(), InboundChannels::default(), diag)
        .expect("spawn cloud client");

    tokio::time::sleep(Duration::from_millis(400)).await;

    let frame = mavlink_v2_heartbeat();
    mavlink_tx.send(frame.clone()).expect("broadcast");

    let received = tokio::time::timeout(Duration::from_secs(5), got_rx)
        .await
        .expect("timed out waiting for mavlink/tx publish")
        .expect("oneshot dropped");

    // The "decode" for the raw MAVLink topic is a byte-equality
    // check: the wire format is opaque and the lite agent must not
    // mutate it on the way out.
    assert_eq!(
        received, frame,
        "outbound mavlink/tx wire format must round-trip byte-for-byte; \
         any drift here means the publish path is mutating the frame"
    );

    driver.abort();
    broker.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn inbound_mavlink_rx_round_trips_raw_bytes() {
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
    let config = make_config(port, pairing_path);
    let diag = DiagState::shared();
    spawn_cloud_client(config, mavlink_tx.clone(), channels, diag)
        .expect("spawn cloud client");
    // Keep the broadcast Sender alive for the test duration; without
    // this, `spawn_cloud_client` consumes the only Sender, the
    // publish loop sees `RecvError::Closed` and exits before the
    // inbound publish reaches the FC-writer mpsc.
    let _mavlink_tx_keepalive = mavlink_tx;

    tokio::time::sleep(Duration::from_millis(500)).await;

    let mut pub_opts = MqttOptions::new("drift-mavrx-pub", "127.0.0.1", port);
    pub_opts.set_keep_alive(Duration::from_secs(5));
    let (pub_client, mut pub_eventloop) = AsyncClient::new(pub_opts, 16);
    let pub_driver = tokio::spawn(async move {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(8);
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
        .expect("publish mavlink/rx");

    let received = tokio::time::timeout(Duration::from_secs(5), fc_rx.recv())
        .await
        .expect("timed out waiting for fc writer mpsc")
        .expect("fc writer mpsc closed");
    assert_eq!(
        received, frame,
        "inbound mavlink/rx wire format must round-trip byte-for-byte to the FC writer"
    );

    pub_driver.abort();
    broker.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn webrtc_offer_answer_round_trips_json_envelope() {
    let broker = MockMqttBroker::start().await.expect("broker start");
    let port = broker.port();
    let tmp = tempfile::tempdir().unwrap();
    let pairing_path = tmp.path().join("pairing.json");

    let offer_topic = format!("ados/{DEVICE_ID}/webrtc/offer");
    let answer_topic = format!("ados/{DEVICE_ID}/webrtc/answer");

    // Subscribe to webrtc/answer at QoS 1 to observe the agent's
    // outbound rejection envelope.
    let mut sub_opts = MqttOptions::new("drift-webrtc-sub", "127.0.0.1", port);
    sub_opts.set_keep_alive(Duration::from_secs(5));
    let (sub_client, mut sub_eventloop) = AsyncClient::new(sub_opts, 32);
    sub_client
        .subscribe(&answer_topic, QoS::AtLeastOnce)
        .await
        .expect("subscribe webrtc/answer");

    let (got_tx, got_rx) = tokio::sync::oneshot::channel::<Vec<u8>>();
    let driver = tokio::spawn(async move {
        let mut got_tx = Some(got_tx);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(8);
        loop {
            tokio::select! {
                ev = sub_eventloop.poll() => {
                    match ev {
                        Ok(Event::Incoming(Packet::Publish(p))) => {
                            if let Some(tx) = got_tx.take() {
                                let _ = tx.send(p.payload.to_vec());
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
    let config = make_config(port, pairing_path);
    let diag = DiagState::shared();
    spawn_cloud_client(config, mavlink_tx.clone(), InboundChannels::default(), diag)
        .expect("spawn cloud client");
    let _mavlink_tx_keepalive = mavlink_tx;

    tokio::time::sleep(Duration::from_millis(500)).await;

    // Inbound offer envelope: SDP-typed JSON object with a
    // session_id the agent must echo back.
    let mut pub_opts = MqttOptions::new("drift-webrtc-pub", "127.0.0.1", port);
    pub_opts.set_keep_alive(Duration::from_secs(5));
    let (pub_client, mut pub_eventloop) = AsyncClient::new(pub_opts, 16);
    let pub_driver = tokio::spawn(async move {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(8);
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

    let offer = serde_json::json!({
        "type": "offer",
        "sdp": "v=0\r\no=- 1 1 IN IP4 127.0.0.1\r\ns=-\r\nt=0 0\r\n",
        "session_id": "drift-session-abc",
    });
    let offer_bytes = serde_json::to_vec(&offer).expect("encode offer");

    // Decode our own envelope to confirm the test fixture matches
    // the documented schema before we push it on the wire.
    let _: serde_json::Value =
        serde_json::from_slice(&offer_bytes).expect("offer fixture decodes back as JSON");

    pub_client
        .publish(&offer_topic, QoS::AtLeastOnce, false, offer_bytes)
        .await
        .expect("publish offer");

    let answer_bytes = tokio::time::timeout(Duration::from_secs(5), got_rx)
        .await
        .expect("timed out waiting for webrtc/answer")
        .expect("oneshot dropped");

    // Round-trip the answer envelope. Drift in the agent's encoder
    // would either produce non-JSON bytes (decode fails) or drop
    // the session_id key (the GCS would lose its correlation).
    let answer: serde_json::Value =
        serde_json::from_slice(&answer_bytes).expect("answer body decodes as JSON");
    let obj = answer
        .as_object()
        .expect("answer is a JSON object");
    assert!(
        obj.contains_key("type"),
        "answer envelope must carry a `type` field"
    );
    assert_eq!(
        obj.get("session_id").and_then(|v| v.as_str()),
        Some("drift-session-abc"),
        "answer must echo the inbound session_id verbatim"
    );

    driver.abort();
    pub_driver.abort();
    broker.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn command_envelope_decodes_as_json_at_dispatcher() {
    // The `command` topic carries a JSON envelope. We exercise the
    // dispatcher round-trip indirectly: a well-formed JSON
    // status_request must reach the heartbeat trigger, while a
    // malformed payload must NOT. Either outcome confirms the
    // codec is `serde_json::from_slice` on a UTF-8 byte slice.
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
    let config = make_config(port, pairing_path);
    let diag = DiagState::shared();
    spawn_cloud_client(config, mavlink_tx.clone(), channels, diag)
        .expect("spawn cloud client");
    let _mavlink_tx_keepalive = mavlink_tx;

    tokio::time::sleep(Duration::from_millis(500)).await;

    let mut pub_opts = MqttOptions::new("drift-cmd-pub", "127.0.0.1", port);
    pub_opts.set_keep_alive(Duration::from_secs(5));
    let (pub_client, mut pub_eventloop) = AsyncClient::new(pub_opts, 16);
    let pub_driver = tokio::spawn(async move {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(8);
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

    // Well-formed command — must reach the dispatcher.
    let valid = serde_json::json!({
        "request_id": "drift-cmd-1",
        "type": "status_request",
    });
    let valid_bytes = serde_json::to_vec(&valid).unwrap();
    let _: serde_json::Value =
        serde_json::from_slice(&valid_bytes).expect("valid envelope decodes as JSON");
    pub_client
        .publish(&command_topic, QoS::AtLeastOnce, false, valid_bytes)
        .await
        .expect("publish valid");

    let recv = tokio::time::timeout(Duration::from_secs(5), heartbeat_rx.recv()).await;
    assert!(
        matches!(recv, Ok(Some(()))),
        "valid status_request must round-trip through the JSON codec \
         and fire the heartbeat trigger"
    );

    // Malformed command — must NOT reach the dispatcher (the
    // handler short-circuits on a parse error and drops the
    // payload). We give the loop a short window to potentially
    // mis-route then assert no signal arrived.
    pub_client
        .publish(
            &command_topic,
            QoS::AtLeastOnce,
            false,
            b"not json at all".to_vec(),
        )
        .await
        .expect("publish malformed");

    let post_recv =
        tokio::time::timeout(Duration::from_millis(500), heartbeat_rx.recv()).await;
    assert!(
        post_recv.is_err(),
        "malformed command body must NOT trigger a heartbeat; the JSON \
         codec must short-circuit on parse failure"
    );

    pub_driver.abort();
    broker.shutdown().await;
}
