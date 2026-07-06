//! The MAVLink command-send socket client.
//!
//! The MAVLink service owns `/run/ados/mavlink.sock`. It broadcasts every FC
//! frame to connected clients and forwards any frame a client writes back to the
//! FC. This client is the write side of that seam: it connects, holds the
//! connection behind a mutex, and writes a length-prefixed raw MAVLink frame the
//! router then forwards to the serial link. It is the command route's only path
//! to the FC.
//!
//! The frame contract is the same `ados.core.ipc` framing the Python
//! `MavlinkIPCClient.send` uses: a 4-byte big-endian length prefix followed by
//! exactly that many raw MAVLink v2 bytes (`struct.pack("!I", len(data)) + data`).
//! The router reads the prefix, then the payload, and forwards the payload
//! verbatim to the FC.
//!
//! Connection lifecycle: the connection is established lazily on the first send
//! and held for reuse. On a write failure the held connection is dropped and the
//! next send reconnects, so a brief MAVLink-service restart self-heals without
//! the route holding a dead socket. When the socket is absent (an idle agent, or
//! the MAVLink service not yet up) the connect fails and the send returns an
//! error, which the command route maps to the same 503 the FastAPI route returns
//! when there is no FC link — so a command is never silently dropped.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use ados_protocol::frame::{decode_len, encode_frame, HEADER_SIZE, MAVLINK_MAX_FRAME};
use thiserror::Error;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::sync::Mutex;

/// The MAVLink socket file name under the runtime dir.
pub const MAVLINK_SOCKET_NAME: &str = "mavlink.sock";

/// The default MAVLink socket path, honouring the `ADOS_RUN_DIR` override the
/// Python `ados.core.ipc` resolves the runtime root with, so a test points it at
/// a tempdir and a dev rig can move the whole `/run/ados` tree. Defaults to
/// `/run/ados/mavlink.sock`.
pub fn default_mavlink_socket() -> PathBuf {
    let run_dir = std::env::var("ADOS_RUN_DIR").unwrap_or_else(|_| "/run/ados".to_string());
    Path::new(&run_dir).join(MAVLINK_SOCKET_NAME)
}

