//! Egress client for the auxiliary application stream.
//!
//! [`aux_mux`](crate::aux_mux) says what an aux datagram *means*; this module is
//! how a producer gets one onto the air. Both halves live here because a channel
//! tag is useless without a carrier, and every producer that ever sends on the
//! lane (MAVLink, status, identity) needs the identical two steps:
//!
//! 1. Ask the running radio service to bring the aux transmit/receive pair up,
//!    over the newline-JSON command socket it serves (`{"op":"open"}` ->
//!    `{"ok":true,"active":true,"tx_port":N,"rx_port":M}`).
//! 2. UDP-send framed datagrams to `127.0.0.1:tx_port`, which is the loopback
//!    ingress of the `wfb_tx` the open spawned.
//!
//! ## What a successful send does and does not prove
//!
//! `send` returning `Ok` means the datagram entered the local kernel's UDP
//! buffer. It does NOT mean `wfb_tx` framed it, the radio radiated it, or the
//! peer decoded it. The only proof of delivery is a received-side counter on the
//! far end. Callers must not report a link as working on the strength of a send
//! that returned.
//!
//! ## Safe-by-default
//!
//! The aux pair does not exist until something opens it, and an operator can
//! forbid it outright. A refused open is reported as [`AuxEgressError::Disabled`]
//! — a distinct variant, because a deliberately disabled lane is an operator
//! choice to report once and live with, not a fault to retry hard against.
//!
//! ## Relationship to the Atlas WFB bearer
//!
//! The Atlas transport carries its own copy of this handshake, written before
//! this client existed and wired into that crate's bearer-ladder error type.
//! Converging the two is a worthwhile follow-up, but it changes a shipping lane
//! and belongs in its own change; this module is deliberately generic (raw
//! payload + channel tag, no event type) so that convergence is a small step.

use std::path::PathBuf;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UdpSocket, UnixStream};
use tokio::sync::Mutex;

use crate::aux_mux::{self, AuxChannel};

/// Bound on one aux command-socket round trip.
///
/// A radio service that accepts the connection but never replies must not park
/// the caller: on timeout the open reports [`AuxEgressError::Unavailable`] so a
/// producer backs off instead of stalling behind a wedged peer.
pub const AUX_REQUEST_TIMEOUT: Duration = Duration::from_secs(2);

/// The error string the radio returns when the operator dead-switch is off.
pub const E_AUX_DISABLED: &str = "E_AUX_DISABLED";

/// Why an aux egress operation did not complete.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuxEgressError {
    /// The operator dead-switch is off: the radio refused to open the pair and
    /// spawned nothing. Retrying hard will not help; report once and stay quiet.
    Disabled,
    /// The radio replied but declined for some other reason (a spawn failure,
    /// an unknown op). Carries the reported error string.
    Refused(String),
    /// The radio could not be reached, did not reply in time, or the reply was
    /// unusable. Retriable: the service may simply not be up yet.
    Unavailable(String),
    /// The payload does not fit one aux frame. Never truncated: a partial
    /// payload would decode as corruption on the far side rather than as our
    /// bug. Carries the offending payload length.
    TooLarge(usize),
    /// The datagram could not be handed to the kernel.
    Send(String),
}

impl std::fmt::Display for AuxEgressError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Disabled => write!(f, "auxiliary stream disabled by operator"),
            Self::Refused(e) => write!(f, "auxiliary open refused: {e}"),
            Self::Unavailable(e) => write!(f, "auxiliary stream unavailable: {e}"),
            Self::TooLarge(n) => write!(f, "payload of {n} bytes exceeds one aux frame"),
            Self::Send(e) => write!(f, "auxiliary datagram send failed: {e}"),
        }
    }
}

impl std::error::Error for AuxEgressError {}

/// A producer's handle on the auxiliary application stream.
///
/// Opens lazily on first send and keeps the egress socket for subsequent ones.
/// A send that fails drops the socket so the next send re-opens the stream
/// rather than blackholing into a stale handle.
pub struct AuxEgress {
    /// The radio's aux command socket (`radio-aux.sock` under the run dir).
    cmd_sock: PathBuf,
    /// Bound on each command-socket round trip.
    request_timeout: Duration,
    /// The connected egress socket, `None` until the pair is open. The mutex
    /// serialises sends; the open handshake deliberately runs outside it so a
    /// hung radio stalls only its own caller.
    conn: Mutex<Option<UdpSocket>>,
}

