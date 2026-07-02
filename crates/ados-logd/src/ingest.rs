//! The ingest socket: a Unix stream listener that producers connect to and
//! write length-prefixed msgpack frames on. The accept loop hands each decoded
//! frame to the writer over a bounded channel; it never blocks the producer and
//! never lets a slow or dead writer stall the flight stack.
//!
//! Each client is handled by its own task, so a malformed frame or a disconnect
//! on one connection cannot affect another. Framing reuses the shared 4-byte
//! big-endian length prefix; a zero-length frame is rejected and a frame larger
//! than the per-contract cap is rejected before any payload is read.
//!
//! Backpressure policy on a full channel: a high-volume, low-severity frame
//! (a `TRACE`/`DEBUG` log, a telemetry sample) is dropped immediately and a
//! per-class counter is bumped so the drop is visible. A high-severity frame
//! (`WARN`/`ERROR` log, any event) is given a brief bounded wait so it is not
//! silently lost, then dropped only if the writer is still saturated. The
//! producer thread is never made to wait without bound.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::io::AsyncReadExt;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::mpsc;

use ados_protocol::frame::{decode_len, FrameError, HEADER_SIZE};
use ados_protocol::logd::{IngestFrame, Level, LogdError, LOGD_MAX_FRAME};

/// How long a high-severity frame is allowed to wait for channel capacity
/// before it is dropped. Bounded so a producer never blocks without limit even
/// for a `WARN`/`ERROR` record; the writer drains far faster than this in the
/// common case, so the wait is almost never reached.
const HIGH_SEVERITY_SEND_TIMEOUT: Duration = Duration::from_millis(50);

/// The broad class of an ingest frame, used to pick the drop policy and to bin
/// the drop counters reported on the stats surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FrameClass {
    /// A log record.
    Log,
    /// A telemetry sample.
    Telemetry,
    /// A discrete event.
    Event,
    /// A hardware snapshot.
    Hw,
}

impl FrameClass {
    /// The class of a frame, derived from its variant.
    pub fn of(frame: &IngestFrame) -> FrameClass {
        match frame {
            IngestFrame::Log(_) => FrameClass::Log,
            IngestFrame::Telemetry(_) => FrameClass::Telemetry,
            IngestFrame::Event(_) => FrameClass::Event,
            IngestFrame::Hw(_) => FrameClass::Hw,
        }
    }

    /// A stable lowercase label for the stats map.
    pub fn label(self) -> &'static str {
        match self {
            FrameClass::Log => "log",
            FrameClass::Telemetry => "telemetry",
            FrameClass::Event => "event",
            FrameClass::Hw => "hw",
        }
    }
}

/// True when a frame must be preserved through backpressure (an event, or a log
/// at `WARN`/`ERROR`). Low-severity logs and high-rate telemetry are droppable.
fn is_high_severity(frame: &IngestFrame) -> bool {
    match frame {
        IngestFrame::Event(_) => true,
        IngestFrame::Log(l) => l.level.as_u8() >= Level::Warn.as_u8(),
        IngestFrame::Telemetry(_) | IngestFrame::Hw(_) => false,
    }
}

/// Shared counters surfaced by the daemon: frames accepted off the socket and
/// frames dropped, binned by class. Cheap atomics so the accept tasks update
/// them without a lock.
#[derive(Debug, Default)]
pub struct IngestStats {
    accepted: AtomicU64,
    dropped: [AtomicU64; 4],
}

impl IngestStats {
    fn class_idx(class: FrameClass) -> usize {
        match class {
            FrameClass::Log => 0,
            FrameClass::Telemetry => 1,
            FrameClass::Event => 2,
            FrameClass::Hw => 3,
        }
    }

    fn record_accepted(&self) {
        self.accepted.fetch_add(1, Ordering::Relaxed);
    }

    fn record_dropped(&self, class: FrameClass) {
        self.dropped[Self::class_idx(class)].fetch_add(1, Ordering::Relaxed);
    }

    /// Total frames handed to the writer.
    pub fn accepted(&self) -> u64 {
        self.accepted.load(Ordering::Relaxed)
    }

    /// Frames dropped for a given class.
    pub fn dropped(&self, class: FrameClass) -> u64 {
        self.dropped[Self::class_idx(class)].load(Ordering::Relaxed)
    }

    /// A snapshot of the per-class drop counters keyed by the class label.
    pub fn dropped_by_class(&self) -> HashMap<String, u64> {
        let mut m = HashMap::new();
        for class in [
            FrameClass::Log,
            FrameClass::Telemetry,
            FrameClass::Event,
            FrameClass::Hw,
        ] {
            m.insert(class.label().to_string(), self.dropped(class));
        }
        m
    }
}

/// The bound Unix listener plus the path it owns (for cleanup on shutdown).
pub struct IngestSocket {
    listener: UnixListener,
    path: PathBuf,
}