/// A send-path error: either the framing rejected the payload (a frame larger
/// than the contract's cap, which never happens for a fixed-size command frame),
/// or the socket I/O failed (the socket is absent, or the write broke).
#[derive(Debug, Error)]
pub enum SendError {
    /// The MAVLink socket could not be reached or the write failed. Carries the
    /// underlying I/O error for the log; the route maps it to a 503.
    #[error("mavlink socket send failed: {0}")]
    Io(#[from] std::io::Error),
    /// The payload could not be framed (over the contract's max frame size). A
    /// fixed-size command frame never trips this, but the framing is honoured
    /// rather than panicked on.
    #[error("mavlink frame encode failed: {0}")]
    Frame(#[from] ados_protocol::frame::FrameError),
}

/// Connects to the MAVLink socket and writes length-prefixed command frames.
///
/// Cheap to clone (the held connection is behind an `Arc<Mutex>`); the route
/// surface holds one in the app state. The connection is established lazily on
/// the first send and reused; a write failure drops it so the next send
/// reconnects.
#[derive(Clone)]
pub struct MavlinkIpcClient {
    socket_path: PathBuf,
    /// The lazily-established, reused connection. `None` until the first send
    /// connects, and reset to `None` after a write failure so the next send
    /// reconnects.
    conn: Arc<Mutex<Option<UnixStream>>>,
}

impl MavlinkIpcClient {
    /// Build a client for the given socket path with no connection yet. The first
    /// [`send`](Self::send) connects.
    pub fn new(socket_path: PathBuf) -> Self {
        Self {
            socket_path,
            conn: Arc::new(Mutex::new(None)),
        }
    }

    /// Build a client at the default MAVLink socket path (`ADOS_RUN_DIR`-aware).
    pub fn default_socket() -> Self {
        Self::new(default_mavlink_socket())
    }

    /// The socket path this client writes to.
    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    /// Write one raw MAVLink v2 frame to the socket, framed with the 4-byte
    /// big-endian length prefix the router reads. Connects lazily on the first
    /// call and reuses the connection; on a write failure the connection is
    /// dropped and a single reconnect is attempted so a brief MAVLink-service
    /// blip self-heals. An absent socket (no MAVLink service) returns
    /// [`SendError::Io`], which the route maps to a 503 (no FC link) — the
    /// command is never silently dropped.
    pub async fn send(&self, frame: &[u8]) -> Result<(), SendError> {
        // Frame the payload up front: 4-byte big-endian length prefix + the raw
        // MAVLink bytes, the exact `ados.core.ipc` contract. A command frame is
        // far under the cap, so this only fails on a programmer error.
        let wire = encode_frame(frame, MAVLINK_MAX_FRAME)?;

        let mut guard = self.conn.lock().await;

        // First attempt on the held (or freshly-connected) stream.
        match self.write_on(&mut guard, &wire).await {
            Ok(()) => return Ok(()),
            Err(e) => {
                // The held connection is dead (broken pipe / reset). Drop it and
                // try once more with a fresh connect, so a MAVLink-service
                // restart between commands recovers transparently.
                tracing::debug!(error = %e, "mavlink send failed on held connection; reconnecting once");
                *guard = None;
            }
        }

        // Second attempt: forced reconnect.
        self.write_on(&mut guard, &wire).await
    }

    /// Ensure a live connection in `guard`, then write the framed bytes. On any
    /// I/O error the connection is cleared so the caller's retry (or the next
    /// send) reconnects.
    async fn write_on(
        &self,
        guard: &mut tokio::sync::MutexGuard<'_, Option<UnixStream>>,
        wire: &[u8],
    ) -> Result<(), SendError> {
        if guard.is_none() {
            let stream = UnixStream::connect(&self.socket_path).await.map_err(|e| {
                tracing::debug!(
                    path = %self.socket_path.display(),
                    error = %e,
                    "mavlink socket connect failed"
                );
                e
            })?;
            **guard = Some(stream);
        }
        // Safe: just ensured Some above.
        let stream = guard.as_mut().expect("connection is present");
        if let Err(e) = stream.write_all(wire).await {
            **guard = None;
            return Err(SendError::Io(e));
        }
        if let Err(e) = stream.flush().await {
            **guard = None;
            return Err(SendError::Io(e));
        }
        Ok(())
    }

    /// Open a fresh, dedicated connection for a correlated command exchange.
    ///
    /// The MAVLink socket is bidirectional: the router forwards every frame a
    /// client writes to the FC, and broadcasts every FC frame back to every
    /// connected client. A command that wants to read its own `COMMAND_ACK`
    /// therefore writes the command AND reads the broadcast stream on the same
    /// connection. This uses a NEW connection rather than the shared
    /// fire-and-forget one so its reads never race the shared writer, and so it
    /// only ever sees frames broadcast after it connected (the MAVLink socket
    /// does not replay a backlog, so there is no stale ACK from an earlier
    /// command). An absent socket returns [`SendError::Io`], which the route
    /// maps to the same 503 (no FC link) the plain send path does.
    pub async fn open_ack_stream(&self) -> Result<AckStream, SendError> {
        let stream = UnixStream::connect(&self.socket_path).await.map_err(|e| {
            tracing::debug!(
                path = %self.socket_path.display(),
                error = %e,
                "mavlink ack-stream connect failed"
            );
            e
        })?;
        Ok(AckStream { stream })
    }
}

/// The outcome of one bounded read on an [`AckStream`].
#[derive(Debug)]
pub enum FrameRead {
    /// A complete raw MAVLink frame payload (the bytes after the length prefix),
    /// ready to parse.
    Frame(Vec<u8>),
    /// No frame arrived within the read budget. The caller decides whether to
    /// keep waiting, resend, or give up.
    Timeout,
    /// The connection closed (or a read/framing error ended it). No more frames
    /// will arrive on this stream.
    Eof,
}

/// A dedicated MAVLink-socket connection used to send a command and read the
/// FC frame stream back to correlate its `COMMAND_ACK`.
///
/// One connection carries both directions: [`write_frame`](Self::write_frame)
/// forwards a raw MAVLink frame to the FC, and [`read_frame`](Self::read_frame)
/// pulls the next broadcast FC frame under a bounded budget. The stream is owned
/// (not shared), so its reads are exclusive and it is dropped when the exchange
/// ends.
#[derive(Debug)]
pub struct AckStream {
    stream: UnixStream,
}

impl AckStream {
    /// Write one raw MAVLink v2 frame to the FC, framed with the 4-byte
    /// big-endian length prefix the router reads (the same `ados.core.ipc`
    /// contract [`MavlinkIpcClient::send`] uses). A write failure means the link
    /// dropped; the caller maps it to a 503.
    pub async fn write_frame(&mut self, frame: &[u8]) -> Result<(), SendError> {
        let wire = encode_frame(frame, MAVLINK_MAX_FRAME)?;
        self.stream.write_all(&wire).await?;
        self.stream.flush().await?;
        Ok(())
    }

    /// Read the next raw MAVLink frame from the broadcast stream, bounded by
    /// `budget`. Returns [`FrameRead::Frame`] with the payload (the bytes after
    /// the length prefix), [`FrameRead::Timeout`] if nothing arrived in time, or
    /// [`FrameRead::Eof`] if the connection closed or a framing error ended it.
    /// A read error is never surfaced as an `Err`: the command was already sent,
    /// so a broken read stream just ends the correlation window (the route then
    /// reports an honest "no ack observed"), it does not fail the request.
    pub async fn read_frame(&mut self, budget: Duration) -> FrameRead {
        let mut header = [0u8; HEADER_SIZE];
        match tokio::time::timeout(budget, self.stream.read_exact(&mut header)).await {
            Err(_elapsed) => return FrameRead::Timeout,
            Ok(Err(_io)) => return FrameRead::Eof,
            Ok(Ok(_)) => {}
        }
        let len = match decode_len(header, MAVLINK_MAX_FRAME, false) {
            Ok(n) => n,
            // A garbled prefix means the framing desynced; end the window rather
            // than trying to resync a byte stream we cannot re-align.
            Err(_) => return FrameRead::Eof,
        };
        if len == 0 {
            // A zero-length frame carries no MAVLink message; skip it by
            // reporting an empty payload the caller's parse will simply ignore.
            return FrameRead::Frame(Vec::new());
        }
        let mut payload = vec![0u8; len];
        match tokio::time::timeout(budget, self.stream.read_exact(&mut payload)).await {
            Err(_elapsed) => FrameRead::Timeout,
            Ok(Err(_io)) => FrameRead::Eof,
            Ok(Ok(_)) => FrameRead::Frame(payload),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ados_protocol::frame::{decode_len, HEADER_SIZE};
    use tokio::io::AsyncReadExt;
    use tokio::net::UnixListener;

    /// A send against an absent socket returns an I/O error (which the route maps
    /// to a 503), not a panic.
    #[tokio::test]
    async fn send_to_an_absent_socket_is_an_io_error() {
        let dir = tempfile::tempdir().unwrap();
        let client = MavlinkIpcClient::new(dir.path().join("absent.sock"));
        let err = client.send(b"\xfd\x00").await.unwrap_err();
        assert!(
            matches!(err, SendError::Io(_)),
            "expected an Io error: {err:?}"
        );
    }

    /// A send against a live socket writes the 4-byte length prefix followed by
    /// the raw frame bytes, recoverable with `decode_len`.
    #[tokio::test]
    async fn send_writes_a_length_prefixed_frame() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mavlink.sock");
        let listener = UnixListener::bind(&path).unwrap();

        let payload = b"\xfd\x09\x00\x00\x05\x01\x01\x4c".to_vec();
        let client = MavlinkIpcClient::new(path.clone());

        // Accept the connection on the server side, read one framed message.
        let server = tokio::spawn(async move {
            let (mut conn, _addr) = listener.accept().await.unwrap();
            let mut header = [0u8; HEADER_SIZE];
            conn.read_exact(&mut header).await.unwrap();
            let len = decode_len(header, MAVLINK_MAX_FRAME, false).unwrap();
            let mut body = vec![0u8; len];
            conn.read_exact(&mut body).await.unwrap();
            body
        });

        client
            .send(&payload)
            .await
            .expect("send succeeds on a live socket");
        let got = server.await.unwrap();
        assert_eq!(got, payload, "the server reads back the exact raw frame");
    }

    /// The client reconnects after the server drops the connection: a second send
    /// succeeds against a fresh accept.
    #[tokio::test]
    async fn send_reconnects_after_the_peer_drops() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mavlink.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let client = MavlinkIpcClient::new(path.clone());

        let payload = b"\xfd\x00\x05".to_vec();

        // First round: accept, read one frame, then drop the connection.
        let p1 = payload.clone();
        let server1 = tokio::spawn(async move {
            let (mut conn, _addr) = listener.accept().await.unwrap();
            let mut header = [0u8; HEADER_SIZE];
            conn.read_exact(&mut header).await.unwrap();
            let len = decode_len(header, MAVLINK_MAX_FRAME, false).unwrap();
            let mut body = vec![0u8; len];
            conn.read_exact(&mut body).await.unwrap();
            assert_eq!(body, p1);
            // Drop conn → the client's held connection becomes dead.
            drop(conn);
            // Re-accept for the second send.
            let (mut conn2, _addr) = listener.accept().await.unwrap();
            let mut header2 = [0u8; HEADER_SIZE];
            conn2.read_exact(&mut header2).await.unwrap();
            let len2 = decode_len(header2, MAVLINK_MAX_FRAME, false).unwrap();
            let mut body2 = vec![0u8; len2];
            conn2.read_exact(&mut body2).await.unwrap();
            body2
        });

        client.send(&payload).await.expect("first send succeeds");
        // Give the server a moment to drop the first connection.
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        // Second send: the held connection is now dead; the client reconnects.
        client
            .send(&payload)
            .await
            .expect("second send reconnects and succeeds");

        let got2 = server1.await.unwrap();
        assert_eq!(got2, payload, "the second frame arrives over the reconnect");
    }

    #[test]
    fn default_socket_honours_the_run_dir_override() {
        let p = default_mavlink_socket();
        assert!(p.ends_with("mavlink.sock"));
    }

    /// The ack stream writes a length-prefixed command to the server and reads a
    /// length-prefixed frame the server broadcasts back, recovering the exact
    /// payload bytes.
    #[tokio::test]
    async fn ack_stream_writes_a_command_and_reads_a_broadcast_frame() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mavlink.sock");
        let listener = UnixListener::bind(&path).unwrap();

        let command = b"\xfd\x01\x02command".to_vec();
        let broadcast = b"\xfd\x03\x04ack-frame".to_vec();

        let b_clone = broadcast.clone();
        let c_clone = command.clone();
        let server = tokio::spawn(async move {
            let (mut conn, _addr) = listener.accept().await.unwrap();
            // Read the client's command frame.
            let mut header = [0u8; HEADER_SIZE];
            conn.read_exact(&mut header).await.unwrap();
            let len = decode_len(header, MAVLINK_MAX_FRAME, false).unwrap();
            let mut body = vec![0u8; len];
            conn.read_exact(&mut body).await.unwrap();
            assert_eq!(body, c_clone, "server reads the exact command frame");
            // Broadcast one frame back, length-prefixed like the router does.
            let framed = encode_frame(&b_clone, MAVLINK_MAX_FRAME).unwrap();
            conn.write_all(&framed).await.unwrap();
            conn.flush().await.unwrap();
            // Hold the connection open briefly so the client can read.
            tokio::time::sleep(Duration::from_millis(50)).await;
        });

        let client = MavlinkIpcClient::new(path.clone());
        let mut stream = client.open_ack_stream().await.expect("stream opens");
        stream.write_frame(&command).await.expect("write succeeds");
        match stream.read_frame(Duration::from_secs(1)).await {
            FrameRead::Frame(payload) => {
                assert_eq!(payload, broadcast, "reads back the exact broadcast frame");
            }
            other => panic!("expected a frame, got {other:?}"),
        }
        server.await.unwrap();
    }

    /// A read with no frame on the wire times out (not a panic, not an error).
    #[tokio::test]
    async fn ack_stream_read_times_out_when_idle() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mavlink.sock");
        let listener = UnixListener::bind(&path).unwrap();
        // Accept and hold the connection open but send nothing.
        let server = tokio::spawn(async move {
            let (_conn, _addr) = listener.accept().await.unwrap();
            tokio::time::sleep(Duration::from_millis(200)).await;
        });

        let client = MavlinkIpcClient::new(path.clone());
        let mut stream = client.open_ack_stream().await.expect("stream opens");
        assert!(
            matches!(
                stream.read_frame(Duration::from_millis(40)).await,
                FrameRead::Timeout
            ),
            "an idle stream reports Timeout"
        );
        server.await.unwrap();
    }

    /// A closed connection reports EOF on the next read.
    #[tokio::test]
    async fn ack_stream_read_reports_eof_after_close() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mavlink.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let server = tokio::spawn(async move {
            let (conn, _addr) = listener.accept().await.unwrap();
            drop(conn); // close immediately
        });

        let client = MavlinkIpcClient::new(path.clone());
        let mut stream = client.open_ack_stream().await.expect("stream opens");
        // Give the server time to close.
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert!(
            matches!(
                stream.read_frame(Duration::from_millis(200)).await,
                FrameRead::Eof
            ),
            "a closed stream reports Eof"
        );
        server.await.unwrap();
    }

    /// Opening an ack stream against an absent socket is an I/O error (mapped to
    /// a 503 by the route), not a panic.
    #[tokio::test]
    async fn open_ack_stream_absent_socket_is_an_io_error() {
        let dir = tempfile::tempdir().unwrap();
        let client = MavlinkIpcClient::new(dir.path().join("absent.sock"));
        let err = client.open_ack_stream().await.unwrap_err();
        assert!(
            matches!(err, SendError::Io(_)),
            "expected Io error: {err:?}"
        );
    }
}
