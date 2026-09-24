//! The broker transport seam.
//!
//! [`MqttTransport`] is the small async surface the plugin publish lane and the signaling
//! relay route through (`publish`, `subscribe`, and a stream of incoming
//! messages). A test fake implements it without a broker; [`RumqttcTransport`]
//! is the real rumqttc-next client over WSS+TLS.
//!
//! The MAVLink relay does NOT route its hot publish path through this trait — it
//! owns its own rumqttc client so it can apply the bounded-queue + inflight gate
//! directly (see [`super::mavlink_relay`]). The trait covers the
//! request/response-shaped surfaces where a test fake is the most useful.
//!
//! Every consumer MUST take its subscriptions through [`MqttTransport::subscribe`]
//! rather than through the raw [`RumqttcTransport::client`], because that is the
//! one path that records the topic for replay on the next accepted session. A
//! subscribe issued straight at the client is held by the broker only until the
//! first reconnect and then silently vanishes.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use parking_lot::Mutex;
use rumqttc::{
    AsyncClient, Broker, ConnectReturnCode, Event, Incoming, MqttOptions, QoS as RumqttcQoS,
    TlsConfiguration, Transport,
};
use tokio::sync::mpsc;

/// Publish quality of service. Maps to the broker's q0 / q1.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MqttQos {
    AtMostOnce,
    AtLeastOnce,
}

impl From<MqttQos> for RumqttcQoS {
    fn from(q: MqttQos) -> Self {
        match q {
            MqttQos::AtMostOnce => RumqttcQoS::AtMostOnce,
            MqttQos::AtLeastOnce => RumqttcQoS::AtLeastOnce,
        }
    }
}

/// One inbound message delivered to a subscriber.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IncomingMessage {
    pub topic: String,
    pub payload: Vec<u8>,
}

/// A transport failure (connect, publish, or subscribe).
#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    #[error("mqtt client error: {0}")]
    Client(String),
    #[error("transport closed")]
    Closed,
}

/// The subscriptions a connection must hold for its whole life.
///
/// rumqttc asks the broker for a clean session on every dial, so a reconnect
/// gets a FRESH broker session with an empty subscription list and rumqttc
/// replays only pending publishes. A consumer that subscribes once before
/// entering its loop therefore loses every subscription at the first tunnel
/// blip or keep-alive timeout: on the MAVLink relay that means
/// `ados/{id}/mavlink/tx` (telemetry out of the aircraft) keeps flowing while
/// `ados/{id}/mavlink/rx` (the operator's commands INTO the flight controller)
/// is permanently dead, with `connected()` still reporting true. A one-way
/// flight-control link that advertises itself as healthy.
#[derive(Debug, Default)]
struct SubscriptionSet {
    topics: Mutex<Vec<(String, MqttQos)>>,
}

impl SubscriptionSet {
    /// Record a subscription, replacing any earlier QoS for the same topic so a
    /// re-subscribe at a different QoS cannot double-issue on the next replay.
    fn record(&self, topic: &str, qos: MqttQos) {
        let mut topics = self.topics.lock();
        match topics.iter_mut().find(|(t, _)| t == topic) {
            Some(entry) => entry.1 = qos,
            None => topics.push((topic.to_string(), qos)),
        }
    }

    /// Every tracked subscription, in the order it was first taken. Clones out
    /// so the lock is never held across the replay's awaits.
    fn snapshot(&self) -> Vec<(String, MqttQos)> {
        self.topics.lock().clone()
    }
}

