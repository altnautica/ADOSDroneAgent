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

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use rumqttc::{
    AsyncClient, Event, Incoming, MqttOptions, QoS as RumqttcQoS, TlsConfiguration, Transport,
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
                    Ok(_) => {}
                    // A connection error is transient; rumqttc reconnects on the
                    // next poll. Back off briefly so a hard-down broker does not
                    // spin the loop.
                    Err(e) => {
                        tracing::debug!(error = %e, "mqtt event loop poll error");
                        tokio::time::sleep(Duration::from_millis(500)).await;
                    }
                }
            }
        });
        Arc::new(RumqttcTransport {
            client,
            incoming: tokio::sync::Mutex::new(Some(rx)),
            _eventloop: eventloop,
        })
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
}