impl IngestSocket {
    /// Bind the ingest socket at `path`. Removes a stale socket from a prior run
    /// (otherwise `bind` fails with `EADDRINUSE`), creates the parent directory
    /// if absent, and tightens the mode to `0o660` on Linux so only the agent
    /// group can write frames.
    pub fn bind(path: impl AsRef<Path>) -> std::io::Result<Self> {
        let path = path.as_ref().to_path_buf();
        // The shared helper owns the create-dir / remove-stale / bind / chmod
        // (0o660) hygiene; group-owning to `ados` afterward keeps the mode's
        // group-rw grant reaching a non-root member (a chown does not clear the
        // rw bits, so the final owner+group+mode state is unchanged).
        let listener = ados_protocol::ipc::bind_command_socket(&path, 0o660)?;
        #[cfg(target_os = "linux")]
        crate::set_ados_group(&path);
        Ok(Self { listener, path })
    }

    /// The socket path this listener owns.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Run the accept loop until `shutdown` resolves. Each accepted connection is
/// served by its own task that reads framed msgpack and forwards decoded frames
/// to `tx`. The loop returns when the shutdown future completes; in-flight
/// client tasks observe the closed channel on the next send and end.
pub async fn run_accept_loop<F>(
    socket: IngestSocket,
    tx: mpsc::Sender<IngestFrame>,
    stats: Arc<IngestStats>,
    shutdown: F,
) where
    F: std::future::Future<Output = ()>,
{
    tracing::info!(path = %socket.path().display(), "ingest socket listening");
    tokio::pin!(shutdown);
    loop {
        tokio::select! {
            _ = &mut shutdown => {
                tracing::info!("ingest accept loop stopping");
                break;
            }
            accepted = socket.listener.accept() => {
                match accepted {
                    Ok((stream, _addr)) => {
                        let tx = tx.clone();
                        let stats = Arc::clone(&stats);
                        tokio::spawn(async move {
                            serve_client(stream, tx, stats).await;
                        });
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "ingest accept failed");
                        // A persistent accept error must not hot-spin the loop.
                        tokio::time::sleep(Duration::from_millis(200)).await;
                    }
                }
            }
        }
    }
}

/// Read framed msgpack from one client until EOF, a protocol error, or the
/// writer channel closing. A decode error on one frame ends only this
/// connection; it never crashes the accept loop or another client.
async fn serve_client(
    mut stream: UnixStream,
    tx: mpsc::Sender<IngestFrame>,
    stats: Arc<IngestStats>,
) {
    loop {
        match read_frame(&mut stream).await {
            Ok(Some(frame)) => {
                if forward(&tx, &stats, frame).await.is_err() {
                    // The writer side is gone; nothing more to do.
                    break;
                }
            }
            Ok(None) => break, // clean EOF
            Err(e) => {
                tracing::debug!(error = %e, "ingest client read error");
                break;
            }
        }
    }
}

/// Read exactly one length-prefixed frame. Returns `Ok(None)` on a clean EOF at
/// a frame boundary (no partial header read).
async fn read_frame(stream: &mut UnixStream) -> Result<Option<IngestFrame>, ReadError> {
    let mut header = [0u8; HEADER_SIZE];
    match stream.read_exact(&mut header).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(ReadError::Io(e)),
    }
    // Reject zero-length and oversized frames before allocating the payload.
    let len = decode_len(header, LOGD_MAX_FRAME, true)?;
    let mut body = vec![0u8; len];
    stream.read_exact(&mut body).await.map_err(ReadError::Io)?;
    let frame = IngestFrame::decode(&body)?;
    Ok(Some(frame))
}

/// Forward one decoded frame to the writer, applying the per-class drop policy
/// on a full channel. Returns `Err(())` only when the channel is closed (the
/// writer has gone away), which ends the client loop.
async fn forward(
    tx: &mpsc::Sender<IngestFrame>,
    stats: &IngestStats,
    frame: IngestFrame,
) -> Result<(), ()> {
    let class = FrameClass::of(&frame);
    match tx.try_send(frame) {
        Ok(()) => {
            stats.record_accepted();
            Ok(())
        }
        Err(mpsc::error::TrySendError::Full(frame)) => {
            if is_high_severity(&frame) {
                // Give a high-severity record a brief bounded chance rather than
                // dropping it outright; never an unbounded wait on the producer.
                match tokio::time::timeout(HIGH_SEVERITY_SEND_TIMEOUT, tx.send(frame)).await {
                    Ok(Ok(())) => {
                        stats.record_accepted();
                        Ok(())
                    }
                    Ok(Err(_)) => Err(()), // channel closed mid-wait
                    Err(_) => {
                        // Still saturated after the bounded wait: drop, count it.
                        stats.record_dropped(class);
                        Ok(())
                    }
                }
            } else {
                // Droppable class: shed it immediately to protect the card and
                // keep the producer wait-free.
                stats.record_dropped(class);
                Ok(())
            }
        }
        Err(mpsc::error::TrySendError::Closed(_)) => Err(()),
    }
}

