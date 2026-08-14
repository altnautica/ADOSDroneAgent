//! The bearer transport for whole TUNNEL frames.
//!
//! ## The bearer is the auxiliary application lane
//!
//! Config frames ride the radio's general-purpose auxiliary lane: the drone's
//! `-p2` downlink and the ground station's `-p3` uplink, tagged
//! [`AuxChannel::ConfigTunnel`] by the shared aux framing. That lane already
//! exists end to end — a published channel mux, a client that brings the
//! transmit/receive pair up on demand through the radio service, and a
//! per-channel dispatch on both rigs — so this transport adds a channel to a
//! carrier that is already running rather than a new radio integration.
//!
//! An earlier version of this crate claimed the low-rate `-p1` control plane as
//! its bearer. That was wrong and is why the bridge went unbuilt: `-p1` is fully
//! occupied by the hop announce/ack exchange, its ports are hard-reserved by the
//! radio service's own port guard, and it carries no demultiplexer at all, so
//! there was nowhere for a second stream to land.
//!
//! ## Why this is code and not two port numbers
//!
//! Pointing the old local UDP pair at the aux ports does NOT work. The aux
//! downlink is shared with the MAVLink tee, and the receiving rig decodes the aux
//! framing FIRST — an unmuxed datagram fails that decode and is counted as
//! another application's traffic and dropped. A frame has to be framed for the
//! lane it rides.
//!
//! ## Send is direct, receive comes through the plane consumer
//!
//! Egress writes straight onto the lane: on a drone through the radio service's
//! aux command socket (which brings the pair up on demand and answers with the
//! transmit ingress port), on a ground station straight to the already-running
//! uplink transmit ingress, which has no command socket to negotiate through.
//!
//! Ingress cannot bind the plane's own loopback port — the rig's plane consumer
//! already holds it — so the consumer forwards frames on this channel to this
//! service's loopback ingress ([`ados_protocol::config_tunnel_ingest`]), aux
//! framing intact. This transport re-decodes and filters to its own channel
//! rather than trusting the hop.
//!
//! The transport carries complete TUNNEL frames (the RF model), not the inner
//! payload; building/parsing the frame is the caller's job so the wire format
//! is identical to what crosses RF. One frame is one aux datagram: a v2 TUNNEL
//! frame is at most a 10-byte header plus a 133-byte body plus a 2-byte CRC,
//! about 145 bytes, against the aux lane's 1200-byte payload ceiling — so there
//! is no fragmentation anywhere in this path.

use std::path::PathBuf;

use async_trait::async_trait;
use tokio::net::UdpSocket;

use ados_protocol::aux_egress::{AuxEgress, AuxEgressError};
use ados_protocol::aux_mux::{self, AuxChannel, AUX_HEADER_LEN, AUX_MAX_PAYLOAD};

/// A largest-plausible datagram: an aux frame's 6-byte header plus its payload
/// ceiling. Bounds the receive buffer so a runaway datagram cannot grow memory,
/// with room for the framing itself so an oversized frame is rejected by the
/// decoder rather than silently truncated by the read.
const RECV_BUF: usize = AUX_HEADER_LEN + AUX_MAX_PAYLOAD;

#[async_trait]
pub trait TunnelTransport: Send + Sync {
    /// Send one complete TUNNEL frame onto the bearer.
    async fn send_frame(&self, frame: &[u8]) -> std::io::Result<()>;
    /// Receive the next inbound datagram (a complete TUNNEL frame) off the
    /// bearer. Pends until a frame arrives.
    async fn recv_frame(&self) -> std::io::Result<Vec<u8>>;
}

/// The real bearer transport: the auxiliary lane, tagged
/// [`AuxChannel::ConfigTunnel`].
pub struct AuxTunnelTransport {
    egress: AuxEgress,
    /// The service's own loopback ingress, which the rig's aux plane consumer
    /// forwards this channel's frames to.
    ingress: UdpSocket,
}

impl AuxTunnelTransport {
    /// Assemble from an already-built egress client and a bound ingress socket.
    #[must_use]
    pub fn from_parts(egress: AuxEgress, ingress: UdpSocket) -> Self {
        Self { egress, ingress }
    }

