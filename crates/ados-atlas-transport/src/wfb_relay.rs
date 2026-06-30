//! The WFB-relay bearer: carry small Atlas events drone→ground over the WFB
//! radio link, for the field/outdoor topology where there is no shared LAN.
//!
//! The drone side rides the agent's **auxiliary application stream** — an
//! on-demand UDP-tunnel over wfb-ng's `wfb_tx`/`wfb_rx` on a dedicated radio_id,
//! brought up through `/run/ados/radio-aux.sock` (`{"op":"open"}` spawns the
//! `wfb_tx -p 2 -u <tx_port>` egress; anything sent to `127.0.0.1:<tx_port>`
//! goes over the air). This bearer opens the aux stream once, then UDP-sends one
//! framed [`AtlasEvent`] (a self-delimiting msgpack frame) per datagram. The
//! ground agent's Atlas relay (`ados-groundlink`) decodes the off-air datagrams
//! and re-emits them onto the LAN into the compute node's event router.
//!
//! **The WFB lane is decimated.** `wfb_tx` FEC-blocks per UDP datagram with a
//! ~1.4 KB per-packet payload cap, so a full-res keyframe (the LAN bearer's
//! many-MB envelope) cannot ride it — only small descriptor/pose/status events
//! do. A framed event over the cap is rejected with [`TransportError::
//! PayloadTooLarge`] (a per-bearer capacity limit, so retriable: the ladder
//! declines WFB by its cap and offers the event to a fatter downstream lane).
//!
//! The aux stream is safe-by-default off (an `aux_enable` dead-switch in
//! `ados-radio`): an open against a disabled deployment is refused, which this
//! bearer surfaces as [`TransportError::Unavailable`] so the ladder skips it.

use std::path::{Path, PathBuf};
use std::time::Duration;

use ados_protocol::atlas::AtlasEvent;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UdpSocket, UnixStream};
use tokio::sync::Mutex;

use crate::{AtlasBearer, BearerKind, TransportError};

/// The wfb-ng per-packet payload ceiling for a single aux datagram. Conservative
/// vs the ~1.4 KB FEC-block cap so a framed event never silently truncates.
pub const WFB_MAX_DATAGRAM: usize = 1300;

/// Bound on the aux command-socket round-trip. A wedged `ados-radio` (accepts
/// but never replies) must NOT park the send path: on timeout the bearer reports
/// Unavailable (retriable) so the ladder falls over to the cloud lane.
const AUX_TIMEOUT: Duration = Duration::from_secs(2);

/// Default aux command socket (matches `ados_radio::paths::RADIO_AUX_SOCK`).
const DEFAULT_AUX_SOCK: &str = "/run/ados/radio-aux.sock";

/// Carries small framed Atlas events over the WFB aux application stream.
pub struct WfbRelayBearer {
    /// The aux command socket that opens/closes/queries the aux tx/rx pair.
    aux_cmd_sock: PathBuf,
    /// The framed-event size ceiling for this lane.
    max_datagram: usize,
    /// The connected egress socket, lazily opened on first send (after the aux
    /// stream is up). `None` until opened. A `Mutex` serialises sends, which is
    /// fine at the keyframe/descriptor rate (a few per second).
    conn: Mutex<Option<UdpSocket>>,
}

impl WfbRelayBearer {
    /// A bearer that opens the aux stream lazily via the default command socket.
    pub fn new() -> Self {
        Self::with_socket(DEFAULT_AUX_SOCK, WFB_MAX_DATAGRAM)
    }

    /// A bearer with an explicit aux command socket + datagram ceiling.
    pub fn with_socket(aux_cmd_sock: impl Into<PathBuf>, max_datagram: usize) -> Self {
        Self {
            aux_cmd_sock: aux_cmd_sock.into(),
            max_datagram,
            conn: Mutex::new(None),
        }
    }

    /// Test/dev: a bearer already connected to a UDP target, skipping the aux
    /// handshake — so the framing + emission path is exercised over a plain
    /// loopback socketpair with no `wfb_tx`/aux socket.
    pub fn connected_for_test(sock: UdpSocket, max_datagram: usize) -> Self {
        Self {
            aux_cmd_sock: PathBuf::new(),
            max_datagram,
            conn: Mutex::new(Some(sock)),
        }
    }