/// What one polled broker event means to this transport's own bookkeeping.
///
/// The rumqttc event is reduced to this the moment it is polled, so the part
/// carrying the logic — the confirmed-session flag and the subscription replay —
/// is driven by a value a test can build without a broker.
#[derive(Debug, Clone, PartialEq, Eq)]
enum SessionEvent {
    /// The broker ACCEPTED the session. Every tracked subscription is re-issued
    /// here, including on the first connect.
    SessionUp,
    /// The broker REFUSED the session (bad auth, unavailable, banned). Not a
    /// session, so nothing is replayed onto it.
    SessionRefused,
    /// The session is gone: a broker disconnect, a poll error, or the loop end.
    SessionDown,
    /// An inbound publish to fan out to the consumer.
    Message(IncomingMessage),
    /// Outgoing acks, pings, and everything else with no bookkeeping.
    Ignored,
}

/// Reduce a polled rumqttc event to the transport's own session vocabulary.
fn classify_event(event: &Event) -> SessionEvent {
    match event {
        Event::Incoming(Incoming::Publish(p)) => SessionEvent::Message(IncomingMessage {
            topic: String::from_utf8_lossy(&p.topic).into_owned(),
            payload: p.payload.to_vec(),
        }),
        // A successful ConnAck is the only signal the broker accepted the
        // session; a refusal code (bad auth, service unavailable) is NOT
        // connected and must not be replayed onto.
        Event::Incoming(Incoming::ConnAck(ack)) => {
            if ack.code == ConnectReturnCode::Success {
                SessionEvent::SessionUp
            } else {
                SessionEvent::SessionRefused
            }
        }
        Event::Incoming(Incoming::Disconnect(_)) => SessionEvent::SessionDown,
        _ => SessionEvent::Ignored,
    }
}

/// The seam a subscription replay is issued through. [`AsyncClient`] is the
/// production impl; a test recorder proves the replay without a broker.
#[async_trait]
trait SubscribeIssuer: Send + Sync {
    async fn issue(&self, topic: &str, qos: MqttQos) -> Result<(), TransportError>;
}

#[async_trait]
impl SubscribeIssuer for AsyncClient {
    async fn issue(&self, topic: &str, qos: MqttQos) -> Result<(), TransportError> {
        self.subscribe(topic.to_string(), qos.into())
            .await
            .map_err(|e| TransportError::Client(e.to_string()))
    }
}

/// Apply one session event: drive the confirmed-connection flag, REPLAY the
/// subscription set onto a freshly accepted session, and fan an inbound publish
/// out to the consumer. Returns `false` when the event loop must stop (the
/// consumer dropped its receiver).
async fn apply_session_event<I: SubscribeIssuer + ?Sized>(
    event: SessionEvent,
    connected: &AtomicBool,
    subs: &SubscriptionSet,
    issuer: &I,
    tx: &mpsc::Sender<IncomingMessage>,
) -> bool {
    match event {
        SessionEvent::Message(msg) => return tx.send(msg).await.is_ok(),
        SessionEvent::SessionUp => {
            connected.store(true, Ordering::Release);
            // The broker granted a fresh session with no subscriptions, so
            // every topic this transport holds is re-issued now. Best-effort
            // per topic: one failed re-issue must not abandon the rest.
            for (topic, qos) in subs.snapshot() {
                if let Err(e) = issuer.issue(&topic, qos).await {
                    tracing::warn!(topic = %topic, error = %e, "mqtt resubscribe failed");
                }
            }
            tracing::debug!("mqtt broker connack success");
        }
        SessionEvent::SessionRefused => {
            connected.store(false, Ordering::Release);
            tracing::warn!("mqtt broker connack refused");
        }
        SessionEvent::SessionDown => {
            connected.store(false, Ordering::Release);
            tracing::debug!("mqtt broker session down");
        }
        SessionEvent::Ignored => {}
    }
    true
}

/// The async broker surface. `publish` and `subscribe` are request-shaped; the
/// incoming stream is drained by the consumer's own task.
#[async_trait]
pub trait MqttTransport: Send + Sync {
    /// Publish a payload to a topic at the given QoS (non-retained).
    async fn publish(
        &self,
        topic: &str,
        qos: MqttQos,
        payload: Vec<u8>,
    ) -> Result<(), TransportError>;

