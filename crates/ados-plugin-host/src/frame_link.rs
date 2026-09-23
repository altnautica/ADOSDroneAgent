//! One reconnecting client for the router's length-prefixed byte sockets
//! (`/run/ados/mavlink.sock` and `/run/ados/msp.sock`).
//!
//! Both sockets speak the same wire: a 4-byte big-endian length, then the raw
//! bytes, in both directions. Bytes written toward the socket are commands
//! toward the flight controller; FC bytes fan out on the same connection. The
//! router never parses the MSP stream and forwards MAVLink batches verbatim, so
//! this link carries opaque byte chunks and knows nothing about either protocol.
//!
//! The link owns the socket in one task that reconnects forever at a fixed
//! interval, so a router restart heals without the host noticing. On every new
//! connection the declarations (the off-box or injector claims that decide how
//! the router gates this writer) are written BEFORE any queued command, so
//! injector traffic can never pass as operator traffic after a reconnect.
//!
//! [`FrameLink::send`] reports what happened instead of swallowing it: a frame
//! over the wire cap, a full queue, a missing connection and, on a link that
//! declared itself an injector, the router's PIC gate refusing it are each an
//! error, so a caller never tells a plugin a command was sent when it was not.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use ados_protocol::frame::{encode_frame, MAVLINK_MAX_FRAME};
use ados_protocol::ipc::read_length_prefixed;
use tokio::io::AsyncWriteExt;
use tokio::net::UnixStream;
use tokio::sync::{broadcast, mpsc};
use tokio::task::JoinHandle;

use crate::pic_gate::PicGate;

/// Depth of the inbound fanout and the outbound command queue. Matches the
/// router's queue depth so a subscriber that briefly stalls lags rather than
/// wedging the reader.
pub const LINK_DEPTH: usize = 256;

/// Fixed wait between connection attempts. There is no cap: the router may be
/// down for a reconfigure or not installed yet, and the link must come back by
/// itself whenever it returns.
pub const RECONNECT_INTERVAL: Duration = Duration::from_secs(3);

/// Produces the declaration payloads written at the start of every connection,
/// in order. Called per connection so a time-limited attestation is minted
/// fresh each time rather than replayed stale.
pub type Declarations = Box<dyn Fn() -> Vec<Vec<u8>> + Send + Sync>;

/// Why a command did not reach the router socket.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendError {
    /// Larger than the socket's frame cap; the router would refuse it.
    TooLarge,
    /// The outbound queue is full; the writer is behind.
    QueueFull,
    /// No live connection to the router.
    Disconnected,
    /// The link declared itself an autonomous injector and the router's PIC
    /// gate refuses injector traffic right now: an operator holds manual
    /// control, or the PIC arbiter is not reporting.
    PicRefused,
}

impl SendError {
    /// The stable reason string a plugin sees in a `sent: false` response.
    pub fn reason(self) -> &'static str {
        match self {
            Self::TooLarge => "too_large",
            Self::QueueFull => "queue_full",
            Self::Disconnected => "disconnected",
            Self::PicRefused => "pic_refused",
        }
    }
}

impl std::fmt::Display for SendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.reason())
    }
}

/// A self-healing link to one router byte socket.
pub struct FrameLink {
    outbound: mpsc::Sender<Vec<u8>>,
    inbound: broadcast::Sender<Vec<u8>>,
    connected: Arc<AtomicBool>,
    task: JoinHandle<()>,
    /// Set on a link that declares an injector: the router's PIC gate, asked
    /// before a command is queued.
    pic_gate: Option<PicGate>,
}

impl FrameLink {
    /// Start the link task for `path`. Never fails: an absent socket is retried
    /// every [`RECONNECT_INTERVAL`], and sends report [`SendError::Disconnected`]
    /// until a connection is up.
    pub fn spawn(path: impl AsRef<Path>, declarations: Declarations) -> Self {
        Self::spawn_with_interval(
            path.as_ref().to_path_buf(),
            declarations,
            RECONNECT_INTERVAL,
        )
    }