    /// The drone side: egress through the radio service's aux command socket,
    /// which owns the transmit/receive pair's lifecycle there and brings it up on
    /// demand (nothing radiates until the first send).
    pub async fn on_drone(
        aux_cmd_sock: impl Into<PathBuf>,
        ingress_port: u16,
    ) -> std::io::Result<Self> {
        let ingress = UdpSocket::bind(("127.0.0.1", ingress_port)).await?;
        Ok(Self::from_parts(AuxEgress::new(aux_cmd_sock), ingress))
    }

    /// The ground station side: egress straight to the aux uplink transmit
    /// ingress. A ground station has no `radio-aux.sock` — its `wfb_tx -p3` is
    /// spawned by the receive chain, so the ingress is a plain UDP port with no
    /// handshake to negotiate.
    pub async fn on_ground_station(aux_tx_port: u16, ingress_port: u16) -> std::io::Result<Self> {
        let ingress = UdpSocket::bind(("127.0.0.1", ingress_port)).await?;
        let egress = AuxEgress::connected_to_udp(aux_tx_port)
            .await
            .map_err(egress_io_error)?;
        Ok(Self::from_parts(egress, ingress))
    }

    /// The loopback port frames are received on (reported on the sidecar so an
    /// operator can see where the plane consumer must forward).
    pub fn ingress_port(&self) -> std::io::Result<u16> {
        Ok(self.ingress.local_addr()?.port())
    }
}

/// Map an aux egress failure onto an io error, preserving what a caller can act
/// on: an operator-disabled lane is a permission verdict, not a transport fault,
/// and an oversized frame is this build's own bug rather than a link problem.
fn egress_io_error(e: AuxEgressError) -> std::io::Error {
    use std::io::ErrorKind;
    let kind = match &e {
        AuxEgressError::Disabled => ErrorKind::PermissionDenied,
        AuxEgressError::Refused(_) => ErrorKind::ConnectionRefused,
        AuxEgressError::Unavailable(_) => ErrorKind::NotConnected,
        AuxEgressError::TooLarge(_) => ErrorKind::InvalidData,
        AuxEgressError::Send(_) => ErrorKind::BrokenPipe,
    };
    std::io::Error::new(kind, e.to_string())
}

#[async_trait]
impl TunnelTransport for AuxTunnelTransport {
    async fn send_frame(&self, frame: &[u8]) -> std::io::Result<()> {
        self.egress
            .send(AuxChannel::ConfigTunnel, frame)
            .await
            .map_err(egress_io_error)
    }

    async fn recv_frame(&self) -> std::io::Result<Vec<u8>> {
        let mut buf = [0u8; RECV_BUF];
        loop {
            let (n, _src) = self.ingress.recv_from(&mut buf).await?;
            match aux_mux::decode(&buf[..n]) {
                Ok((AuxChannel::ConfigTunnel, payload)) => return Ok(payload.to_vec()),
                // Another channel on this loopback port means the forwarding
                // consumer misrouted, so say which one rather than dropping it
                // mutely — and never hand it to the tunnel's reassembler.
                Ok((channel, _)) => {
                    tracing::debug!(?channel, "tunnel_config_aux_wrong_channel")
                }
                Err(e) => tracing::debug!(error = ?e, "tunnel_config_aux_undecodable"),
            }
        }
    }
}

#[cfg(test)]
pub(crate) mod mock {
    //! An in-memory transport pair for hardware-free end-to-end tests: a
    //! frame sent on one half is received on the other.
    use super::*;
    use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
    use tokio::sync::Mutex;

    pub struct MockTransport {
        outbound: UnboundedSender<Vec<u8>>,
        inbound: Mutex<UnboundedReceiver<Vec<u8>>>,
    }

    /// A crossed pair: `a.send_frame` is delivered to `b.recv_frame` and vice
    /// versa — an in-memory stand-in for the paired aux lane, with no framing
    /// and no sockets, so a test of the chunking layer stays a test of the
    /// chunking layer.
    #[must_use]
    pub fn duplex() -> (MockTransport, MockTransport) {
        let (a_tx, a_rx) = unbounded_channel();
        let (b_tx, b_rx) = unbounded_channel();
        (
            MockTransport {
                outbound: b_tx,
                inbound: Mutex::new(a_rx),
            },
            MockTransport {
                outbound: a_tx,
                inbound: Mutex::new(b_rx),
            },
        )
    }

