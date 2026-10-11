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
    /// The replay still owed to the current accepted session: `Some` from a
    /// `SessionUp` until every entry has been enqueued on the client, `None`
    /// otherwise. The replay is enqueued without awaiting, from the task that
    /// is the event loop's only poller, so an entry the full request channel
    /// refused waits here for the next polled event instead of wedging the
    /// loop that would drain the channel.
    pending_replay: Mutex<Option<Vec<(String, MqttQos)>>>,
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

    /// Start the replay for a freshly accepted session: every tracked topic is
    /// owed to it.
    fn begin_replay(&self) {
        let all = self.topics.lock().clone();
        *self.pending_replay.lock() = Some(all);
    }

    /// The session is gone: nothing is owed to it any more.
    fn cancel_replay(&self) {
        *self.pending_replay.lock() = None;
    }

    /// Enqueue as much of the owed replay as the client takes right now. Stops
    /// at the first refusal (a full request channel) and keeps the rest for
    /// the next call. Returns `true` when a replay was owed and is now fully
    /// enqueued.
    fn drive_replay<I: SubscribeIssuer + ?Sized>(&self, issuer: &I) -> bool {
        let mut pending = self.pending_replay.lock();
        let Some(owed) = pending.as_mut() else {
            return false;
        };
        let mut done = 0;
        for (topic, qos) in owed.iter() {
            match issuer.try_issue(topic, *qos) {
                Ok(()) => done += 1,
                Err(e) => {
                    tracing::debug!(topic = %topic, error = %e, "mqtt resubscribe deferred");
                    break;
                }
            }
        }
        owed.drain(..done);
        if owed.is_empty() {
            *pending = None;
            true
        } else {
            false
        }
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
/// production impl; a test recorder proves the replay without a broker. The
/// call never awaits: it runs on the event loop's only poller.
trait SubscribeIssuer: Send + Sync {
    fn try_issue(&self, topic: &str, qos: MqttQos) -> Result<(), TransportError>;
}

impl SubscribeIssuer for AsyncClient {
    fn try_issue(&self, topic: &str, qos: MqttQos) -> Result<(), TransportError> {
        self.try_subscribe(topic.to_string(), qos.into())
            .map_err(|e| TransportError::Client(e.to_string()))
    }
}

/// Apply one session event: drive the confirmed-connection flag, REPLAY the
/// subscription set onto a freshly accepted session, and fan an inbound publish
/// out to the consumer. Returns `false` when the event loop must stop (the
/// consumer dropped its receiver).
///
/// Every call is one successfully polled event, so an owed replay is retried
/// here first. The link reads connected only once the whole replay has been
/// enqueued, so a lane never sees a session whose subscriptions are missing.
async fn apply_session_event<I: SubscribeIssuer + ?Sized>(
    event: SessionEvent,
    connected: &AtomicBool,
    subs: &SubscriptionSet,
    issuer: &I,
    tx: &mpsc::Sender<IncomingMessage>,
) -> bool {
    match event {
        SessionEvent::SessionUp => {
            // The broker granted a fresh session with no subscriptions, so
            // every topic this transport holds is owed to it.
            connected.store(false, Ordering::Release);
            subs.begin_replay();
            tracing::debug!("mqtt broker connack success");
        }
        SessionEvent::SessionRefused => {
            connected.store(false, Ordering::Release);
            subs.cancel_replay();
            tracing::warn!("mqtt broker connack refused");
            return true;
        }
        SessionEvent::SessionDown => {
            connected.store(false, Ordering::Release);
            subs.cancel_replay();
            tracing::debug!("mqtt broker session down");
            return true;
        }
        SessionEvent::Message(_) | SessionEvent::Ignored => {}
    }
    if subs.drive_replay(issuer) {
        connected.store(true, Ordering::Release);
    }
    match event {
        SessionEvent::Message(msg) => tx.send(msg).await.is_ok(),
        _ => true,
    }
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
        self.client
            .subscribe(topic.to_string(), qos.into())
            .await
            .map_err(|e| TransportError::Client(e.to_string()))
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
mod tests;