    fn spawn_with_interval(path: PathBuf, declarations: Declarations, interval: Duration) -> Self {
        let (outbound, out_rx) = mpsc::channel::<Vec<u8>>(LINK_DEPTH);
        let (inbound, _rx) = broadcast::channel(LINK_DEPTH);
        let connected = Arc::new(AtomicBool::new(false));
        let task = tokio::spawn(run(
            path,
            declarations,
            interval,
            out_rx,
            inbound.clone(),
            connected.clone(),
        ));
        Self {
            outbound,
            inbound,
            connected,
            task,
            pic_gate: None,
        }
    }

    /// Check every command against the router's PIC gate before queueing it.
    /// For a link whose declarations subject it to that gate.
    pub fn with_pic_gate(mut self, gate: PicGate) -> Self {
        self.pic_gate = Some(gate);
        self
    }

    /// Frame `data` and queue it toward the flight controller. `Ok` means the
    /// frame is queued on a live connection, behind the declarations, and the
    /// router's PIC gate (when this link is subject to it) lets it through.
    pub fn send(&self, data: &[u8]) -> Result<(), SendError> {
        let frame = encode_frame(data, MAVLINK_MAX_FRAME).map_err(|_| SendError::TooLarge)?;
        if !self.connected.load(Ordering::Acquire) {
            return Err(SendError::Disconnected);
        }
        if self.pic_gate.as_ref().is_some_and(PicGate::refuses) {
            return Err(SendError::PicRefused);
        }
        self.outbound.try_send(frame).map_err(|e| match e {
            mpsc::error::TrySendError::Full(_) => SendError::QueueFull,
            mpsc::error::TrySendError::Closed(_) => SendError::Disconnected,
        })
    }

    /// A fresh receiver for the FC-side fanout. It survives reconnects: frames
    /// resume on the same receiver once the router is back.
    pub fn subscribe(&self) -> broadcast::Receiver<Vec<u8>> {
        self.inbound.subscribe()
    }

    /// Whether a connection to the router is live right now.
    pub fn is_connected(&self) -> bool {
        self.connected.load(Ordering::Acquire)
    }