    #[async_trait]
    impl TunnelTransport for MockTransport {
        async fn send_frame(&self, frame: &[u8]) -> std::io::Result<()> {
            // A closed peer receiver drops the frame (lossy bearer semantics).
            let _ = self.outbound.send(frame.to_vec());
            Ok(())
        }

        async fn recv_frame(&self) -> std::io::Result<Vec<u8>> {
            let mut rx = self.inbound.lock().await;
            match rx.recv().await {
                Some(frame) => Ok(frame),
                // A closed channel parks forever rather than busy-looping the
                // caller's select arm (matches a quiet real bearer).
                None => {
                    std::future::pending::<()>().await;
                    unreachable!()
                }
            }
        }
    }
}

#[cfg(test)]
pub(crate) mod aux_fixture {
    //! A hardware-free stand-in for the whole aux bridge: two real
    //! [`AuxTunnelTransport`]s, one per rig, crossed so each one's egress lands
    //! on the other's ingress.
    //!
    //! What it exercises that [`super::mock`] cannot: the radio open handshake,
    //! the aux encode, a real UDP hop, the aux decode, and the channel filter.
    //! What it stands in for: `wfb_tx` framing the datagram, the air, `wfb_rx`
    //! decoding it, and the rig's plane consumer forwarding the frame to this
    //! service's ingress. That last hop forwards the frame VERBATIM, which is
    //! why pointing the handshake's transmit port straight at the far ingress is
    //! a faithful model of it and not a shortcut past it.
    use super::*;
    use std::time::Duration;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixListener;

    /// A fake aux command socket that replies to `reply_count` requests with an
    /// active pair pointing at `tx_port`, then stops answering.
    ///
    /// The listener is bound BEFORE the task is spawned: binding inside the task
    /// leaves the client racing the bind, which a fixed sleep only papers over.
    pub fn fake_radio(
        sock_path: PathBuf,
        tx_port: u16,
        reply_count: usize,
    ) -> tokio::task::JoinHandle<()> {
        let listener = UnixListener::bind(&sock_path).expect("bind fake aux command socket");
        tokio::spawn(async move {
            for _ in 0..reply_count {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let (rx, mut tx) = stream.into_split();
                let mut line = String::new();
                let _ = BufReader::new(rx).read_line(&mut line).await;
                let reply = format!(
                    "{{\"ok\":true,\"active\":true,\"tx_port\":{tx_port},\"rx_port\":5603}}\n"
                );
                let _ = tx.write_all(reply.as_bytes()).await;
            }
            // Accept but never answer afterwards, so a second handshake would
            // hang rather than silently succeed.
            while listener.accept().await.is_ok() {}
        })
    }

    /// A crossed drone/ground pair. Keep the whole struct alive for the test's
    /// lifetime: dropping `_dir` removes the fake command socket, and the two
    /// halves may be moved out individually (no `Drop` impl, so a partial move
    /// into an injector/terminator is allowed).
    pub struct CrossedPair {
        pub drone: AuxTunnelTransport,
        pub ground: AuxTunnelTransport,
        pub _dir: tempfile::TempDir,
        pub _radio: tokio::task::JoinHandle<()>,
    }