/// Errors reading one frame off a client connection.
#[derive(Debug, thiserror::Error)]
enum ReadError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("framing error: {0}")]
    Frame(#[from] FrameError),
    #[error("frame decode error: {0}")]
    Decode(#[from] LogdError),
}

#[cfg(test)]
mod tests {
    use super::*;
    use ados_protocol::logd::{EventFrame, LogFrame, TelemetryFrame};

    #[test]
    fn frame_class_maps_each_variant() {
        let log = IngestFrame::Log(LogFrame::new(1, "s", Level::Info, "m"));
        let tele = IngestFrame::Telemetry(TelemetryFrame::new(1, "m", 1.0));
        let evt = IngestFrame::Event(EventFrame::new(1, "k", "s", Level::Info));
        let hw = IngestFrame::Hw(ados_protocol::logd::HwSnapshot::new(1));
        assert_eq!(FrameClass::of(&log), FrameClass::Log);
        assert_eq!(FrameClass::of(&tele), FrameClass::Telemetry);
        assert_eq!(FrameClass::of(&evt), FrameClass::Event);
        assert_eq!(FrameClass::of(&hw), FrameClass::Hw);
    }

    #[test]
    fn high_severity_keeps_events_and_warn_plus_logs() {
        // Events and WARN/ERROR logs are preserved through backpressure.
        assert!(is_high_severity(&IngestFrame::Event(EventFrame::new(
            1,
            "k",
            "s",
            Level::Trace
        ))));
        assert!(is_high_severity(&IngestFrame::Log(LogFrame::new(
            1,
            "s",
            Level::Warn,
            "m"
        ))));
        assert!(is_high_severity(&IngestFrame::Log(LogFrame::new(
            1,
            "s",
            Level::Error,
            "m"
        ))));
        // INFO/DEBUG/TRACE logs and all telemetry/hw are droppable.
        assert!(!is_high_severity(&IngestFrame::Log(LogFrame::new(
            1,
            "s",
            Level::Info,
            "m"
        ))));
        assert!(!is_high_severity(&IngestFrame::Log(LogFrame::new(
            1,
            "s",
            Level::Debug,
            "m"
        ))));
        assert!(!is_high_severity(&IngestFrame::Telemetry(
            TelemetryFrame::new(1, "m", 1.0)
        )));
        assert!(!is_high_severity(&IngestFrame::Hw(
            ados_protocol::logd::HwSnapshot::new(1)
        )));
    }

    #[tokio::test]
    async fn forward_drops_low_severity_when_channel_is_full() {
        // A one-slot channel: fill it, then a droppable frame is shed and the
        // drop counter for its class advances; the producer never blocks.
        let (tx, _rx) = mpsc::channel::<IngestFrame>(1);
        let stats = IngestStats::default();

        // First send fills the only slot.
        forward(
            &tx,
            &stats,
            IngestFrame::Telemetry(TelemetryFrame::new(1, "cpu.load", 1.0)),
        )
        .await
        .unwrap();
        assert_eq!(stats.accepted(), 1);

        // Second send finds the channel full: a telemetry frame is dropped.
        forward(
            &tx,
            &stats,
            IngestFrame::Telemetry(TelemetryFrame::new(2, "cpu.load", 2.0)),
        )
        .await
        .unwrap();
        assert_eq!(stats.accepted(), 1);
        assert_eq!(stats.dropped(FrameClass::Telemetry), 1);
    }

    #[tokio::test]
    async fn forward_drops_high_severity_only_after_the_bounded_wait() {
        // With no reader draining, a full one-slot channel forces even a
        // high-severity frame to drop, but only after the bounded wait; it is
        // never lost silently (the drop counter records it).
        let (tx, _rx) = mpsc::channel::<IngestFrame>(1);
        let stats = IngestStats::default();
        forward(
            &tx,
            &stats,
            IngestFrame::Event(EventFrame::new(1, "k", "s", Level::Error)),
        )
        .await
        .unwrap();
        assert_eq!(stats.accepted(), 1);

        forward(
            &tx,
            &stats,
            IngestFrame::Event(EventFrame::new(2, "k", "s", Level::Error)),
        )
        .await
        .unwrap();
        assert_eq!(stats.accepted(), 1);
        assert_eq!(stats.dropped(FrameClass::Event), 1);
    }

    #[tokio::test]
    async fn forward_signals_closed_channel() {
        let (tx, rx) = mpsc::channel::<IngestFrame>(1);
        drop(rx);
        let r = forward(
            &tx,
            &IngestStats::default(),
            IngestFrame::Log(LogFrame::new(1, "s", Level::Info, "m")),
        )
        .await;
        assert!(r.is_err());
    }

    #[test]
    fn dropped_by_class_snapshot_has_all_classes() {
        let stats = IngestStats::default();
        stats.record_dropped(FrameClass::Log);
        let m = stats.dropped_by_class();
        assert_eq!(m.get("log"), Some(&1));
        assert_eq!(m.get("telemetry"), Some(&0));
        assert_eq!(m.get("event"), Some(&0));
        assert_eq!(m.get("hw"), Some(&0));
    }
}