    /// One newline-JSON request/response round-trip against the aux command
    /// socket, bounded by `AUX_TIMEOUT`. `op` is `open` / `close` / `status`. A
    /// timeout (a wedged `ados-radio`) maps to Unavailable so the caller skips
    /// this bearer rather than blocking the ladder forever.
    async fn aux_request(sock: &Path, op: &str) -> Result<serde_json::Value, TransportError> {
        let work = async {
            let stream = UnixStream::connect(sock)
                .await
                .map_err(|e| TransportError::Request(e.to_string()))?;
            let (rx, mut tx) = stream.into_split();
            tx.write_all(format!("{{\"op\":\"{op}\"}}\n").as_bytes())
                .await
                .map_err(|e| TransportError::Request(e.to_string()))?;
            let mut line = String::new();
            BufReader::new(rx)
                .read_line(&mut line)
                .await
                .map_err(|e| TransportError::Request(e.to_string()))?;
            serde_json::from_str(&line).map_err(|e| TransportError::Request(e.to_string()))
        };
        tokio::time::timeout(AUX_TIMEOUT, work)
            .await
            .map_err(|_| TransportError::Unavailable)?
    }

    /// Ensure the aux stream is open and the egress socket is connected. The open
    /// handshake runs WITHOUT holding the conn mutex, so a slow/hung `ados-radio`
    /// can never block a concurrent `is_available()` or `send()` (it only stalls
    /// its own call, which the `AUX_TIMEOUT` bounds). Idempotent: a concurrent
    /// opener that wins the race keeps its socket; the loser drops its own.
    async fn ensure_conn(&self) -> Result<(), TransportError> {
        if self.conn.lock().await.is_some() {
            return Ok(());
        }
        let reply = Self::aux_request(&self.aux_cmd_sock, "open").await?;
        // A disabled deployment / failed spawn → skip this bearer, not a hard error.
        if reply.get("ok").and_then(|v| v.as_bool()) != Some(true)
            || reply.get("active").and_then(|v| v.as_bool()) != Some(true)
        {
            return Err(TransportError::Unavailable);
        }
        let tx_port: u16 = reply
            .get("tx_port")
            .and_then(|v| v.as_u64())
            .and_then(|v| u16::try_from(v).ok())
            .filter(|p| *p != 0)
            .ok_or(TransportError::Unavailable)?;
        let sock = UdpSocket::bind("127.0.0.1:0")
            .await
            .map_err(|e| TransportError::Request(e.to_string()))?;
        sock.connect(("127.0.0.1", tx_port))
            .await
            .map_err(|e| TransportError::Request(e.to_string()))?;
        // Store only if nobody beat us; either socket targets the same port.
        let mut guard = self.conn.lock().await;
        if guard.is_none() {
            *guard = Some(sock);
        }
        Ok(())
    }
}

impl Default for WfbRelayBearer {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl AtlasBearer for WfbRelayBearer {
    fn kind(&self) -> BearerKind {
        BearerKind::WfbRelay
    }

    async fn is_available(&self) -> bool {
        // Already open → up. Otherwise ask the aux socket whether the pair is
        // active (proves the lane is provisioned without opening it).
        if self.conn.lock().await.is_some() {
            return true;
        }
        matches!(
            Self::aux_request(&self.aux_cmd_sock, "status").await,
            Ok(v) if v.get("active").and_then(|x| x.as_bool()) == Some(true)
        )
    }

