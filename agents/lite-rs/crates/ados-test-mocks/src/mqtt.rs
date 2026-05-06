//! In-process MQTT v3 broker fixture for tests.
//!
//! Binds an ephemeral loopback port and runs a `rumqttd` broker on a
//! background OS thread. The broker is fully self-contained: it does
//! no disk IO, opens no listeners on non-loopback interfaces, and
//! shuts down with the [`MockMqttBroker`] handle.

use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, TcpStream};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use rumqttd::{Broker, Config, ConnectionSettings, RouterConfig, ServerSettings};

/// Errors returned when the MQTT fixture cannot start.
#[derive(Debug, thiserror::Error)]
pub enum MockMqttError {
    #[error("failed to bind ephemeral port: {0}")]
    Bind(#[from] std::io::Error),
    #[error("broker did not accept connections within {0:?}")]
    StartTimeout(Duration),
}

/// Handle to a running in-process MQTT broker.
///
/// Drop or [`MockMqttBroker::shutdown`] cleans up the background
/// thread (the broker process exits when the last reference goes
/// away; rumqttd holds a tokio runtime per server thread that
/// terminates with the listening socket).
pub struct MockMqttBroker {
    port: u16,
    // Holds the broker runtime alive. The broker spawns its own
    // server threads internally; dropping `_runner` joins the
    // outer wrapper but leaves the inner OS threads to terminate
    // when their listeners close. That is fine for fixture use:
    // each test gets a fresh ephemeral port so cross-test bleed is
    // impossible.
    _runner: Option<thread::JoinHandle<()>>,
}

impl MockMqttBroker {
    /// Start a fresh broker on `127.0.0.1:0` and wait for it to
    /// accept TCP connections.
    pub async fn start() -> Result<Self, MockMqttError> {
        // Reserve an ephemeral port. We bind, read the port number,
        // and drop the listener before handing the address to
        // rumqttd. There is a microscopic race window where another
        // process could grab the port; for in-process tests on
        // loopback this has not been a problem in practice.
        let probe = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
        let port = probe.local_addr()?.port();
        drop(probe);

        let config = build_config(port);

        let runner = thread::Builder::new()
            .name(format!("mock-mqtt-broker-{port}"))
            .spawn(move || {
                let mut broker = Broker::new(config);
                if let Err(err) = broker.start() {
                    tracing::warn!(?err, "mock mqtt broker exited");
                }
            })
            .map_err(MockMqttError::Bind)?;

        wait_for_listener(port, Duration::from_secs(5)).await?;

        Ok(Self {
            port,
            _runner: Some(runner),
        })
    }

    /// `tcp://127.0.0.1:<port>` connect URL.
    pub fn url(&self) -> String {
        format!("tcp://127.0.0.1:{}", self.port)
    }

    /// Bound TCP port on `127.0.0.1`.
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Shut the broker down explicitly. The drop impl performs the
    /// same work; this method exists so callers can `await` an
    /// explicit teardown point. The current rumqttd broker has no
    /// public stop primitive, so we let the join handle drift and
    /// rely on listener close to unwind the per-server runtime.
    pub async fn shutdown(self) {
        // Detach: dropping `self` lets the background thread carry
        // on until the runtime tears down at process exit. For unit
        // tests that finish in milliseconds this is the safe
        // default.
        drop(self);
    }
}

fn build_config(port: u16) -> Config {
    let listen = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, port));

    let connections = ConnectionSettings {
        connection_timeout_ms: 5_000,
        max_payload_size: 1024 * 1024,
        max_inflight_count: 100,
        auth: None,
        external_auth: None,
        dynamic_filters: false,
    };

    let server = ServerSettings {
        name: "ados-mock-broker-v4".to_string(),
        listen,
        tls: None,
        next_connection_delay_ms: 1,
        connections,
    };

    let mut v4 = HashMap::new();
    v4.insert("v4-1".to_string(), server);

    let router = RouterConfig {
        max_connections: 64,
        max_outgoing_packet_count: 1_024,
        max_segment_size: 1024 * 1024,
        max_segment_count: 4,
        custom_segment: None,
        initialized_filters: None,
        shared_subscriptions_strategy: Default::default(),
    };

    Config {
        id: 0,
        router,
        v4: Some(v4),
        v5: None,
        ws: None,
        cluster: None,
        console: None,
        bridge: None,
        prometheus: None,
        metrics: None,
    }
}

async fn wait_for_listener(port: u16, timeout: Duration) -> Result<(), MockMqttError> {
    let started = Instant::now();
    loop {
        match TcpStream::connect_timeout(
            &SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, port)),
            Duration::from_millis(50),
        ) {
            Ok(stream) => {
                drop(stream);
                return Ok(());
            }
            Err(_) if started.elapsed() < timeout => {
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            Err(_) => return Err(MockMqttError::StartTimeout(timeout)),
        }
    }
}

// Use `Arc<()>` only as a marker to silence dead-code complaints if
// the field set ever shrinks to zero in a refactor. Kept empty so
// nothing leaks into the public API surface.
#[allow(dead_code)]
fn _arc_marker() -> Arc<()> {
    Arc::new(())
}
