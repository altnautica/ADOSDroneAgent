//! The loopback hand-off that gets a config-over-radio frame from the aux plane
//! consumer to the local `ados-tunnel-config` service.
//!
//! ## Why a hand-off rather than a direct bind
//!
//! Exactly one process may hold a UDP bind, and on both rigs the aux plane's
//! decoded loopback port is already held: the drone's uplink port by the MAVLink
//! router's uplink consumer, the ground station's downlink port by the
//! groundlink aux consumer. Those consumers are the only readers of their plane,
//! and they already demux every aux datagram by
//! [`crate::aux_mux::AuxChannel`]. So the config tunnel does not open a second
//! bind on a port it could never get; the consumer that already decoded the
//! frame forwards it here.
//!
//! Frames are forwarded with their aux framing INTACT. The tunnel service
//! decodes and re-checks the channel itself rather than trusting the hop, so a
//! stray datagram arriving on this loopback port — from anything at all — cannot
//! be mistaken for a config frame, and the forwarder stays a dumb pipe with no
//! second copy of the framing rules.
//!
//! ## Why the port is resolved from config, not a literal
//!
//! Three processes have to agree on it (the drone's uplink consumer, the ground
//! station's aux consumer, and the tunnel service that binds it), and they live
//! in crates that deliberately do not depend on one another. That is the exact
//! shape of the failure [`crate::aux_ports`] exists to prevent: a port moved in
//! config on one side leaves the others writing to the old number, so the lane
//! goes silent and nothing reports an error. Every side resolves it here.

use std::net::SocketAddr;

use serde::Deserialize;
use tokio::net::UdpSocket;
use tokio::sync::Mutex;

/// Loopback port the `ados-tunnel-config` service binds for inbound frames, and
/// the port a plane consumer forwards [`crate::aux_mux::AuxChannel::ConfigTunnel`]
/// frames to. Mirrored by `ados_config_tunnel::config::DEFAULT_RX_PORT`, which
/// is defined as this constant.
pub const DEFAULT_INGRESS_PORT: u16 = 5820;

#[derive(Debug, Default, Deserialize)]
struct RawRoot {
    #[serde(default)]
    radio: RawRadio,
}

#[derive(Debug, Default, Deserialize)]
struct RawRadio {
    #[serde(default)]
    tunnel: RawTunnel,
}

#[derive(Debug, Default, Deserialize)]
struct RawTunnel {
    #[serde(default)]
    rx_port: Option<u16>,
}

/// Resolve the ingress port from YAML text (`radio.tunnel.rx_port`).
///
/// Anything missing or unparseable falls back to [`DEFAULT_INGRESS_PORT`]. Port
/// 0 is refused: it means "any port" to the kernel, which for a fixed rendezvous
/// between three processes binds somewhere the others cannot find.
#[must_use]
pub fn ingress_port_from_yaml(text: &str) -> u16 {
    let raw: RawRoot = serde_norway::from_str(text).unwrap_or_default();
    raw.radio
        .tunnel
        .rx_port
        .filter(|p| *p != 0)
        .unwrap_or(DEFAULT_INGRESS_PORT)
}

/// Resolve from a config file, falling back to the default when it is absent or
/// unreadable — a node with no config still has to come up coherent.
#[must_use]
pub fn ingress_port_from(path: &std::path::Path) -> u16 {
    match std::fs::read_to_string(path) {
        Ok(text) => ingress_port_from_yaml(&text),
        Err(_) => DEFAULT_INGRESS_PORT,
    }
}

/// Resolve from the agent's config file (honouring the `ADOS_CONFIG_YAML`
/// override, so a test points every side at one temp file).
#[must_use]
pub fn ingress_port() -> u16 {
    ingress_port_from(&crate::aux_ports::config_path())
}

/// A plane consumer's forwarder to the local config-tunnel service.
///
/// Best-effort by construction: the tunnel service is inert on a node that never
/// opted the channel in, so nothing is bound to the ingress port on most
/// deployments and a send failure is the NORMAL case rather than a fault. The
/// socket is unconnected and every datagram is addressed explicitly, so an ICMP
/// port-unreachable from an absent service cannot latch an error onto the socket
/// and poison the next forward once the service does start.
pub struct ConfigTunnelIngest {
    target: SocketAddr,
    /// Bound lazily on first forward, so a consumer on a node with no config
    /// tunnel never opens a socket at all.
    sock: Mutex<Option<UdpSocket>>,
}