impl AuxEgress {
    /// A client against the given aux command socket.
    pub fn new(cmd_sock: impl Into<PathBuf>) -> Self {
        Self::with_timeout(cmd_sock, AUX_REQUEST_TIMEOUT)
    }

    /// A client with an explicit round-trip bound (tests use a short one so a
    /// wedged-peer case does not cost seconds).
    pub fn with_timeout(cmd_sock: impl Into<PathBuf>, request_timeout: Duration) -> Self {
        Self {
            cmd_sock: cmd_sock.into(),
            request_timeout,
            conn: Mutex::new(None),
        }
    }

    /// Test/dev: a client already connected to a UDP target, skipping the
    /// handshake, so the framing and emission path can be exercised over plain
    /// loopback with no radio and no command socket.
    pub fn connected_for_test(sock: UdpSocket) -> Self {
        Self {
            cmd_sock: PathBuf::new(),
            request_timeout: AUX_REQUEST_TIMEOUT,
            conn: Mutex::new(Some(sock)),
        }
    }

    /// Production: a client connected directly to a UDP target port, skipping
    /// the radio command-socket handshake. Used on a ground station where the
    /// `wfb_tx -p3 -u<port>` process is already running (spawned by the
    /// groundlink receive chain), so the aux TX ingress is a plain UDP port
    /// with no `radio-aux.sock` command socket to negotiate through.
    ///
    /// The drone side still uses the command-socket path ([`Self::new`]) because
    /// the radio service owns the open/close lifecycle there.
    pub async fn connected_to_udp(target_port: u16) -> Result<Self, AuxEgressError> {
        let sock = tokio::net::UdpSocket::bind("127.0.0.1:0")
            .await
            .map_err(|e| AuxEgressError::Unavailable(e.to_string()))?;
        sock.connect(("127.0.0.1", target_port))
            .await
            .map_err(|e| AuxEgressError::Unavailable(e.to_string()))?;
        Ok(Self {
            cmd_sock: PathBuf::new(),
            request_timeout: AUX_REQUEST_TIMEOUT,
            conn: Mutex::new(Some(sock)),
        })
    }

    /// Whether the egress socket is currently held open by this client.
    pub async fn is_open(&self) -> bool {
        self.conn.lock().await.is_some()
    }

    /// One newline-JSON request/response round trip, bounded by the configured
    /// timeout.
    async fn request(&self, op: &str) -> Result<serde_json::Value, AuxEgressError> {
        let work = async {
            let stream = UnixStream::connect(&self.cmd_sock)
                .await
                .map_err(|e| AuxEgressError::Unavailable(e.to_string()))?;
            let (rx, mut tx) = stream.into_split();
            tx.write_all(format!("{{\"op\":\"{op}\"}}\n").as_bytes())
                .await
                .map_err(|e| AuxEgressError::Unavailable(e.to_string()))?;
            let mut line = String::new();
            BufReader::new(rx)
                .read_line(&mut line)
                .await
                .map_err(|e| AuxEgressError::Unavailable(e.to_string()))?;
            serde_json::from_str(&line).map_err(|e| AuxEgressError::Unavailable(e.to_string()))
        };
        tokio::time::timeout(self.request_timeout, work)
            .await
            .map_err(|_| AuxEgressError::Unavailable("command socket timed out".into()))?
    }

    /// Whether the radio reports the aux pair already up, without opening it.
    pub async fn is_active(&self) -> bool {
        if self.is_open().await {
            return true;
        }
        matches!(
            self.request("status").await,
            Ok(v) if v.get("active").and_then(|x| x.as_bool()) == Some(true)
        )
    }

    /// Ensure the aux pair is up and the egress socket is connected.
    ///
    /// Idempotent and cheap once open. The handshake runs without the conn lock
    /// held, so a slow radio never blocks a concurrent send; a racing opener
    /// that wins keeps its socket and the loser drops its own (both target the
    /// same port, so either is correct).
    pub async fn ensure_open(&self) -> Result<(), AuxEgressError> {
        if self.is_open().await {
            return Ok(());
        }
        let reply = self.request("open").await?;
        if reply.get("ok").and_then(|v| v.as_bool()) != Some(true) {
            let err = reply
                .get("error")
                .and_then(|v| v.as_str())
                .unwrap_or("unspecified");
            return Err(if err == E_AUX_DISABLED {
                AuxEgressError::Disabled
            } else {
                AuxEgressError::Refused(err.to_string())
            });
        }
        if reply.get("active").and_then(|v| v.as_bool()) != Some(true) {
            return Err(AuxEgressError::Refused("open reported inactive".into()));
        }
        let tx_port: u16 = reply
            .get("tx_port")
            .and_then(|v| v.as_u64())
            .and_then(|v| u16::try_from(v).ok())
            .filter(|p| *p != 0)
            .ok_or_else(|| AuxEgressError::Refused("open reported no transmit port".into()))?;

        let sock = UdpSocket::bind("127.0.0.1:0")
            .await
            .map_err(|e| AuxEgressError::Unavailable(e.to_string()))?;
        sock.connect(("127.0.0.1", tx_port))
            .await
            .map_err(|e| AuxEgressError::Unavailable(e.to_string()))?;
        let mut guard = self.conn.lock().await;
        if guard.is_none() {
            *guard = Some(sock);
        }
        Ok(())
    }