    /// Non-blocking publish: enqueue a payload for delivery without awaiting the
    /// broker. Returns immediately; a full outgoing queue is an error (the caller
    /// drops the payload rather than blocking). For a fire-and-forget lossy stream
    /// (a live detection tee, like the MAVLink telemetry topic) where recency
    /// beats completeness and the producer must never stall on a slow uplink.
    fn try_publish(
        &self,
        topic: &str,
        qos: MqttQos,
        payload: Vec<u8>,
    ) -> Result<(), TransportError>;

    /// Subscribe to a topic at the given QoS.
    async fn subscribe(&self, topic: &str, qos: MqttQos) -> Result<(), TransportError>;
}

/// How a [`RumqttcTransport`] reaches the broker: MQTT over a TLS WebSocket (the
/// managed broker's tunnel) or MQTT over TLS on a plain TCP port (a self-hosted
/// broker on 8883). Both carry the shared ring-backed rustls config.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrokerWire {
    /// `wss://host:port<ws_path>`.
    Wss,
    /// MQTT over TLS straight on `host:port`.
    Tls,
}

/// Fixed delay before the event loop re-dials a broker that refused or dropped
/// the session. A recovery loop retries on a fixed 2-5 s cadence with no cap;
/// every attempt is a full DNS + TCP + TLS (+ WebSocket) handshake, so a tighter
/// loop only burns a metered uplink against a broker that is down.
const RECONNECT_DELAY: Duration = Duration::from_secs(3);

/// How a [`RumqttcTransport`] dials the broker. Carries the resolved
/// host/port/path/credentials; TLS is the shared ring-backed rustls config.
#[derive(Debug, Clone)]
pub struct TransportConfig {
    pub client_id: String,
    pub host: String,
    pub port: u16,
    pub wire: BrokerWire,
    /// The WebSocket path the broker serves MQTT on (`Wss` only).
    pub ws_path: String,
    pub username: String,
    pub password: String,
    /// MQTT in-flight ceiling for at-least-once publishes. The relays set this
    /// high so a telemetry burst is not throttled by the client.
    pub inflight: u16,
    pub keep_alive: Duration,
}

impl TransportConfig {
    /// Build the rumqttc options for this config: the broker target for the
    /// wire, TLS carrying the shared rustls config, credentials, keep-alive, and
    /// the inflight ceiling.
    ///
    /// The WSS target must be built with [`Broker::websocket`]: handing rumqttc a
    /// URL string makes a plain TCP broker whose host is the whole URL, and a WSS
    /// transport over a TCP broker refuses every dial.
    fn build_options(&self) -> Result<MqttOptions, TransportError> {
        let tls = TlsConfiguration::Rustls(crate::tls::client_config_arc());
        let (broker, transport) = match self.wire {
            BrokerWire::Wss => {
                let url = format!("ws://{}:{}{}", self.host, self.port, self.ws_path);
                let broker = Broker::websocket(url.clone())
                    .map_err(|e| TransportError::Client(format!("broker url {url}: {e}")))?;
                (broker, Transport::Wss(tls))
            }
            BrokerWire::Tls => (
                Broker::tcp(self.host.clone(), self.port),
                Transport::Tls(tls),
            ),
        };
        let mut opts = MqttOptions::new(self.client_id.clone(), broker);
        opts.set_credentials(self.username.clone(), self.password.clone().into_bytes());
        opts.set_keep_alive(self.keep_alive.as_secs() as u16);
        opts.set_outgoing_inflight_upper_limit(self.inflight);
        opts.set_transport(transport);
        Ok(opts)
    }
}