    async fn send(&self, event: &AtlasEvent) -> Result<(), TransportError> {
        // Frame first so an oversized event is rejected before opening anything.
        let body = event.to_msgpack()?;
        if body.len() > self.max_datagram {
            // Retriable: a fatter downstream lane may carry it (the ladder
            // declines WFB by its cap and falls over).
            return Err(TransportError::PayloadTooLarge(body.len()));
        }
        // Open outside the conn lock (ensure_conn does its handshake unlocked).
        self.ensure_conn().await?;
        let mut guard = self.conn.lock().await;
        let send_res = match guard.as_ref() {
            // UDP fire-and-forget: Ok means the datagram entered the kernel, NOT
            // that wfb_tx radiated it or the peer decoded it —
            // the ground relay's received-side counter is the delivery proof.
            Some(sock) => sock.send(&body).await,
            None => return Err(TransportError::Unavailable),
        };
        match send_res {
            Ok(_) => Ok(()),
            Err(e) => {
                // Self-heal: drop the dead socket so the next send re-opens the
                // aux stream rather than blackholing into a stale handle.
                *guard = None;
                Err(TransportError::Request(e.to_string()))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::UnixListener;

    fn event(topic: &str, payload: Vec<u8>) -> AtlasEvent {
        AtlasEvent {
            topic: topic.to_string(),
            payload,
        }
    }

    #[tokio::test]
    async fn a_sent_event_is_received_over_the_aux_datagram() {
        // Stand in for wfb_tx's loopback ingress with a plain UDP receiver.
        let rx = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let rx_addr = rx.local_addr().unwrap();
        let tx = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        tx.connect(rx_addr).await.unwrap();
        let bearer = WfbRelayBearer::connected_for_test(tx, WFB_MAX_DATAGRAM);

        let sent = event("atlas.pose", vec![1, 2, 3, 4]);
        bearer.send(&sent).await.unwrap();

        let mut buf = vec![0u8; 2048];
        let (n, _) = rx.recv_from(&mut buf).await.unwrap();
        let got = AtlasEvent::from_msgpack(&buf[..n]).unwrap();
        assert_eq!(got.topic, "atlas.pose");
        assert_eq!(got.payload, vec![1, 2, 3, 4]);
        assert!(bearer.is_available().await, "an open bearer is available");
    }

    #[tokio::test]
    async fn an_oversized_event_is_rejected_retriably_without_sending() {
        // A receiver that must NEVER get a datagram.
        let rx = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let tx = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        tx.connect(rx.local_addr().unwrap()).await.unwrap();
        let bearer = WfbRelayBearer::connected_for_test(tx, 16);

        let err = bearer
            .send(&event("atlas.keyframe", vec![0u8; 4096]))
            .await
            .unwrap_err();
        assert!(matches!(err, TransportError::PayloadTooLarge(_)));
        assert!(
            err.is_retriable(),
            "WFB declines by its cap; the ladder falls over to a fatter lane"
        );
        // Nothing was sent: a short read times out with no datagram.
        let mut buf = [0u8; 64];
        let got =
            tokio::time::timeout(std::time::Duration::from_millis(50), rx.recv_from(&mut buf))
                .await;
        assert!(got.is_err(), "no datagram should have been emitted");
    }

    /// Drive the aux `{"op":"open"}` handshake against a fake command socket and
    /// assert the bearer connects to the replied tx_port and then sends.
    #[tokio::test]
    async fn the_aux_open_handshake_connects_to_the_replied_tx_port() {
        let dir = tempfile::tempdir().unwrap();
        let sock_path = dir.path().join("radio-aux.sock");
        // The real egress port wfb_tx would listen on; our fake stands in for it.
        let egress = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let tx_port = egress.local_addr().unwrap().port();

        let listener = UnixListener::bind(&sock_path).unwrap();
        let server = tokio::spawn(async move {
            // Reply to every request (open + status) with the active pair.
            loop {
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
        });

        let bearer = WfbRelayBearer::with_socket(&sock_path, WFB_MAX_DATAGRAM);
        assert!(
            bearer.is_available().await,
            "status reports the pair active"
        );
        bearer
            .send(&event("atlas.occupancy", vec![7, 7]))
            .await
            .unwrap();

        let mut buf = vec![0u8; 256];
        let (n, _) = egress.recv_from(&mut buf).await.unwrap();
        let got = AtlasEvent::from_msgpack(&buf[..n]).unwrap();
        assert_eq!(got.topic, "atlas.occupancy");
        server.abort();
    }

    #[tokio::test]
    async fn a_disabled_aux_stream_is_unavailable_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let sock_path = dir.path().join("radio-aux.sock");
        let listener = UnixListener::bind(&sock_path).unwrap();
        let server = tokio::spawn(async move {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let (rx, mut tx) = stream.into_split();
            let mut line = String::new();
            let _ = BufReader::new(rx).read_line(&mut line).await;
            let _ = tx
                .write_all(b"{\"ok\":false,\"error\":\"E_AUX_DISABLED\"}\n")
                .await;
        });
        let bearer = WfbRelayBearer::with_socket(&sock_path, WFB_MAX_DATAGRAM);
        let err = bearer
            .send(&event("atlas.pose", vec![1]))
            .await
            .unwrap_err();
        assert!(matches!(err, TransportError::Unavailable));
        assert!(err.is_retriable(), "a down lane lets the ladder fall over");
        server.abort();
    }
}
