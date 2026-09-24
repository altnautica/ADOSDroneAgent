//! The MAVLink command-send socket client.
//!
//! The MAVLink service owns `/run/ados/mavlink.sock`. It broadcasts every FC
//! frame to connected clients and forwards any frame a client writes back to the
//! FC. This client is the write side of that seam: it writes a length-prefixed
//! raw MAVLink frame the router then forwards to the serial link. It is the
//! command routes' only path to the FC.
//!
//! The frame contract is the same `ados.core.ipc` framing the Python
//! `MavlinkIPCClient.send` uses: a 4-byte big-endian length prefix followed by
//! exactly that many raw MAVLink v2 bytes (`struct.pack("!I", len(data)) + data`).
//! The router reads the prefix, then the payload, and forwards the payload
//! verbatim to the FC.
//!
//! Connection lifecycle: a fire-and-forget send opens a connection, writes the
//! frame and closes it, all within [`SEND_TIMEOUT`]. Holding one connection open
//! between sends would make this client a broadcast subscriber that never reads:
//! the router would fill its queue with FC frames, evict it as a slow consumer
//! (counting a false eviction on the health counter) and the next send would hit
//! a dead socket. When the socket is absent (an idle agent, or the MAVLink
//! service not yet up) the connect fails and the send returns an error, which
//! the routes map to a 503 — so a command is never silently dropped.

use std::path::{Path, PathBuf};
use std::time::Duration;

use ados_protocol::frame::{decode_len, encode_frame, HEADER_SIZE, MAVLINK_MAX_FRAME};
use thiserror::Error;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

/// The deadline for writing one command frame to the router. The router drains
/// inbound frames continuously, so a write that cannot complete in this window
/// means the router is wedged.
pub const SEND_TIMEOUT: Duration = Duration::from_secs(3);

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
/// Cheap to clone (just the socket path); the route surface holds one in the
/// app state.
#[derive(Clone)]
pub struct MavlinkIpcClient {
    socket_path: PathBuf,
}

impl MavlinkIpcClient {
    /// Build a client for the given socket path.
    pub fn new(socket_path: PathBuf) -> Self {
        Self { socket_path }
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
    /// big-endian length prefix the router reads, on a connection opened for this
    /// frame and closed after it. Bounded by [`SEND_TIMEOUT`]. An absent socket or
    /// a stalled write returns [`SendError::Io`], which the routes map to a 503.
    pub async fn send(&self, frame: &[u8]) -> Result<(), SendError> {
        let wire = encode_frame(frame, MAVLINK_MAX_FRAME)?;
        let exchange = async {
            let mut stream = UnixStream::connect(&self.socket_path).await.map_err(|e| {
                tracing::debug!(
                    path = %self.socket_path.display(),
                    error = %e,
                    "mavlink socket connect failed"
                );
                e
            })?;
            stream.write_all(&wire).await?;
            stream.flush().await?;
            Ok::<(), std::io::Error>(())
        };
        match tokio::time::timeout(SEND_TIMEOUT, exchange).await {
            Ok(r) => r.map_err(SendError::Io),
            Err(_) => Err(SendError::Io(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "the MAVLink router did not take the frame in time",
            ))),
        }
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
        Ok(AckStream {
            stream,
            desynced: false,
        })
    }
}

/// The outcome of one bounded read on an [`AckStream`].
#[derive(Debug)]
pub enum FrameRead {
    /// A complete raw MAVLink frame payload (the bytes after the length prefix),
    /// ready to parse.
    Frame(Vec<u8>),
    /// No frame arrived within the read budget. The bounded read was cancelled
    /// mid-frame, so the stream it came from is retired (see [`AckStream`]): the
    /// caller resends on a FRESH stream or gives up, it never reads this one
    /// again.
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
///
/// ## One bounded read that expires retires the stream
///
/// `read_frame` bounds `read_exact` with `tokio::time::timeout`, and `read_exact`
/// is explicitly NOT cancellation-safe: when the timeout fires, the future is
/// dropped after having possibly consumed part of a length prefix or part of a
/// payload, and those bytes are gone. The stream is then at an unknown offset
/// inside a frame, and a raw byte stream carries no resync marker to recover
/// with — so every later read parses a shifted window and silently misparses
/// real frames. On the command path that meant an ACCEPTED arm / disarm /
/// mode-set ACK was missed and the request reported "no ack observed" for a
/// command the FC had executed.
///
/// So a timeout marks the stream `desynced` and every subsequent call refuses:
/// reads report [`FrameRead::Eof`] (this connection will never yield another
/// parseable frame) and writes error. The caller's recovery is to open a fresh
/// stream, which is what [`crate::routes`]' command retry loop does per attempt.
#[derive(Debug)]
pub struct AckStream {
    stream: UnixStream,
    /// Set once a bounded read was cancelled mid-frame. The stream is at an
    /// unknown byte offset from that point on and must not be read or written
    /// again — only replaced.
    desynced: bool,
}

impl AckStream {
    /// Write one raw MAVLink v2 frame to the FC, framed with the 4-byte
    /// big-endian length prefix the router reads (the same `ados.core.ipc`
    /// contract [`MavlinkIpcClient::send`] uses). A write failure means the link
    /// dropped; the caller maps it to a 503.
    ///
    /// Refuses on a desynced stream. The write direction is technically still
    /// intact, but a command written here is one whose ACK this stream can no
    /// longer read, so sending it would repeat the flight command while
    /// guaranteeing the caller cannot observe the result.
    pub async fn write_frame(&mut self, frame: &[u8]) -> Result<(), SendError> {
        if self.desynced {
            return Err(SendError::Io(std::io::Error::other(
                "ack stream desynced by a cancelled read; open a fresh stream",
            )));
        }
        let wire = encode_frame(frame, MAVLINK_MAX_FRAME)?;
        let write = async {
            self.stream.write_all(&wire).await?;
            self.stream.flush().await
        };
        match tokio::time::timeout(SEND_TIMEOUT, write).await {
            Ok(r) => r.map_err(SendError::Io),
            Err(_) => {
                // A cancelled write leaves the stream at an unknown offset.
                self.desynced = true;
                Err(SendError::Io(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "the MAVLink router did not take the frame in time",
                )))
            }
        }
    }