    /// Build the pair. Both ingress ports are ephemeral, so the fixture never
    /// collides with a real service on the configured default port.
    pub async fn crossed_pair() -> CrossedPair {
        let dir = tempfile::tempdir().expect("tempdir");
        let ground_ingress = UdpSocket::bind("127.0.0.1:0")
            .await
            .expect("ground ingress");
        let ground_port = ground_ingress.local_addr().unwrap().port();
        let drone_ingress = UdpSocket::bind("127.0.0.1:0").await.expect("drone ingress");
        let drone_port = drone_ingress.local_addr().unwrap().port();

        // The drone goes through the command socket, as production does; the
        // handshake answers with the transmit ingress, which here is the ground
        // side's tunnel ingress. A generous reply budget so a self-healed
        // re-open cannot hang the fixture on the "stops answering" tail.
        let sock_path = dir.path().join("radio-aux.sock");
        let radio = fake_radio(sock_path.clone(), ground_port, 8);
        let drone = AuxTunnelTransport::from_parts(
            AuxEgress::with_timeout(&sock_path, Duration::from_millis(500)),
            drone_ingress,
        );

        // The ground station uses the direct-ingress constructor, as production
        // does: it has no radio command socket.
        let ground = AuxTunnelTransport::from_parts(
            AuxEgress::connected_to_udp(drone_port)
                .await
                .expect("ground egress"),
            ground_ingress,
        );

        CrossedPair {
            drone,
            ground,
            _dir: dir,
            _radio: radio,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn mock_duplex_delivers_both_directions() {
        let (a, b) = mock::duplex();
        a.send_frame(b"hi-b").await.unwrap();
        b.send_frame(b"hi-a").await.unwrap();
        assert_eq!(b.recv_frame().await.unwrap(), b"hi-b");
        assert_eq!(a.recv_frame().await.unwrap(), b"hi-a");
    }

    /// The bridge itself: open handshake, aux encode, UDP hop, aux decode,
    /// channel filter — in both directions.
    #[tokio::test]
    async fn a_tunnel_frame_crosses_the_aux_bridge_verbatim() {
        let pair = aux_fixture::crossed_pair().await;

        pair.ground.send_frame(b"a-request-frame").await.unwrap();
        assert_eq!(pair.drone.recv_frame().await.unwrap(), b"a-request-frame");

        pair.drone.send_frame(b"a-response-frame").await.unwrap();
        assert_eq!(pair.ground.recv_frame().await.unwrap(), b"a-response-frame");
    }

    /// The lane is shared. A frame on any other channel must never reach the
    /// tunnel's reassembler: the aux downlink also carries the MAVLink tee, so
    /// this is the case that decides whether a telemetry batch can be pushed
    /// into the config path.
    #[tokio::test]
    async fn a_frame_on_another_aux_channel_is_not_delivered_to_the_tunnel() {
        let pair = aux_fixture::crossed_pair().await;
        let port = pair.drone.ingress_port().unwrap();
        let sender = UdpSocket::bind("127.0.0.1:0").await.unwrap();

        for channel in [
            AuxChannel::Mavlink,
            AuxChannel::Status,
            AuxChannel::Identity,
            AuxChannel::Request,
            AuxChannel::Response,
            AuxChannel::LinkFeedback,
        ] {
            let frame = aux_mux::encode(channel, b"not-for-the-tunnel").unwrap();
            sender.send_to(&frame, ("127.0.0.1", port)).await.unwrap();
        }
        // Unframed junk on the same port is dropped by the same filter.
        sender
            .send_to(b"raw-bytes", ("127.0.0.1", port))
            .await
            .unwrap();
        // And then one real frame, which is the only thing that must come out.
        let ours = aux_mux::encode(AuxChannel::ConfigTunnel, b"ours").unwrap();
        sender.send_to(&ours, ("127.0.0.1", port)).await.unwrap();

        let got = tokio::time::timeout(std::time::Duration::from_secs(2), pair.drone.recv_frame())
            .await
            .expect("the tunnel frame arrives")
            .unwrap();
        assert_eq!(got, b"ours");
    }

    #[tokio::test]
    async fn a_send_with_no_radio_reports_a_retriable_transport_verdict() {
        // A drone whose radio service is not up yet: the caller must be able to
        // tell an unreachable radio (retry later) from an operator-disabled lane
        // (report once and stay quiet), so the two map to different io kinds.
        let dir = tempfile::tempdir().unwrap();
        let sock_path = dir.path().join("radio-aux.sock");
        let ingress = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let transport = AuxTunnelTransport::from_parts(
            AuxEgress::with_timeout(&sock_path, std::time::Duration::from_millis(100)),
            ingress,
        );
        let err = transport.send_frame(b"frame").await.unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::NotConnected);
    }
}
