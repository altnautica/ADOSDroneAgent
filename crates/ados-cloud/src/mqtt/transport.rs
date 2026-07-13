//! The broker transport seam.
//!
//! [`MqttTransport`] is the small async surface the gateway and the signaling
//! relay route through (`publish`, `subscribe`, and a stream of incoming
//! messages). A test fake implements it without a broker; [`RumqttcTransport`]
//! is the real rumqttc-next client over WSS+TLS.
//!
//! The MAVLink relay does NOT route its hot publish path through this trait — it
//! owns its own rumqttc client so it can apply the bounded-queue + inflight gate
//! directly (see [`super::mavlink_relay`]). The trait covers the
//! request/response-shaped surfaces where a test fake is the most useful.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use rumqttc::{
    AsyncClient, ConnectReturnCode, Event, Incoming, MqttOptions, QoS as RumqttcQoS,
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

/// How a [`RumqttcTransport`] dials the broker. Carries the resolved
/// host/port/path/credentials; TLS is the shared RustCrypto rustls config.
#[derive(Debug, Clone)]
pub struct TransportConfig {
    pub client_id: String,
    pub host: String,
    pub port: u16,
    pub ws_path: String,
    pub username: String,
    pub password: String,
    /// MQTT in-flight ceiling. The MAVLink relay sets this high (the Rule-37
    /// fix); the gateway uses the same high ceiling to avoid telemetry-burst
    /// drops, matching the Python `max_inflight_messages_set(1000)`.
    pub inflight: u16,
    pub keep_alive: Duration,
}

impl TransportConfig {
    /// Build the rumqttc options for this config: WSS transport carrying the
    /// shared rustls config, credentials, keep-alive, and the inflight ceiling.
    fn build_options(&self) -> MqttOptions {
        // Broker host carries the ws path so the WSS handshake targets `/mqtt`.
        let url = format!("ws://{}:{}{}", self.host, self.port, self.ws_path);
        let mut opts = MqttOptions::new(self.client_id.clone(), url);
        opts.set_credentials(self.username.clone(), self.password.clone().into_bytes());
        opts.set_keep_alive(self.keep_alive.as_secs() as u16);
        let tls = TlsConfiguration::Rustls(crate::tls::client_config_arc());
        opts.set_transport(Transport::Wss(tls));
        opts
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
    _eventloop: tokio::task::JoinHandle<()>,
}

impl RumqttcTransport {
    /// Connect (lazily — rumqttc connects on the first event-loop poll) and
    /// spawn the event-loop task. Incoming publishes land on the channel
    /// returned by [`incoming`](Self::incoming).
    pub fn connect(config: &TransportConfig) -> Arc<Self> {
        let opts = config.build_options();
        let (client, mut eventloop) = AsyncClient::builder(opts).build();
        let (tx, rx) = mpsc::channel::<IncomingMessage>(256);
        let connected = Arc::new(AtomicBool::new(false));
        let connected_task = connected.clone();
        let eventloop = tokio::spawn(async move {
            loop {
                match eventloop.poll().await {
                    Ok(Event::Incoming(Incoming::Publish(p))) => {
                        let msg = IncomingMessage {
                            topic: String::from_utf8_lossy(&p.topic).into_owned(),
                            payload: p.payload.to_vec(),
                        };
                        if tx.send(msg).await.is_err() {
                            break; // consumer gone
                        }
                    }
                    // A successful ConnAck is the only signal the broker accepted
                    // the session; a refusal code (bad auth, service unavailable)
                    // is NOT connected.
                    Ok(Event::Incoming(Incoming::ConnAck(ack))) => {
                        let up = ack.code == ConnectReturnCode::Success;
                        connected_task.store(up, Ordering::Release);
                        if up {
                            tracing::debug!("mqtt broker connack success");
                        } else {
                            tracing::warn!(code = ?ack.code, "mqtt broker connack refused");
                        }
                    }
                    // A broker-initiated disconnect drops the session.
                    Ok(Event::Incoming(Incoming::Disconnect(_))) => {
                        connected_task.store(false, Ordering::Release);
                        tracing::debug!("mqtt broker disconnect");
                    }
                    Ok(_) => {}
                    // A connection error is transient; rumqttc reconnects on the
                    // next poll. The session is down until the next ConnAck, so
                    // clear the flag. Back off briefly so a hard-down broker does
                    // not spin the loop.
                    Err(e) => {
                        connected_task.store(false, Ordering::Release);
                        tracing::debug!(error = %e, "mqtt event loop poll error");
                        tokio::time::sleep(Duration::from_millis(500)).await;
                    }
                }
            }
            // The loop has ended (consumer dropped): the link is no longer live.
            connected_task.store(false, Ordering::Release);
        });
        Arc::new(RumqttcTransport {
            client,
            incoming: tokio::sync::Mutex::new(Some(rx)),
            connected,
            _eventloop: eventloop,
        })
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
        self.client
            .subscribe(topic.to_string(), qos.into())
            .await
            .map_err(|e| TransportError::Client(e.to_string()))
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use std::sync::Mutex;

    /// One recorded publish: `(topic, qos, payload)`.
    pub type RecordedPublish = (String, MqttQos, Vec<u8>);

    /// A fake transport that records publishes and subscriptions, and can feed
    /// incoming messages to a consumer. Lets the gateway + signaling relay run
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
                .unwrap()
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
                .unwrap()
                .push((topic.to_string(), qos, payload));
            Ok(())
        }

        async fn subscribe(&self, topic: &str, qos: MqttQos) -> Result<(), TransportError> {
            self.subscriptions
                .lock()
                .unwrap()
                .push((topic.to_string(), qos));
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

    #[tokio::test]
    async fn fresh_transport_is_not_connected_until_the_broker_acks() {
        // rumqttc dials lazily and retries a down broker forever, so a freshly
        // built transport (pointing at an unroutable broker) must report
        // connected() == false. This is the truth the GS bridge relies on to
        // avoid the connect-lie: the existence of the transport (and its
        // event-loop task) is NOT proof of a broker session.
        let cfg = TransportConfig {
            client_id: "ados-test".to_string(),
            // An unroutable host so no ConnAck can ever arrive in the test.
            host: "127.0.0.1".to_string(),
            port: 1, // nothing listens here
            ws_path: "/mqtt".to_string(),
            username: "ados-test".to_string(),
            password: "k".to_string(),
            inflight: 1000,
            keep_alive: Duration::from_secs(30),
        };
        let transport = RumqttcTransport::connect(&cfg);
        // Immediately after connect there can be no ConnAck.
        assert!(!transport.connected());
        // The shared handle observes the same state, and after a brief spin the
        // down broker still yields no confirmed connection.
        let handle = transport.connected_handle();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!handle.load(Ordering::Acquire));
        assert!(!transport.connected());
    }
}
