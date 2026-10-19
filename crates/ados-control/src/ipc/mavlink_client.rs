//! The MAVLink command-send socket client.
//!
//! The MAVLink service owns `/run/ados/mavlink.sock`. It broadcasts every FC
//! frame to connected clients and forwards any frame a client writes back to the
//! FC. This client is the write side of that seam: it writes a length-prefixed
//! raw MAVLink frame the router then forwards to the serial link. It is the
//! command routes' only path to the FC.
//!
//! The frame contract is the router's MAVLink-socket framing
//! ([`ados_protocol::mavlink_socket`]): every record is a 4-byte big-endian
//! length prefix and that many bytes. A client writes raw MAVLink frames; the
//! router forwards the payload verbatim to the FC. A record the router
//! broadcasts starts with its 8-byte receive-time header before the frame.
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
use ados_protocol::mavlink_socket::{
    decode_body, MAVLINK_SOCKET_HEADER_LEN, MAVLINK_SOCKET_MAX_BODY,
};
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
    /// A complete raw MAVLink frame (the record body after the receive-time
    /// header), ready to parse.
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
    /// `budget`. Returns [`FrameRead::Frame`] with the frame (the record body
    /// after its receive-time header), [`FrameRead::Timeout`] if nothing arrived
    /// in time, or [`FrameRead::Eof`] if the connection closed, a framing error
    /// ended it, or an earlier timeout already retired it.
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
        let len = match decode_len(header, MAVLINK_SOCKET_MAX_BODY, false) {
            Ok(n) if n >= MAVLINK_SOCKET_HEADER_LEN => n,
            // A garbled prefix, or a body too short for the receive-time header,
            // means the framing desynced; end the window rather than trying to
            // resync a byte stream we cannot re-align.
            _ => return FrameRead::Eof,
        };
        let mut body = vec![0u8; len];
        match tokio::time::timeout(budget, self.stream.read_exact(&mut body)).await {
            Err(_elapsed) => {
                self.desynced = true;
                FrameRead::Timeout
            }
            Ok(Err(_io)) => FrameRead::Eof,
            Ok(Ok(_)) => match decode_body(&body) {
                Some((_rx_mono_ns, frame)) => FrameRead::Frame(frame.to_vec()),
                None => FrameRead::Eof,
            },
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
mod tests;
