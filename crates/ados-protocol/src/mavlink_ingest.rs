//! Client for a node's MAVLink republish seam.
//!
//! A node's MAVLink router owns the frame fan-out that feeds its IPC socket and
//! its direct-GCS transports. Normally that fan-out is fed by the router's own
//! flight-controller link. A ground station has no flight controller: the
//! vehicle's frames arrive over the radio's auxiliary lane, are decoded by the
//! ground data plane, and then have to get into that same fan-out so a ground
//! control station connected to the ground station sees the vehicle exactly as
//! it would on a direct link.
//!
//! Nothing carried frames in that direction. The router's existing socket
//! inbound path is the opposite direction — bytes a client sends there are
//! written toward the flight controller — so reusing it would push the
//! vehicle's own telemetry back at the autopilot. This module is the missing
//! direction: a separate socket whose frames are published to the fan-out and
//! nowhere else.
//!
//! ## Wire
//!
//! The [`crate::frame`] primitive: a 4-byte big-endian length followed by one
//! raw MAVLink frame, identical to the router's other frame socket, so a reader
//! written against one cannot misparse the other.
//!
//! ## What a successful send does and does not prove
//!
//! `Ok` means the framed bytes entered the socket. It does NOT mean the router
//! accepted the frame, published it, or that any ground control station was
//! connected to receive it. The proof of republish is the router's own accepted
//! counter, on the far side of this socket.
//!
//! ## Bounded by construction
//!
//! Every operation is bounded by [`INGEST_WRITE_TIMEOUT`], so a wedged or
//! stalled router costs a caller one timeout rather than parking it forever.
//! Callers on a live data path are expected to drop the frame on a timeout, not
//! queue it: this lane is a convenience over a radio, never a reason to stall.

use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::io::AsyncWriteExt;
use tokio::net::UnixStream;
use tokio::sync::Mutex;

use crate::frame::{encode_frame, FrameError, MAVLINK_MAX_FRAME};

/// Socket file name under the agent run directory.
pub const MAVLINK_INGEST_SOCK_NAME: &str = "mavlink-ingest.sock";

/// Default agent run directory, overridden by `ADOS_RUN_DIR`.
const DEFAULT_RUN_DIR: &str = "/run/ados";

/// Bound on one connect or one framed write.
///
/// Short on purpose. The peer is a local process on the same box, so anything
/// slower than this is a stall rather than latency, and a data-path caller must
/// find that out quickly enough to drop the frame instead of backing up behind
/// it.
pub const INGEST_WRITE_TIMEOUT: Duration = Duration::from_millis(500);

/// The republish socket path, honouring the `ADOS_RUN_DIR` override so a test
/// or a second instance does not collide with the running service.
pub fn ingest_socket_path() -> PathBuf {
    let dir = std::env::var("ADOS_RUN_DIR").unwrap_or_else(|_| DEFAULT_RUN_DIR.to_string());
    Path::new(&dir).join(MAVLINK_INGEST_SOCK_NAME)
}

/// Why a frame did not reach the republish seam.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IngestError {
    /// The frame is larger than the socket's frame cap. Never truncated: half a
    /// MAVLink frame fails its checksum downstream and reads as link corruption
    /// rather than as our bug. Carries the offending length.
    TooLarge(usize),
    /// The socket could not be reached. Retriable — the router may simply not
    /// be up yet, or may not serve this seam on this node's profile.
    Unavailable(String),
    /// The connection was established but the write failed or timed out.
    Send(String),
}

impl std::fmt::Display for IngestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooLarge(n) => write!(f, "frame of {n} bytes exceeds the socket frame cap"),
            Self::Unavailable(e) => write!(f, "republish seam unavailable: {e}"),
            Self::Send(e) => write!(f, "republish write failed: {e}"),
        }
    }
}

impl std::error::Error for IngestError {}