    /// One write attempt against the socket currently installed. `Ok(None)`
    /// means there was none to write to.
    async fn try_send_frame(&self, frame: &[u8]) -> Result<Option<()>, AuxEgressError> {
        let mut guard = self.conn.lock().await;
        let Some(sock) = guard.as_ref() else {
            return Ok(None);
        };
        match sock.send(frame).await {
            Ok(_) => Ok(Some(())),
            Err(e) => {
                // Self-heal: drop the dead socket so the next send re-opens.
                *guard = None;
                Err(AuxEgressError::Send(e.to_string()))
            }
        }
    }

    /// Frame `payload` for `channel` and emit it as one aux datagram.
    ///
    /// An oversized payload is rejected BEFORE anything is opened, so a producer
    /// that only ever emits oversized frames never brings the radio pair up.
    /// `Ok` means the datagram entered the kernel — see the module note on what
    /// that does not prove.
    pub async fn send(&self, channel: AuxChannel, payload: &[u8]) -> Result<(), AuxEgressError> {
        let frame =
            aux_mux::encode(channel, payload).ok_or(AuxEgressError::TooLarge(payload.len()))?;
        // `ensure_open` installs its socket under the same lock a write takes,
        // so it cannot be called with that lock held. That leaves a window in
        // which a SIBLING call's failed write nulls the socket between our open
        // and our write. That is the sibling's self-heal working, not this
        // call's failure, so re-open once rather than reporting a spurious
        // `Unavailable("socket closed")` to a caller that did nothing wrong.
        self.ensure_open().await?;
        if self.try_send_frame(&frame).await?.is_some() {
            return Ok(());
        }
        self.ensure_open().await?;
        self.try_send_frame(&frame)
            .await?
            .ok_or_else(|| AuxEgressError::Unavailable("socket closed".into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::UnixListener;

    /// A fake aux command socket that replies to `reply_count` requests with an
    /// active pair pointing at `tx_port`, then stops answering.
    fn fake_radio(
        sock_path: PathBuf,
        tx_port: u16,
        reply_count: usize,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let listener = UnixListener::bind(&sock_path).unwrap();
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

    /// A fake aux command socket that always refuses with the given error.
    fn refusing_radio(sock_path: PathBuf, error: &'static str) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let listener = UnixListener::bind(&sock_path).unwrap();
            while let Ok((stream, _)) = listener.accept().await {
                let (rx, mut tx) = stream.into_split();
                let mut line = String::new();
                let _ = BufReader::new(rx).read_line(&mut line).await;
                let _ = tx
                    .write_all(format!("{{\"ok\":false,\"error\":\"{error}\"}}\n").as_bytes())
                    .await;
            }
        })
    }

    #[tokio::test]
    async fn the_open_handshake_connects_to_the_replied_transmit_port() {
        let dir = tempfile::tempdir().unwrap();
        let sock_path = dir.path().join("radio-aux.sock");
        // Stands in for the loopback ingress wfb_tx would listen on.
        let egress_rx = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let tx_port = egress_rx.local_addr().unwrap().port();
        let radio = fake_radio(sock_path.clone(), tx_port, 4);
        tokio::time::sleep(Duration::from_millis(20)).await;

        let client = AuxEgress::new(&sock_path);
        client
            .send(AuxChannel::Mavlink, b"frame bytes")
            .await
            .unwrap();

        let mut buf = [0u8; 256];
        let (n, _) = egress_rx.recv_from(&mut buf).await.unwrap();
        let (channel, payload) = aux_mux::decode(&buf[..n]).unwrap();
        assert_eq!(channel, AuxChannel::Mavlink);
        assert_eq!(payload, b"frame bytes");
        radio.abort();
    }

    #[tokio::test]
    async fn the_stream_is_opened_once_and_reused() {
        // The fake answers exactly one request, so a second handshake would
        // hang and time out. Both sends must still arrive, proving the client
        // reuses the socket rather than re-opening per datagram.
        let dir = tempfile::tempdir().unwrap();
        let sock_path = dir.path().join("radio-aux.sock");
        let egress_rx = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let tx_port = egress_rx.local_addr().unwrap().port();
        let radio = fake_radio(sock_path.clone(), tx_port, 1);
        tokio::time::sleep(Duration::from_millis(20)).await;

        let client = AuxEgress::with_timeout(&sock_path, Duration::from_millis(100));
        client.send(AuxChannel::Status, b"one").await.unwrap();
        client.send(AuxChannel::Status, b"two").await.unwrap();

        let mut buf = [0u8; 64];
        let (n, _) = egress_rx.recv_from(&mut buf).await.unwrap();
        assert_eq!(aux_mux::decode(&buf[..n]).unwrap().1, b"one");
        let (n, _) = egress_rx.recv_from(&mut buf).await.unwrap();
        assert_eq!(aux_mux::decode(&buf[..n]).unwrap().1, b"two");
        radio.abort();
    }

    #[tokio::test]
    async fn a_disabled_lane_reports_disabled_and_emits_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let sock_path = dir.path().join("radio-aux.sock");
        let radio = refusing_radio(sock_path.clone(), E_AUX_DISABLED);
        tokio::time::sleep(Duration::from_millis(20)).await;

        let client = AuxEgress::with_timeout(&sock_path, Duration::from_millis(200));
        let err = client
            .send(AuxChannel::Mavlink, b"never radiated")
            .await
            .unwrap_err();
        assert_eq!(err, AuxEgressError::Disabled);
        assert!(!client.is_open().await, "nothing is held open");
        radio.abort();
    }

    #[tokio::test]
    async fn another_refusal_is_distinct_from_the_operator_dead_switch() {
        let dir = tempfile::tempdir().unwrap();
        let sock_path = dir.path().join("radio-aux.sock");
        let radio = refusing_radio(sock_path.clone(), "E_AUX_OPEN_FAILED");
        tokio::time::sleep(Duration::from_millis(20)).await;

        let client = AuxEgress::with_timeout(&sock_path, Duration::from_millis(200));
        let err = client.ensure_open().await.unwrap_err();
        assert_eq!(err, AuxEgressError::Refused("E_AUX_OPEN_FAILED".into()));
        radio.abort();
    }

    #[tokio::test]
    async fn an_oversized_payload_is_rejected_before_the_lane_is_opened() {
        // The command socket does not exist, so any open attempt would report
        // Unavailable. Getting TooLarge proves the size check ran first and no
        // handshake was attempted.
        let client =
            AuxEgress::with_timeout("/nonexistent/radio-aux.sock", Duration::from_millis(50));
        let too_big = vec![0u8; aux_mux::AUX_MAX_PAYLOAD + 1];
        let err = client
            .send(AuxChannel::Mavlink, &too_big)
            .await
            .unwrap_err();
        assert_eq!(err, AuxEgressError::TooLarge(aux_mux::AUX_MAX_PAYLOAD + 1));
    }

    #[tokio::test]
    async fn a_missing_radio_is_unavailable_not_a_panic() {
        let client =
            AuxEgress::with_timeout("/nonexistent/radio-aux.sock", Duration::from_millis(50));
        assert!(matches!(
            client.ensure_open().await,
            Err(AuxEgressError::Unavailable(_))
        ));
        assert!(!client.is_active().await);
    }

    #[tokio::test]
    async fn a_radio_that_never_replies_times_out_rather_than_parking_the_caller() {
        let dir = tempfile::tempdir().unwrap();
        let sock_path = dir.path().join("radio-aux.sock");
        let path = sock_path.clone();
        let radio = tokio::spawn(async move {
            let listener = UnixListener::bind(&path).unwrap();
            // Accept and hold the connection open, answering nothing.
            let mut held = Vec::new();
            while let Ok((stream, _)) = listener.accept().await {
                held.push(stream);
            }
        });
        tokio::time::sleep(Duration::from_millis(20)).await;

        let client = AuxEgress::with_timeout(&sock_path, Duration::from_millis(80));
        let started = std::time::Instant::now();
        let err = client.ensure_open().await.unwrap_err();
        assert!(matches!(err, AuxEgressError::Unavailable(_)));
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "the timeout must bound the call"
        );
        radio.abort();
    }
}
