use super::*;
use std::sync::atomic::Ordering;

#[test]
fn qos_maps_to_rumqttc() {
    assert_eq!(
        RumqttcQoS::from(MqttQos::AtMostOnce),
        RumqttcQoS::AtMostOnce
    );
    assert_eq!(
        RumqttcQoS::from(MqttQos::AtLeastOnce),
        RumqttcQoS::AtLeastOnce
    );
}

fn test_config(host: &str, port: u16, wire: BrokerWire) -> TransportConfig {
    TransportConfig {
        client_id: "ados-test".to_string(),
        host: host.to_string(),
        port,
        wire,
        ws_path: "/mqtt".to_string(),
        username: "ados-test".to_string(),
        password: "k".to_string(),
        inflight: 1000,
        keep_alive: Duration::from_secs(30),
    }
}

/// The dial must actually reach the configured broker address. A broker
/// target built from a URL string is a TCP broker whose host is the whole
/// URL, and rumqttc refuses a WSS transport over it before opening a socket.
async fn assert_dial_reaches_listener(wire: BrokerWire) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let opts = test_config("127.0.0.1", port, wire)
        .build_options()
        .expect("a well-formed broker target");
    let (_client, mut eventloop) = AsyncClient::builder(opts).build();
    let dial = tokio::spawn(async move {
        let _ = eventloop.poll().await;
    });
    let accepted = tokio::time::timeout(Duration::from_secs(5), listener.accept()).await;
    dial.abort();
    assert!(
        matches!(accepted, Ok(Ok(_))),
        "the {wire:?} dial never opened a connection to the broker port"
    );
}

#[tokio::test]
async fn a_wss_dial_reaches_the_configured_broker_port() {
    assert_dial_reaches_listener(BrokerWire::Wss).await;
}

#[tokio::test]
async fn a_tls_dial_reaches_the_configured_broker_port() {
    assert_dial_reaches_listener(BrokerWire::Tls).await;
}

#[tokio::test]
async fn fresh_transport_is_not_connected_until_the_broker_acks() {
    // rumqttc dials lazily and retries a down broker forever, so a freshly
    // built transport (pointing at an unroutable broker) must report
    // connected() == false. This is the truth the GS bridge relies on to
    // avoid the connect-lie: the existence of the transport (and its
    // event-loop task) is NOT proof of a broker session.
    // Port 1: nothing listens there, so no ConnAck can ever arrive.
    let transport =
        RumqttcTransport::connect(&test_config("127.0.0.1", 1, BrokerWire::Wss)).unwrap();
    // Immediately after connect there can be no ConnAck.
    assert!(!transport.connected());
    // The shared handle observes the same state, and after a brief spin the
    // down broker still yields no confirmed connection.
    let handle = transport.connected_handle();
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(!handle.load(Ordering::Acquire));
    assert!(!transport.connected());
}

/// A [`SubscribeIssuer`] that records what was re-issued, so the replay is
/// observable without a broker. `refuse` makes the next N calls fail the
/// way a full client request channel does.
#[derive(Default)]
struct RecordingIssuer {
    issued: Mutex<Vec<(String, MqttQos)>>,
    refuse: std::sync::atomic::AtomicUsize,
}

impl RecordingIssuer {
    /// Drain what has been issued since the last call.
    fn taken(&self) -> Vec<(String, MqttQos)> {
        std::mem::take(&mut *self.issued.lock())
    }
}

impl SubscribeIssuer for RecordingIssuer {
    fn try_issue(&self, topic: &str, qos: MqttQos) -> Result<(), TransportError> {
        let refused = self
            .refuse
            .try_update(Ordering::AcqRel, Ordering::Acquire, |n| n.checked_sub(1))
            .is_ok();
        if refused {
            return Err(TransportError::Client("request channel full".into()));
        }
        self.issued.lock().push((topic.to_string(), qos));
        Ok(())
    }
}

