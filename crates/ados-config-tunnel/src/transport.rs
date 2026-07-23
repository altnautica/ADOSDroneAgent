//! The bearer transport for whole TUNNEL frames.
//!
//! L3 config frames enter and leave through a LOCAL UDP datagram pair: the
//! service binds `rx_port` and sends to `tx_port`. Those dedicated ports are
//! bridged onto the `-p1` WFB control plane by a separate, gated `ados-radio`
//! integration step — this crate never touches the raw WFB sockets or the FC
//! lane, only its own UDP ports, so the config path can never reach the flight
//! controller. Until the RF bridge is wired the service still binds and
//! reports honestly (a received counter that stays 0 means nothing arrives).
//!
//! The transport carries complete TUNNEL frames (the RF model), not the inner
//! payload; building/parsing the frame is the caller's job so the wire format
//! is identical to what crosses RF.

use std::net::SocketAddr;

use async_trait::async_trait;
use tokio::net::UdpSocket;

/// A largest-plausible datagram: a v2 TUNNEL frame is at most a 10-byte header
/// plus a 133-byte body (5 fixed fields + 128 payload) plus a 2-byte CRC, about
/// 145 bytes. Bound the receive buffer generously so a runaway datagram cannot
/// grow memory.
const RECV_BUF: usize = 512;

#[async_trait]
pub trait TunnelTransport: Send + Sync {
    /// Send one complete TUNNEL frame onto the bearer.
    async fn send_frame(&self, frame: &[u8]) -> std::io::Result<()>;
    /// Receive the next inbound datagram (a complete TUNNEL frame) off the
    /// bearer. Pends until a frame arrives.
    async fn recv_frame(&self) -> std::io::Result<Vec<u8>>;
}

/// The real bearer transport: a UDP socket bound to the local `rx_port`, that
/// sends to the local `tx_port`.
pub struct UdpTunnelTransport {
    sock: UdpSocket,
    send_to: SocketAddr,
}

impl UdpTunnelTransport {
    /// Bind `127.0.0.1:rx_port` for receive and target `127.0.0.1:tx_port` for
    /// send.
    pub async fn bind(rx_port: u16, tx_port: u16) -> std::io::Result<Self> {
        let sock = UdpSocket::bind(("127.0.0.1", rx_port)).await?;
        let send_to = SocketAddr::from(([127, 0, 0, 1], tx_port));
        Ok(Self { sock, send_to })
    }
}

#[async_trait]
impl TunnelTransport for UdpTunnelTransport {
    async fn send_frame(&self, frame: &[u8]) -> std::io::Result<()> {
        self.sock.send_to(frame, self.send_to).await.map(|_| ())
    }

    async fn recv_frame(&self) -> std::io::Result<Vec<u8>> {
        let mut buf = [0u8; RECV_BUF];
        let (n, _src) = self.sock.recv_from(&mut buf).await?;
        Ok(buf[..n].to_vec())
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
    /// versa — an in-memory stand-in for the paired `-p1` bearer.
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
mod tests {
    use super::*;

    #[tokio::test]
    async fn udp_transport_round_trips_a_frame() {
        // Bind two transports crossed over loopback and confirm a frame sent
        // on one arrives on the other.
        let a = UdpTunnelTransport::bind(0, 0).await.unwrap();
        let a_rx = a.sock.local_addr().unwrap().port();
        let b = UdpTunnelTransport::bind(0, a_rx).await.unwrap();
        let b_rx = b.sock.local_addr().unwrap().port();
        // Point a's send at b's rx port.
        let a = UdpTunnelTransport {
            sock: a.sock,
            send_to: SocketAddr::from(([127, 0, 0, 1], b_rx)),
        };
        a.send_frame(b"a-tunnel-frame").await.unwrap();
        let got = b.recv_frame().await.unwrap();
        assert_eq!(got, b"a-tunnel-frame");
    }

    #[tokio::test]
    async fn mock_duplex_delivers_both_directions() {
        let (a, b) = mock::duplex();
        a.send_frame(b"hi-b").await.unwrap();
        b.send_frame(b"hi-a").await.unwrap();
        assert_eq!(b.recv_frame().await.unwrap(), b"hi-b");
        assert_eq!(a.recv_frame().await.unwrap(), b"hi-a");
    }
}