/// The real broker transport: a rumqttc-next async client over WSS+TLS. The
/// event loop runs on its own task, fanning incoming publishes onto an mpsc the
/// consumer drains via [`incoming`](Self::incoming).
pub struct RumqttcTransport {
    client: AsyncClient,
    incoming: tokio::sync::Mutex<Option<mpsc::Receiver<IncomingMessage>>>,
    /// The CONFIRMED broker connection state, driven by the event loop: set
    /// `true` only on a successful `ConnAck` and back to `false` on a
    /// `Disconnect`, a poll error, or the loop ending. A consumer must read this
    /// to know the link is live — `connect()` returns immediately because
    /// rumqttc dials lazily and retries a down broker forever, so the existence
    /// of this transport (or of its event-loop task) is NOT proof of a session.
    connected: Arc<AtomicBool>,
    /// Every subscription taken on this transport, replayed on each accepted
    /// session. Without it a reconnect silently drops the inbound half of the
    /// link (see [`SubscriptionSet`]).
    subs: Arc<SubscriptionSet>,
    _eventloop: tokio::task::JoinHandle<()>,
}

impl RumqttcTransport {
    /// Connect (lazily — rumqttc connects on the first event-loop poll) and
    /// spawn the event-loop task. Incoming publishes land on the channel
    /// returned by [`incoming`](Self::incoming). Fails only when the dial config
    /// cannot describe a broker at all (an unparseable host).
    pub fn connect(config: &TransportConfig) -> Result<Arc<Self>, TransportError> {
        let opts = config.build_options()?;
        let (client, mut eventloop) = AsyncClient::builder(opts).build();
        let (tx, rx) = mpsc::channel::<IncomingMessage>(256);
        let connected = Arc::new(AtomicBool::new(false));
        let connected_task = connected.clone();
        let subs = Arc::new(SubscriptionSet::default());
        let subs_task = subs.clone();
        // The replay is issued through the same client the consumer publishes
        // on, so a re-subscribe rides the connection that just came up.
        let replay_client = client.clone();
        let eventloop = tokio::spawn(async move {
            loop {
                let event = match eventloop.poll().await {
                    Ok(event) => classify_event(&event),
                    // A connection error is transient; rumqttc reconnects on the
                    // next poll. The session is down until the next ConnAck, so
                    // clear the flag and wait the fixed recovery interval before
                    // the next dial.
                    Err(e) => {
                        tracing::debug!(error = %e, "mqtt event loop poll error");
                        tokio::time::sleep(RECONNECT_DELAY).await;
                        SessionEvent::SessionDown
                    }
                };
                if !apply_session_event(event, &connected_task, &subs_task, &replay_client, &tx)
                    .await
                {
                    break; // consumer gone
                }
            }
            // The loop has ended (consumer dropped): the link is no longer live.
            connected_task.store(false, Ordering::Release);
        });
        Ok(Arc::new(RumqttcTransport {
            client,
            incoming: tokio::sync::Mutex::new(Some(rx)),
            connected,
            subs,
            _eventloop: eventloop,
        }))
    }

    /// Whether the broker session is currently CONFIRMED up (a successful
    /// `ConnAck` was seen and no `Disconnect`/error has since dropped it). This
    /// is the truthful liveness signal — not the existence of the transport or
    /// its event-loop task, which persist across a hard-down broker because
    /// rumqttc retries forever.
    pub fn connected(&self) -> bool {
        self.connected.load(Ordering::Acquire)
    }

    /// A clonable handle to the confirmed-connection flag, so a consumer that
    /// outlives a borrow of the transport (the relay task hands it to its
    /// supervisor) can observe the live state without holding the transport.
    pub fn connected_handle(&self) -> Arc<AtomicBool> {
        self.connected.clone()
    }

    /// Take the incoming-message receiver. Returns `None` after the first call
    /// (there is a single fan-out channel per connection).
    pub async fn take_incoming(&self) -> Option<mpsc::Receiver<IncomingMessage>> {
        self.incoming.lock().await.take()
    }

    /// The underlying client, for callers that publish bytes directly (the
    /// MAVLink relay's bounded publisher).
    pub fn client(&self) -> &AsyncClient {
        &self.client
    }
}