#[tokio::test]
async fn apply_session_event_does_not_await_issuer() {
    // The client request channel is full when the session comes up: the
    // replay must not park the event loop's only poller on it.
    let subs = SubscriptionSet::default();
    subs.record("ados/dev1/mavlink/rx", MqttQos::AtMostOnce);
    subs.record("ados/dev1/webrtc/offer", MqttQos::AtLeastOnce);
    let issuer = RecordingIssuer::default();
    issuer.refuse.store(1, Ordering::Release);
    let connected = AtomicBool::new(false);
    let (tx, _rx) = mpsc::channel::<IncomingMessage>(4);

    let alive = tokio::time::timeout(
        Duration::from_secs(1),
        apply_session_event(SessionEvent::SessionUp, &connected, &subs, &issuer, &tx),
    )
    .await
    .expect("the session-up event returns without waiting on the channel");
    assert!(alive);
    // Nothing enqueued yet, so the link does not read up.
    assert!(issuer.taken().is_empty());
    assert!(!connected.load(Ordering::Acquire));

    // The next polled event drained the channel: the owed replay goes out
    // and only then does the link read connected.
    apply_session_event(SessionEvent::Ignored, &connected, &subs, &issuer, &tx).await;
    assert_eq!(
        issuer.taken(),
        vec![
            ("ados/dev1/mavlink/rx".to_string(), MqttQos::AtMostOnce),
            ("ados/dev1/webrtc/offer".to_string(), MqttQos::AtLeastOnce),
        ]
    );
    assert!(connected.load(Ordering::Acquire));

    // Later events owe nothing and issue nothing.
    apply_session_event(SessionEvent::Ignored, &connected, &subs, &issuer, &tx).await;
    assert!(issuer.taken().is_empty());
    assert!(connected.load(Ordering::Acquire));
}

#[tokio::test]
async fn a_session_drop_cancels_the_owed_replay() {
    let subs = SubscriptionSet::default();
    subs.record("ados/dev1/mavlink/rx", MqttQos::AtMostOnce);
    let issuer = RecordingIssuer::default();
    issuer.refuse.store(1, Ordering::Release);
    let connected = AtomicBool::new(false);
    let (tx, _rx) = mpsc::channel::<IncomingMessage>(4);
    apply_session_event(SessionEvent::SessionUp, &connected, &subs, &issuer, &tx).await;
    apply_session_event(SessionEvent::SessionDown, &connected, &subs, &issuer, &tx).await;
    apply_session_event(SessionEvent::Ignored, &connected, &subs, &issuer, &tx).await;
    assert!(issuer.taken().is_empty());
    assert!(!connected.load(Ordering::Acquire));
}

fn connack(code: ConnectReturnCode) -> Event {
    Event::Incoming(Incoming::ConnAck(rumqttc::ConnAck {
        session_present: false,
        code,
        properties: None,
    }))
}

#[test]
fn a_connack_after_a_disconnect_is_a_fresh_session_not_a_resumed_one() {
    // The boundary that feeds the replay: a broker restart shows up as
    // Disconnect then ConnAck(Success), and BOTH acks must read SessionUp
    // so the second one triggers a replay rather than being treated as a
    // continuation of the first session.
    assert_eq!(
        classify_event(&connack(ConnectReturnCode::Success)),
        SessionEvent::SessionUp
    );
    assert_eq!(
        classify_event(&Event::Incoming(Incoming::Disconnect(
            rumqttc::Disconnect {
                reason_code: rumqttc::DisconnectReasonCode::NormalDisconnection,
                properties: None,
            }
        ))),
        SessionEvent::SessionDown
    );
    // A refusal is not a session, so nothing may be replayed onto it.
    assert_eq!(
        classify_event(&connack(ConnectReturnCode::BadUserNamePassword)),
        SessionEvent::SessionRefused
    );
}

