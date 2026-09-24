//! The ingest socket: a Unix stream listener that producers connect to and
//! write length-prefixed msgpack frames on. The accept loop hands each decoded
//! frame to the writer over a bounded channel; it never blocks the producer and
//! never lets a slow or dead writer stall the flight stack.
//!
//! Each client is handled by its own task, so a malformed frame or a disconnect
//! on one connection cannot affect another. Framing reuses the shared 4-byte
//! big-endian length prefix; a zero-length frame is rejected and a frame larger
//! than the per-contract cap is rejected before any payload is read. A frame
//! whose body is fully read but does not decode (an unknown wire version, a bad
//! field) is counted and skipped: the stream is still aligned, and the frames a
//! producer batched behind it are real.
//!
//! The socket is reachable from the plugin plane (plugins ship their logs
//! here), so a frame's claimed source is not taken on trust. A peer outside the
//! operator plane has its records namespaced as a plugin's (`plugin:` sources,
//! `plugin.` metric and signal keys), so nothing a plugin writes can be stored
//! under a core service's name.
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
    undecodable: AtomicU64,
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

    /// Frames that arrived whole but did not decode, and were skipped.
    pub fn undecodable(&self) -> u64 {
        self.undecodable.load(Ordering::Relaxed)
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
    /// if absent, and sets the mode to `0o660` on the plugin-reachable plane:
    /// the plugin runtime ships its logs here, so the plugin user must be able
    /// to connect. The socket only accepts log frames; the query socket, which
    /// reads the store back, stays on the operator plane.
    pub fn bind(path: impl AsRef<Path>) -> std::io::Result<Self> {
        let path = path.as_ref().to_path_buf();
        let listener = ados_protocol::ipc::bind_plugin_socket(&path, 0o660)?;
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
                        let operator = ados_protocol::ipc::is_operator_peer(&stream);
                        tokio::spawn(async move {
                            serve_client(stream, tx, stats, operator).await;
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

/// Read framed msgpack from one client until EOF, a framing or I/O error, or
/// the writer channel closing. An undecodable frame is skipped; it never ends
/// the connection, crashes the accept loop or touches another client.
/// `operator` is whether the peer is on the operator plane; a peer that is not
/// has every record namespaced as a plugin's before it is stored.
async fn serve_client(
    mut stream: UnixStream,
    tx: mpsc::Sender<IngestFrame>,
    stats: Arc<IngestStats>,
    operator: bool,
) {
    loop {
        match read_frame(&mut stream).await {
            Ok(Read::Frame(mut frame)) => {
                if !operator {
                    mark_plugin_origin(&mut frame);
                }
                if forward(&tx, &stats, frame).await.is_err() {
                    // The writer side is gone; nothing more to do.
                    break;
                }
            }
            Ok(Read::Undecodable(e)) => {
                stats.undecodable.fetch_add(1, Ordering::Relaxed);
                tracing::debug!(error = %e, "ingest frame skipped: body did not decode");
            }
            Ok(Read::Eof) => break,
            Err(e) => {
                tracing::debug!(error = %e, "ingest client read error");
                break;
            }
        }
    }
}

/// The source prefix a plugin-plane record is stored under.
const PLUGIN_SOURCE_PREFIX: &str = "plugin:";
/// The key prefix a plugin-plane metric or hardware signal is stored under.
const PLUGIN_KEY_PREFIX: &str = "plugin.";

/// Namespace a record from a peer outside the operator plane as a plugin's:
/// a log or event source gains `plugin:`, a metric key and every hardware
/// signal key gain `plugin.`. Idempotent, so a record that already carries the
/// prefix is not double-marked.
fn mark_plugin_origin(frame: &mut IngestFrame) {
    fn prefixed(s: &mut String, prefix: &str) {
        if !s.starts_with(prefix) {
            s.insert_str(0, prefix);
        }
    }
    match frame {
        IngestFrame::Log(f) => prefixed(&mut f.source, PLUGIN_SOURCE_PREFIX),
        IngestFrame::Event(f) => prefixed(&mut f.source, PLUGIN_SOURCE_PREFIX),
        IngestFrame::Telemetry(f) => prefixed(&mut f.metric, PLUGIN_KEY_PREFIX),
        IngestFrame::Hw(f) => {
            f.signals = std::mem::take(&mut f.signals)
                .into_iter()
                .map(|(mut k, v)| {
                    prefixed(&mut k, PLUGIN_KEY_PREFIX);
                    (k, v)
                })
                .collect();
        }
    }
}

/// One read off a client connection.
enum Read {
    /// A decoded frame.
    Frame(IngestFrame),
    /// A complete, frame-aligned body that did not decode.
    Undecodable(LogdError),
    /// A clean EOF at a frame boundary.
    Eof,
}

/// Read exactly one length-prefixed frame. A clean EOF at a frame boundary (no
/// partial header read) is [`Read::Eof`]; a body that was read in full but did
/// not decode is [`Read::Undecodable`], so the caller can skip it and keep the
/// stream.
async fn read_frame(stream: &mut UnixStream) -> Result<Read, ReadError> {
    let mut header = [0u8; HEADER_SIZE];
    match stream.read_exact(&mut header).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(Read::Eof),
        Err(e) => return Err(ReadError::Io(e)),
    }
    // Reject zero-length and oversized frames before allocating the payload.
    let len = decode_len(header, LOGD_MAX_FRAME, true)?;
    let mut body = vec![0u8; len];
    stream.read_exact(&mut body).await.map_err(ReadError::Io)?;
    Ok(match IngestFrame::decode(&body) {
        Ok(frame) => Read::Frame(frame),
        Err(e) => Read::Undecodable(e),
    })
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

/// Errors that end a client connection: the stream can no longer be framed.
#[derive(Debug, thiserror::Error)]
enum ReadError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("framing error: {0}")]
    Frame(#[from] FrameError),
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

    /// Drive `serve_client` over a socket pair with the given frames on the
    /// wire, returning what reached the writer channel and the stats.
    async fn serve_wire(wire: Vec<u8>, operator: bool) -> (Vec<IngestFrame>, Arc<IngestStats>) {
        use tokio::io::AsyncWriteExt;
        let (mut producer, server) = UnixStream::pair().unwrap();
        let (tx, mut rx) = mpsc::channel::<IngestFrame>(16);
        let stats = Arc::new(IngestStats::default());
        let task = tokio::spawn(serve_client(server, tx, Arc::clone(&stats), operator));
        producer.write_all(&wire).await.unwrap();
        drop(producer);
        task.await.unwrap();
        let mut got = Vec::new();
        while let Ok(f) = rx.try_recv() {
            got.push(f);
        }
        (got, stats)
    }

    /// A plugin-plane peer cannot write under a core service's name: its log
    /// and event sources are namespaced, as are its metric and signal keys.
    #[tokio::test]
    async fn a_plugin_peer_cannot_write_as_a_core_service() {
        let mut hw = ados_protocol::logd::HwSnapshot::new(4);
        hw.signals.insert(
            "thermal.soc_c".into(),
            ados_protocol::logd::Value::from(99.0),
        );
        let frames = [
            IngestFrame::Log(LogFrame::new(1, "ados-mavlink-router", Level::Error, "m")),
            IngestFrame::Event(EventFrame::new(
                2,
                "fc.disarm",
                "ados-mavlink-router",
                Level::Warn,
            )),
            IngestFrame::Telemetry(TelemetryFrame::new(3, "cpu.load", 1.0)),
            IngestFrame::Hw(hw),
        ];
        let wire: Vec<u8> = frames.iter().flat_map(|f| f.encode().unwrap()).collect();

        let (got, _) = serve_wire(wire.clone(), false).await;
        assert_eq!(got.len(), 4);
        match (&got[0], &got[1], &got[2], &got[3]) {
            (
                IngestFrame::Log(l),
                IngestFrame::Event(e),
                IngestFrame::Telemetry(t),
                IngestFrame::Hw(h),
            ) => {
                assert_eq!(l.source, "plugin:ados-mavlink-router");
                assert_eq!(e.source, "plugin:ados-mavlink-router");
                assert_eq!(t.metric, "plugin.cpu.load");
                assert!(h.signals.contains_key("plugin.thermal.soc_c"));
                assert!(!h.signals.contains_key("thermal.soc_c"));
            }
            other => panic!("unexpected frames {other:?}"),
        }

        // An operator-plane peer's records are stored as written.
        let (got, _) = serve_wire(wire, true).await;
        match &got[0] {
            IngestFrame::Log(l) => assert_eq!(l.source, "ados-mavlink-router"),
            other => panic!("unexpected frame {other:?}"),
        }
    }

    /// A frame that arrives whole but does not decode is skipped; the frames
    /// the producer batched behind it still reach the writer.
    #[tokio::test]
    async fn an_undecodable_frame_is_skipped_not_fatal() {
        let mut future = LogFrame::new(1, "s", Level::Info, "from a newer producer");
        future.v = 99;
        let mut wire = IngestFrame::Log(future).encode().unwrap();
        wire.extend(
            ados_protocol::frame::encode_frame(b"\xc1not msgpack", LOGD_MAX_FRAME).unwrap(),
        );
        wire.extend(
            IngestFrame::Log(LogFrame::new(2, "s", Level::Info, "after"))
                .encode()
                .unwrap(),
        );

        let (got, stats) = serve_wire(wire, true).await;
        assert_eq!(stats.undecodable(), 2);
        assert_eq!(got.len(), 1);
        match &got[0] {
            IngestFrame::Log(l) => assert_eq!(l.msg, "after"),
            other => panic!("unexpected frame {other:?}"),
        }
    }
}