/// A producer's handle on a node's MAVLink republish seam.
///
/// Connects lazily on first send and keeps the connection for subsequent ones.
/// A failed send drops the connection so the next send reconnects rather than
/// writing into a dead handle forever.
pub struct MavlinkIngest {
    path: PathBuf,
    timeout: Duration,
    /// The connected stream, `None` until the first successful connect.
    ///
    /// The lock is held across the whole write. That is deliberate and load
    /// bearing: this is a length-prefixed byte stream, so two interleaved
    /// writes would splice one frame's payload into another's header and
    /// desynchronise the reader permanently. Every path holding it is bounded
    /// by `timeout`.
    conn: Mutex<Option<UnixStream>>,
}

impl MavlinkIngest {
    /// A client against the given socket path.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self::with_timeout(path, INGEST_WRITE_TIMEOUT)
    }

    /// A client against the run directory's standard socket path.
    pub fn at_default_path() -> Self {
        Self::new(ingest_socket_path())
    }

    /// A client with an explicit bound on connect and write, so a test does not
    /// pay the production timeout to prove the wedged-peer case.
    pub fn with_timeout(path: impl Into<PathBuf>, timeout: Duration) -> Self {
        Self {
            path: path.into(),
            timeout,
            conn: Mutex::new(None),
        }
    }

    /// Whether a connection is currently held open.
    pub async fn is_connected(&self) -> bool {
        self.conn.lock().await.is_some()
    }

    /// Send one raw MAVLink frame to the republish seam.
    ///
    /// An oversized frame is rejected before anything is connected, so a
    /// producer emitting only oversized frames never opens the socket at all.
    /// See the module note on what `Ok` does not prove.
    pub async fn send_frame(&self, frame: &[u8]) -> Result<(), IngestError> {
        let framed = encode_frame(frame, MAVLINK_MAX_FRAME).map_err(|e| match e {
            FrameError::TooLarge { len, .. } | FrameError::PayloadTooBig(len) => {
                IngestError::TooLarge(len)
            }
            // encode_frame never produces this variant; mapping it keeps the
            // match total without inventing a category for an impossible case.
            FrameError::ZeroLength => IngestError::TooLarge(0),
        })?;

        let mut guard = self.conn.lock().await;
        if guard.is_none() {
            let connect = tokio::time::timeout(self.timeout, UnixStream::connect(&self.path))
                .await
                .map_err(|_| IngestError::Unavailable("connect timed out".into()))?
                .map_err(|e| IngestError::Unavailable(e.to_string()))?;
            *guard = Some(connect);
        }

        let stream = guard.as_mut().expect("connected above");
        let write = tokio::time::timeout(self.timeout, stream.write_all(&framed)).await;
        match write {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => {
                // Self-heal: a half-written frame has desynchronised the
                // stream, so the connection is unusable and must be replaced,
                // not reused.
                *guard = None;
                Err(IngestError::Send(e.to_string()))
            }
            Err(_) => {
                *guard = None;
                Err(IngestError::Send("write timed out".into()))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt;
    use tokio::net::UnixListener;
    use tokio::sync::mpsc;

    /// A stand-in router: accepts one connection and decodes the length-prefixed
    /// frames off it onto a channel, exactly as the real seam's reader does.
    fn fake_router(path: PathBuf) -> (mpsc::Receiver<Vec<u8>>, tokio::task::JoinHandle<()>) {
        let (tx, rx) = mpsc::channel(16);
        let handle = tokio::spawn(async move {
            let listener = UnixListener::bind(&path).unwrap();
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            loop {
                let mut header = [0u8; crate::frame::HEADER_SIZE];
                if stream.read_exact(&mut header).await.is_err() {
                    return;
                }
                let len = u32::from_be_bytes(header) as usize;
                let mut payload = vec![0u8; len];
                if len > 0 && stream.read_exact(&mut payload).await.is_err() {
                    return;
                }
                if tx.send(payload).await.is_err() {
                    return;
                }
            }
        });
        (rx, handle)
    }

    #[tokio::test]
    async fn a_frame_arrives_byte_for_byte() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(MAVLINK_INGEST_SOCK_NAME);
        let (mut rx, router) = fake_router(path.clone());
        tokio::time::sleep(Duration::from_millis(20)).await;

        let client = MavlinkIngest::new(&path);
        let frame = vec![0xFD, 0x09, 0x00, 0x00, 0x42, 0x01, 0x01, 0x00, 0x00, 0x00];
        client.send_frame(&frame).await.unwrap();

        assert_eq!(rx.recv().await.unwrap(), frame);
        router.abort();
    }

    #[tokio::test]
    async fn the_connection_is_opened_once_and_reused() {
        // The fake accepts exactly one connection, so a client that reconnected
        // per frame would lose the second frame.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(MAVLINK_INGEST_SOCK_NAME);
        let (mut rx, router) = fake_router(path.clone());
        tokio::time::sleep(Duration::from_millis(20)).await;

        let client = MavlinkIngest::new(&path);
        client.send_frame(b"first").await.unwrap();
        client.send_frame(b"second").await.unwrap();

        assert_eq!(rx.recv().await.unwrap(), b"first");
        assert_eq!(rx.recv().await.unwrap(), b"second");
        assert!(client.is_connected().await);
        router.abort();
    }

    #[tokio::test]
    async fn an_absent_router_is_unavailable_not_a_panic() {
        let client = MavlinkIngest::with_timeout(
            "/nonexistent/mavlink-ingest.sock",
            Duration::from_millis(50),
        );
        assert!(matches!(
            client.send_frame(b"frame").await,
            Err(IngestError::Unavailable(_))
        ));
        assert!(!client.is_connected().await);
    }

    #[tokio::test]
    async fn an_oversized_frame_is_rejected_before_connecting() {
        // The path does not exist, so any connect attempt would report
        // Unavailable. TooLarge proves the size check ran first.
        let client = MavlinkIngest::with_timeout(
            "/nonexistent/mavlink-ingest.sock",
            Duration::from_millis(50),
        );
        let too_big = vec![0u8; MAVLINK_MAX_FRAME + 1];
        assert_eq!(
            client.send_frame(&too_big).await,
            Err(IngestError::TooLarge(MAVLINK_MAX_FRAME + 1))
        );
    }

    #[tokio::test]
    async fn a_dropped_router_is_reported_and_the_client_reconnects() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(MAVLINK_INGEST_SOCK_NAME);
        let (mut rx, router) = fake_router(path.clone());
        tokio::time::sleep(Duration::from_millis(20)).await;

        let client = MavlinkIngest::with_timeout(&path, Duration::from_millis(200));
        client.send_frame(b"before").await.unwrap();
        assert_eq!(rx.recv().await.unwrap(), b"before");

        // Router goes away, socket file with it.
        router.abort();
        drop(rx);
        tokio::time::sleep(Duration::from_millis(20)).await;
        std::fs::remove_file(&path).ok();

        // Writes into the dead peer eventually fail. Either the write errors
        // straight away or it lands in the socket buffer and the NEXT one
        // fails; both must end with the client having dropped the connection
        // rather than holding a corpse.
        let mut reported = false;
        for _ in 0..8 {
            if client.send_frame(b"after").await.is_err() {
                reported = true;
                break;
            }
        }
        assert!(reported, "a dead peer must surface as an error");
        assert!(
            !client.is_connected().await,
            "the dead connection must be dropped so the next send reconnects"
        );

        // With the router back, the client recovers on its own.
        let (mut rx2, router2) = fake_router(path.clone());
        tokio::time::sleep(Duration::from_millis(20)).await;
        client.send_frame(b"recovered").await.unwrap();
        assert_eq!(rx2.recv().await.unwrap(), b"recovered");
        router2.abort();
    }

    #[tokio::test]
    async fn the_socket_path_follows_the_run_directory_override() {
        // Guards the seam both processes must agree on: the ground data plane
        // and the router derive this path independently.
        assert!(ingest_socket_path().ends_with(MAVLINK_INGEST_SOCK_NAME));
    }
}