impl ConfigTunnelIngest {
    /// A forwarder to an explicit loopback port.
    #[must_use]
    pub fn new(port: u16) -> Self {
        Self {
            target: SocketAddr::from(([127, 0, 0, 1], port)),
            sock: Mutex::new(None),
        }
    }

    /// A forwarder to the configured ingress port ([`ingress_port`]).
    #[must_use]
    pub fn at_configured_port() -> Self {
        Self::new(ingress_port())
    }

    /// The port this forwarder targets (reported on the consumer's log line so
    /// an operator can see where frames are going).
    #[must_use]
    pub fn target_port(&self) -> u16 {
        self.target.port()
    }

    /// Forward one aux frame verbatim. Returns whether it entered the kernel —
    /// which is not proof the tunnel service read it, only that this hop did not
    /// fail. A failed send drops the socket so the next forward rebinds rather
    /// than writing into a dead handle.
    pub async fn send(&self, frame: &[u8]) -> bool {
        let mut guard = self.sock.lock().await;
        if guard.is_none() {
            match UdpSocket::bind("127.0.0.1:0").await {
                Ok(s) => *guard = Some(s),
                Err(e) => {
                    tracing::debug!(error = %e, "config_tunnel_ingest_bind_failed");
                    return false;
                }
            }
        }
        let sock = guard.as_ref().expect("socket bound above");
        match sock.send_to(frame, self.target).await {
            Ok(_) => true,
            Err(e) => {
                tracing::debug!(error = %e, port = self.target.port(), "config_tunnel_ingest_send_failed");
                *guard = None;
                false
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_port_is_used_when_nothing_is_configured() {
        assert_eq!(ingress_port_from_yaml(""), DEFAULT_INGRESS_PORT);
        assert_eq!(
            ingress_port_from_yaml("network:\n  hostname: node\n"),
            DEFAULT_INGRESS_PORT
        );
    }

    #[test]
    fn an_operator_override_is_honoured() {
        // The failure this guards: the tunnel service binds the configured port
        // while both plane consumers keep forwarding to the default, so config
        // frames arrive over the air, decode fine, and land on a port nobody
        // reads — with no error anywhere.
        assert_eq!(
            ingress_port_from_yaml("radio:\n  tunnel:\n    rx_port: 6001\n"),
            6001
        );
    }

    #[test]
    fn port_zero_and_malformed_yaml_fall_back() {
        assert_eq!(
            ingress_port_from_yaml("radio:\n  tunnel:\n    rx_port: 0\n"),
            DEFAULT_INGRESS_PORT
        );
        assert_eq!(
            ingress_port_from_yaml("radio: [not a map"),
            DEFAULT_INGRESS_PORT
        );
    }

    #[test]
    fn a_missing_file_yields_the_default() {
        assert_eq!(
            ingress_port_from(std::path::Path::new("/nonexistent/ados/config.yaml")),
            DEFAULT_INGRESS_PORT
        );
    }

    #[tokio::test]
    async fn a_forwarded_frame_arrives_verbatim() {
        let listener = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let ingest = ConfigTunnelIngest::new(listener.local_addr().unwrap().port());
        let frame = crate::aux_mux::encode(crate::aux_mux::AuxChannel::ConfigTunnel, b"chunk")
            .expect("within budget");
        assert!(ingest.send(&frame).await);
        let mut buf = [0u8; 256];
        let (n, _) = listener.recv_from(&mut buf).await.unwrap();
        // Framing intact: the service re-decodes rather than trusting the hop.
        assert_eq!(&buf[..n], frame.as_slice());
    }

    #[tokio::test]
    async fn forwarding_with_no_service_listening_returns_promptly() {
        // The normal case on a node that never opted the channel in: the send
        // must not park the consumer's read loop or panic.
        let ingest = ConfigTunnelIngest::new(1);
        let started = std::time::Instant::now();
        let _ = ingest.send(b"frame").await;
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
    }
}