#[tokio::test]
async fn a_broker_reconnect_reissues_every_subscription() {
    // The GCS->FC uplink lives entirely on a subscription. The broker
    // discards it on every fresh session, so without this replay
    // `ados/{id}/mavlink/rx` (operator commands INTO the flight controller)
    // dies at the first tunnel blip while `mavlink/tx` keeps flowing and
    // mqttConnected still reports true.
    let subs = SubscriptionSet::default();
    subs.record("ados/dev1/mavlink/rx", MqttQos::AtMostOnce);
    subs.record("ados/dev1/webrtc/offer", MqttQos::AtLeastOnce);
    let issuer = RecordingIssuer::default();
    let connected = AtomicBool::new(false);
    let (tx, _rx) = mpsc::channel::<IncomingMessage>(4);

    // First accepted session: the set is issued and the link reads up.
    let alive = apply_session_event(
        classify_event(&connack(ConnectReturnCode::Success)),
        &connected,
        &subs,
        &issuer,
        &tx,
    )
    .await;
    assert!(alive);
    assert!(connected.load(Ordering::Acquire));
    assert_eq!(
        issuer.taken(),
        vec![
            ("ados/dev1/mavlink/rx".to_string(), MqttQos::AtMostOnce),
            ("ados/dev1/webrtc/offer".to_string(), MqttQos::AtLeastOnce),
        ]
    );

    // The broker goes away (restart / keep-alive timeout).
    apply_session_event(SessionEvent::SessionDown, &connected, &subs, &issuer, &tx).await;
    assert!(!connected.load(Ordering::Acquire));
    assert!(
        issuer.taken().is_empty(),
        "a disconnect issues nothing; there is no session to subscribe on"
    );

    // rumqttc reconnects: the WHOLE set is re-issued, so the uplink is live
    // again rather than silently dead.
    apply_session_event(
        classify_event(&connack(ConnectReturnCode::Success)),
        &connected,
        &subs,
        &issuer,
        &tx,
    )
    .await;
    assert!(connected.load(Ordering::Acquire));
    assert_eq!(
        issuer.taken(),
        vec![
            ("ados/dev1/mavlink/rx".to_string(), MqttQos::AtMostOnce),
            ("ados/dev1/webrtc/offer".to_string(), MqttQos::AtLeastOnce),
        ]
    );

    // A refused reconnect (rotated key) replays nothing and reads down.
    apply_session_event(
        classify_event(&connack(ConnectReturnCode::NotAuthorized)),
        &connected,
        &subs,
        &issuer,
        &tx,
    )
    .await;
    assert!(!connected.load(Ordering::Acquire));
    assert!(issuer.taken().is_empty());
}

#[tokio::test]
async fn a_resubscribe_at_a_new_qos_replaces_rather_than_duplicates() {
    let subs = SubscriptionSet::default();
    subs.record("ados/dev1/mavlink/rx", MqttQos::AtMostOnce);
    subs.record("ados/dev1/mavlink/rx", MqttQos::AtLeastOnce);
    let issuer = RecordingIssuer::default();
    let connected = AtomicBool::new(false);
    let (tx, _rx) = mpsc::channel::<IncomingMessage>(4);
    apply_session_event(SessionEvent::SessionUp, &connected, &subs, &issuer, &tx).await;
    assert_eq!(
        issuer.taken(),
        vec![("ados/dev1/mavlink/rx".to_string(), MqttQos::AtLeastOnce)]
    );
}

#[tokio::test]
async fn a_dropped_consumer_ends_the_loop() {
    // The one event that must stop the event loop: the receiver is gone, so
    // there is nothing to fan messages out to.
    let subs = SubscriptionSet::default();
    let issuer = RecordingIssuer::default();
    let connected = AtomicBool::new(false);
    let (tx, rx) = mpsc::channel::<IncomingMessage>(1);
    drop(rx);
    let alive = apply_session_event(
        SessionEvent::Message(IncomingMessage {
            topic: "ados/dev1/mavlink/rx".to_string(),
            payload: vec![1],
        }),
        &connected,
        &subs,
        &issuer,
        &tx,
    )
    .await;
    assert!(!alive);
}