    /// Read the next raw MAVLink frame from the broadcast stream, bounded by
    /// `budget`. Returns [`FrameRead::Frame`] with the payload (the bytes after
    /// the length prefix), [`FrameRead::Timeout`] if nothing arrived in time, or
    /// [`FrameRead::Eof`] if the connection closed, a framing error ended it, or
    /// an earlier timeout already retired it.
    /// A read error is never surfaced as an `Err`: the command was already sent,
    /// so a broken read stream just ends the correlation window (the route then
    /// reports an honest "no ack observed"), it does not fail the request.
    ///
    /// A `Timeout` return is terminal for this stream — see the type docs.
    pub async fn read_frame(&mut self, budget: Duration) -> FrameRead {
        if self.desynced {
            return FrameRead::Eof;
        }
        let mut header = [0u8; HEADER_SIZE];
        match tokio::time::timeout(budget, self.stream.read_exact(&mut header)).await {
            Err(_elapsed) => {
                self.desynced = true;
                return FrameRead::Timeout;
            }
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
            Err(_elapsed) => {
                self.desynced = true;
                FrameRead::Timeout
            }
            Ok(Err(_io)) => FrameRead::Eof,
            Ok(Ok(_)) => FrameRead::Frame(payload),
        }
    }

    /// Whether a cancelled read has retired this stream. The command loop opens a
    /// fresh stream per attempt, so this is the property a test asserts rather
    /// than a branch production code takes.
    pub fn is_desynced(&self) -> bool {
        self.desynced
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

    /// Each send uses its own connection and closes it, so the client is never
    /// left attached to the broadcast as a consumer that does not read.
    #[tokio::test]
    async fn each_send_closes_its_connection() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mavlink.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let client = MavlinkIpcClient::new(path.clone());

        let server = tokio::spawn(async move {
            for _ in 0..2 {
                let (mut conn, _addr) = listener.accept().await.unwrap();
                let mut header = [0u8; HEADER_SIZE];
                conn.read_exact(&mut header).await.unwrap();
                let len = decode_len(header, MAVLINK_MAX_FRAME, false).unwrap();
                let mut body = vec![0u8; len];
                conn.read_exact(&mut body).await.unwrap();
                // The client closed after its frame: the next read is EOF.
                let mut rest = [0u8; 1];
                assert_eq!(conn.read(&mut rest).await.unwrap(), 0);
            }
        });
        client.send(b"\xfd\x00\x05").await.expect("first send");
        client.send(b"\xfd\x00\x06").await.expect("second send");
        server.await.unwrap();
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

    /// A frame that arrives AFTER the read budget expired must never be parsed
    /// off the timed-out stream.
    ///
    /// This is the desync: `read_exact` is not cancellation-safe, so the expired
    /// read may already have eaten part of the length prefix. The bytes that land
    /// next are therefore at an unknown offset, and parsing them produced a
    /// misread ACK — the command route then reported "no ack observed" for an arm
    /// or mode-set the FC had accepted. Asserted behaviourally: the server sends
    /// a complete, well-formed frame after the timeout, and the stream still
    /// refuses to yield it.
    #[tokio::test]
    async fn a_frame_arriving_after_a_timeout_is_never_read_off_that_stream() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mavlink.sock");
        let listener = UnixListener::bind(&path).unwrap();

        let late = b"\xfd\x03\x04late-frame".to_vec();
        let late_w = late.clone();
        let server = tokio::spawn(async move {
            let (mut conn, _addr) = listener.accept().await.unwrap();
            // Stay silent past the client's read budget, then send a good frame.
            tokio::time::sleep(Duration::from_millis(120)).await;
            let framed = encode_frame(&late_w, MAVLINK_MAX_FRAME).unwrap();
            conn.write_all(&framed).await.unwrap();
            conn.flush().await.unwrap();
            tokio::time::sleep(Duration::from_millis(100)).await;
        });

        let client = MavlinkIpcClient::new(path.clone());
        let mut stream = client.open_ack_stream().await.expect("stream opens");
        assert!(
            matches!(
                stream.read_frame(Duration::from_millis(30)).await,
                FrameRead::Timeout
            ),
            "the idle window must report Timeout"
        );
        assert!(stream.is_desynced(), "a timed-out read retires the stream");

        // The late frame is now on the wire. A read must NOT hand it back.
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert!(
            matches!(
                stream.read_frame(Duration::from_millis(200)).await,
                FrameRead::Eof
            ),
            "a retired stream reports Eof, never a frame read at an unknown offset"
        );
        // And a resend on the retired stream is refused rather than duplicating a
        // flight command whose ACK could not be observed.
        assert!(
            stream.write_frame(&late).await.is_err(),
            "a retired stream refuses writes"
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