#[async_trait]
impl MqttTransport for RumqttcTransport {
    async fn publish(
        &self,
        topic: &str,
        qos: MqttQos,
        payload: Vec<u8>,
    ) -> Result<(), TransportError> {
        self.client
            .publish(topic.to_string(), qos.into(), false, payload)
            .await
            .map_err(|e| TransportError::Client(e.to_string()))
    }

    fn try_publish(
        &self,
        topic: &str,
        qos: MqttQos,
        payload: Vec<u8>,
    ) -> Result<(), TransportError> {
        // Non-blocking: rumqttc enqueues onto its bounded request channel and
        // returns immediately, erroring when the channel is full (drop-on-busy).
        self.client
            .try_publish(topic.to_string(), qos.into(), false, payload)
            .map_err(|e| TransportError::Client(e.to_string()))
    }

    async fn subscribe(&self, topic: &str, qos: MqttQos) -> Result<(), TransportError> {
        // RECORD before issuing: a subscribe taken while the session is dying
        // must still be replayed on the next accepted session, otherwise the
        // topic is lost even though the caller saw an Ok.
        self.subs.record(topic, qos);
        self.client.issue(topic, qos).await
    }
}

/// A shared transport is a transport: a lane that owns an `Arc` of the session
/// (the signaling relay) routes through it unchanged.
#[async_trait]
impl<T: MqttTransport + ?Sized> MqttTransport for Arc<T> {
    async fn publish(
        &self,
        topic: &str,
        qos: MqttQos,
        payload: Vec<u8>,
    ) -> Result<(), TransportError> {
        (**self).publish(topic, qos, payload).await
    }

    fn try_publish(
        &self,
        topic: &str,
        qos: MqttQos,
        payload: Vec<u8>,
    ) -> Result<(), TransportError> {
        (**self).try_publish(topic, qos, payload)
    }

    async fn subscribe(&self, topic: &str, qos: MqttQos) -> Result<(), TransportError> {
        (**self).subscribe(topic, qos).await
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;

    /// One recorded publish: `(topic, qos, payload)`.
    pub type RecordedPublish = (String, MqttQos, Vec<u8>);

    /// A fake transport that records publishes and subscriptions, and can feed
    /// incoming messages to a consumer. Lets the signaling relay and the bearers run
    /// in a unit test without a broker.
    #[derive(Default)]
    pub struct FakeTransport {
        pub publishes: Mutex<Vec<RecordedPublish>>,
        pub subscriptions: Mutex<Vec<(String, MqttQos)>>,
    }

    #[async_trait]
    impl MqttTransport for FakeTransport {
        async fn publish(
            &self,
            topic: &str,
            qos: MqttQos,
            payload: Vec<u8>,
        ) -> Result<(), TransportError> {
            self.publishes
                .lock()
                .push((topic.to_string(), qos, payload));
            Ok(())
        }

        fn try_publish(
            &self,
            topic: &str,
            qos: MqttQos,
            payload: Vec<u8>,
        ) -> Result<(), TransportError> {
            self.publishes
                .lock()
                .push((topic.to_string(), qos, payload));
            Ok(())
        }

        async fn subscribe(&self, topic: &str, qos: MqttQos) -> Result<(), TransportError> {
            self.subscriptions.lock().push((topic.to_string(), qos));
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
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
    /// observable without a broker.
    #[derive(Default)]
    struct RecordingIssuer {
        issued: Mutex<Vec<(String, MqttQos)>>,
    }

    impl RecordingIssuer {
        /// Drain what has been issued since the last call.
        fn taken(&self) -> Vec<(String, MqttQos)> {
            std::mem::take(&mut *self.issued.lock())
        }
    }

    #[async_trait]
    impl SubscribeIssuer for RecordingIssuer {
        async fn issue(&self, topic: &str, qos: MqttQos) -> Result<(), TransportError> {
            self.issued.lock().push((topic.to_string(), qos));
            Ok(())
        }
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
}