    /// Wait up to `timeout` for a live connection. Returns whether one is up.
    pub async fn connected_within(&self, timeout: Duration) -> bool {
        let deadline = tokio::time::Instant::now() + timeout;
        while !self.is_connected() {
            if tokio::time::Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        true
    }
}

impl Drop for FrameLink {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn run(
    path: PathBuf,
    declarations: Declarations,
    interval: Duration,
    mut out_rx: mpsc::Receiver<Vec<u8>>,
    inbound: broadcast::Sender<Vec<u8>>,
    connected: Arc<AtomicBool>,
) {
    loop {
        match UnixStream::connect(&path).await {
            Ok(stream) => {
                tracing::info!(path = %path.display(), "router socket connected");
                serve(stream, &declarations, &mut out_rx, &inbound, &connected).await;
                connected.store(false, Ordering::Release);
                // Commands queued for the lost connection were never delivered.
                // Drop them rather than replay stale commands onto the next one.
                while out_rx.try_recv().is_ok() {}
                tracing::warn!(path = %path.display(), "router socket connection lost; reconnecting");
            }
            Err(e) => {
                tracing::debug!(path = %path.display(), error = %e, "router socket unavailable");
            }
        }
        tokio::time::sleep(interval).await;
    }
}

/// One connection's lifetime: declarations first, then the reader and the
/// command writer until either side fails.
async fn serve(
    stream: UnixStream,
    declarations: &Declarations,
    out_rx: &mut mpsc::Receiver<Vec<u8>>,
    inbound: &broadcast::Sender<Vec<u8>>,
    connected: &AtomicBool,
) {
    let (mut read_half, mut write_half) = stream.into_split();
    for declaration in declarations() {
        // A declaration that cannot be written means this connection would carry
        // commands without the claim that gates them, so it is not used at all.
        let Ok(frame) = encode_frame(&declaration, MAVLINK_MAX_FRAME) else {
            tracing::error!("router socket declaration exceeds the frame cap");
            return;
        };
        if write_half.write_all(&frame).await.is_err() || write_half.flush().await.is_err() {
            return;
        }
    }

    let tx = inbound.clone();
    let mut reader = tokio::spawn(async move {
        // Zero-length chunks are legal on these sockets (reject_zero = false).
        while let Ok(Some(frame)) =
            read_length_prefixed(&mut read_half, MAVLINK_MAX_FRAME, false).await
        {
            // No receivers is fine; the next subscriber resumes at the tail.
            let _ = tx.send(frame);
        }
    });
    connected.store(true, Ordering::Release);

    loop {
        tokio::select! {
            _ = &mut reader => return,
            frame = out_rx.recv() => {
                let Some(frame) = frame else {
                    reader.abort();
                    return;
                };
                if write_half.write_all(&frame).await.is_err()
                    || write_half.flush().await.is_err()
                {
                    reader.abort();
                    return;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ados_protocol::ipc::{IpcBroadcast, IPC_DECLARE_OFF_BOX_SOURCE};

    fn temp_sock(name: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "ados-framelink-{}-{}.sock",
            std::process::id(),
            name
        ));
        let _ = std::fs::remove_file(&p);
        p
    }

    fn no_declarations() -> Declarations {
        Box::new(Vec::new)
    }

    #[tokio::test]
    async fn send_reports_a_missing_router_and_an_oversized_frame() {
        let link = FrameLink::spawn(temp_sock("absent"), no_declarations());
        assert_eq!(link.send(b"cmd"), Err(SendError::Disconnected));
        assert_eq!(
            link.send(&vec![0u8; MAVLINK_MAX_FRAME + 1]),
            Err(SendError::TooLarge)
        );
    }

    #[tokio::test]
    async fn commands_reach_the_router_and_fc_frames_fan_out() {
        let path = temp_sock("roundtrip");
        let (server, inbound) = IpcBroadcast::bind(&path, LINK_DEPTH, false, Some(16))
            .await
            .unwrap();
        let mut inbound = inbound.unwrap();
        let link = FrameLink::spawn(&path, no_declarations());
        assert!(link.connected_within(Duration::from_secs(2)).await);
        let mut rx = link.subscribe();
        tokio::time::sleep(Duration::from_millis(50)).await;

        link.send(b"command-toward-fc").unwrap();
        let got = tokio::time::timeout(Duration::from_secs(1), inbound.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(got.payload, b"command-toward-fc");

        server
            .broadcast(encode_frame(b"fc-frame", MAVLINK_MAX_FRAME).unwrap().into())
            .await;
        let frame = tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(frame, b"fc-frame");
    }

    /// A router restart must heal by itself: the link reconnects, replays its
    /// declaration before any command, and the same subscriber keeps receiving.
    #[tokio::test]
    async fn a_router_restart_reconnects_and_replays_the_declaration() {
        let path = temp_sock("restart");
        let (first, _first_inbound) = IpcBroadcast::bind(&path, LINK_DEPTH, false, Some(16))
            .await
            .unwrap();
        let link = FrameLink::spawn_with_interval(
            path.clone(),
            Box::new(|| vec![IPC_DECLARE_OFF_BOX_SOURCE.to_vec()]),
            Duration::from_millis(50),
        );
        assert!(link.connected_within(Duration::from_secs(2)).await);
        let mut rx = link.subscribe();

        drop(first);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while link.is_connected() && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(link.send(b"while-down"), Err(SendError::Disconnected));

        let (second, inbound) = IpcBroadcast::bind(&path, LINK_DEPTH, false, Some(16))
            .await
            .unwrap();
        let mut inbound = inbound.unwrap();
        assert!(
            link.connected_within(Duration::from_secs(2)).await,
            "the link must reconnect to a restarted router"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;

        link.send(b"after-restart").unwrap();
        let got = tokio::time::timeout(Duration::from_secs(1), inbound.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(got.payload, b"after-restart");
        assert!(
            got.peer.off_box_source,
            "the declaration must be replayed on the new connection"
        );

        second
            .broadcast(encode_frame(b"fc-again", MAVLINK_MAX_FRAME).unwrap().into())
            .await;
        let frame = tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(frame, b"fc-again");
    }
}
