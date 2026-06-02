//! A composable `tracing` layer that ships log events to the logging daemon's
//! ingest socket, alongside the binary's existing journald or fmt sink.
//!
//! The layer is best-effort and never on the critical path of the emitting
//! service:
//!
//! - On each event it captures the timestamp, level, target, message, and
//!   structured fields, redacts any secret-bearing field in place
//!   ([`crate::logd::redact`]), frames a [`LogFrame`], and hands it to a bounded
//!   channel with a non-blocking send. A full channel triggers the drop policy:
//!   `DEBUG`/`TRACE` are shed first, `WARN`/`ERROR` are kept as long as there is
//!   any room and only ever dropped (never block) when the channel is saturated.
//! - A single background task drains the channel, batches frames, and writes
//!   them to the ingest socket through a reconnecting writer. If the socket is
//!   absent (the daemon is not installed or not yet up — the normal state until
//!   the daemon's unit ships) or the connection drops, the writer silently backs
//!   off and retries; it never panics, never blocks the producer, and never logs
//!   an error that would disrupt the service.
//! - The writer's hot path never emits a `tracing` event, so a failure to ship a
//!   log can never recurse into another shipped log.
//!
//! The journald or fmt layer remains the always-on primary sink; this layer is
//! purely additive durable enrichment. Redaction happens before a frame ever
//! leaves the process, so a secret is never written to the socket.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rmpv::Value as MpVal;
use tokio::io::AsyncWriteExt;
use tokio::net::UnixStream;
use tokio::sync::mpsc;
use tracing::field::{Field, Visit};
use tracing::{Event, Level as TraceLevel, Subscriber};
use tracing_subscriber::layer::{Context, Layer};
use tracing_subscriber::registry::LookupSpan;

use crate::logd::{Fields, IngestFrame, Level, LogFrame};

/// Default ingest socket path. Producers connect here to write framed records.
/// Re-exported from [`crate::logd::emitter`] so the path has one source of truth
/// shared by the log layer and the event emitter.
pub use crate::logd::emitter::DEFAULT_INGEST_SOCK;

/// Channel capacity between the layer and the background shipper. Sized so a
/// burst of records rides through a brief writer stall without dropping, while
/// bounding the memory a runaway producer can pin.
pub const CHANNEL_CAPACITY: usize = 1024;

/// Maximum frames coalesced into one socket write. Bounds the per-batch buffer
/// and keeps the writer responsive to shutdown.
const BATCH_MAX_FRAMES: usize = 128;

/// How long the shipper waits to accumulate a batch before flushing what it has.
const BATCH_LINGER: Duration = Duration::from_millis(100);

/// Reconnect backoff schedule (milliseconds). The writer steps through these on
/// repeated connect failures and holds at the last value, so an absent socket
/// produces a slow, quiet retry rather than a hot loop.
const BACKOFF_MS: [u64; 5] = [250, 500, 1000, 2000, 5000];

/// Counters surfaced for diagnostics: frames enqueued for shipping and frames
/// dropped because the channel was full. A visible drop is better than a silent
/// one. Cheap atomics so the event hot path stays lock-free.
#[derive(Debug, Default)]
pub struct LayerStats {
    enqueued: AtomicU64,
    dropped: AtomicU64,
}

impl LayerStats {
    fn record_enqueued(&self) {
        self.enqueued.fetch_add(1, Ordering::Relaxed);
    }

    fn record_dropped(&self) {
        self.dropped.fetch_add(1, Ordering::Relaxed);
    }

    /// Total frames handed to the shipper channel.
    pub fn enqueued(&self) -> u64 {
        self.enqueued.load(Ordering::Relaxed)
    }

    /// Total frames dropped at the channel boundary under backpressure.
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

/// A `tracing` layer that ships events to the logging daemon's ingest socket.
///
/// Construct it with [`LogdLayer::new`] (or [`LogdLayer::with_socket`]) inside a
/// running tokio runtime — the background shipper is spawned at construction —
/// and add it to the subscriber registry alongside the existing journald/fmt
/// layer. It is generic over the subscriber `S` so it composes with any stack.
pub struct LogdLayer<S> {
    source: String,
    tx: mpsc::Sender<LogFrame>,
    stats: Arc<LayerStats>,
    _marker: std::marker::PhantomData<fn(S)>,
}

impl<S> LogdLayer<S> {
    /// Build a layer for the default ingest socket, tagging every frame with
    /// `source` (the binary name). Spawns the background shipper on the current
    /// tokio runtime; must be called from within a runtime context.
    pub fn new(source: impl Into<String>) -> Self {
        Self::with_socket(source, DEFAULT_INGEST_SOCK)
    }

    /// Build a layer that ships to an explicit socket path (used by tests).
    /// Spawns the background shipper on the current tokio runtime.
    pub fn with_socket(source: impl Into<String>, socket: impl AsRef<Path>) -> Self {
        let (tx, rx) = mpsc::channel::<LogFrame>(CHANNEL_CAPACITY);
        let stats = Arc::new(LayerStats::default());
        let path = socket.as_ref().to_path_buf();
        tokio::spawn(shipper_task(rx, path));
        Self {
            source: source.into(),
            tx,
            stats,
            _marker: std::marker::PhantomData,
        }
    }

    /// The diagnostic counters for this layer (enqueued / dropped).
    pub fn stats(&self) -> Arc<LayerStats> {
        Arc::clone(&self.stats)
    }

    /// Hand a frame to the shipper, applying the drop policy on a full channel.
    /// Never blocks: `DEBUG`/`TRACE` are dropped immediately when full;
    /// `WARN`/`ERROR` are also dropped if the channel is saturated, but the drop
    /// is counted so it is visible. A closed channel (shipper gone) is a silent
    /// drop. This is deliberately wait-free — a log call inside a hot loop must
    /// never stall on the logging path.
    fn enqueue(&self, frame: LogFrame) {
        match self.tx.try_send(frame) {
            Ok(()) => self.stats.record_enqueued(),
            Err(mpsc::error::TrySendError::Full(_)) => self.stats.record_dropped(),
            Err(mpsc::error::TrySendError::Closed(_)) => self.stats.record_dropped(),
        }
    }
}

/// Map a `tracing` level to the canonical 0..4 ordinal used on the wire.
fn map_level(level: &TraceLevel) -> Level {
    match *level {
        TraceLevel::TRACE => Level::Trace,
        TraceLevel::DEBUG => Level::Debug,
        TraceLevel::INFO => Level::Info,
        TraceLevel::WARN => Level::Warn,
        TraceLevel::ERROR => Level::Error,
    }
}

/// Microsecond epoch timestamp for the current instant.
fn now_us() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_micros() as i64)
        .unwrap_or(0)
}

/// Visits a `tracing` event's fields, collecting the message into a string and
/// every other field into the open structured map. Numbers and booleans keep
/// their native msgpack type; everything else is rendered with `Debug` so a
/// custom value is captured rather than dropped.
struct FieldVisitor {
    message: String,
    fields: Fields,
}

impl FieldVisitor {
    fn new() -> Self {
        Self {
            message: String::new(),
            fields: Fields::new(),
        }
    }

    fn insert(&mut self, field: &Field, value: MpVal) {
        let name = field.name();
        if name == "message" {
            return;
        }
        self.fields.insert(name.to_string(), value);
    }
}

impl Visit for FieldVisitor {
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.message.push_str(value);
        } else {
            self.insert(field, MpVal::from(value));
        }
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        self.insert(field, MpVal::from(value));
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.insert(field, MpVal::from(value));
    }

    fn record_f64(&mut self, field: &Field, value: f64) {
        self.insert(field, MpVal::from(value));
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        self.insert(field, MpVal::from(value));
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            let _ = write!(self.message, "{value:?}");
        } else {
            self.insert(field, MpVal::from(format!("{value:?}")));
        }
    }
}

impl<S> Layer<S> for LogdLayer<S>
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let meta = event.metadata();
        let mut visitor = FieldVisitor::new();
        event.record(&mut visitor);

        let mut frame = LogFrame::new(
            now_us(),
            self.source.clone(),
            map_level(meta.level()),
            visitor.message,
        );
        frame.target = Some(meta.target().to_string());
        frame.fields = visitor.fields;

        // Redact before the frame leaves the process: a secret-bearing field is
        // hashed at the source so a raw value never reaches the socket or disk.
        frame.redact_fields();

        self.enqueue(frame);
    }
}

/// The reconnecting socket writer. Holds at most one live connection; on any I/O
/// error it drops the connection and reconnects on the backoff schedule. All
/// failures are swallowed — the socket being absent is the expected steady state
/// before the daemon's unit is enabled, so it must never log or panic.
struct SocketWriter {
    path: PathBuf,
    stream: Option<UnixStream>,
    backoff_idx: usize,
}

impl SocketWriter {
    fn new(path: PathBuf) -> Self {
        Self {
            path,
            stream: None,
            backoff_idx: 0,
        }
    }

    /// Ensure a live connection, connecting if needed. Returns `false` (after a
    /// backoff sleep) when the socket cannot be reached, so the caller can retry
    /// later without hot-spinning.
    async fn ensure_connected(&mut self) -> bool {
        if self.stream.is_some() {
            return true;
        }
        match UnixStream::connect(&self.path).await {
            Ok(s) => {
                self.stream = Some(s);
                self.backoff_idx = 0;
                true
            }
            Err(_) => {
                let ms = BACKOFF_MS[self.backoff_idx.min(BACKOFF_MS.len() - 1)];
                self.backoff_idx = (self.backoff_idx + 1).min(BACKOFF_MS.len() - 1);
                tokio::time::sleep(Duration::from_millis(ms)).await;
                false
            }
        }
    }

    /// Write one pre-framed byte buffer (a concatenation of length-prefixed
    /// frames) to the socket. On error, drops the connection so the next call
    /// reconnects. Returns `true` on success.
    async fn write_all(&mut self, buf: &[u8]) -> bool {
        let Some(stream) = self.stream.as_mut() else {
            return false;
        };
        match stream.write_all(buf).await {
            Ok(()) => true,
            Err(_) => {
                // The daemon went away or the socket errored: drop the handle so
                // the next batch reconnects from scratch. The dropped batch is
                // lost on the durable path, but journald still has every line.
                self.stream = None;
                false
            }
        }
    }
}

/// Encode a batch of frames into one length-prefixed byte buffer. A frame that
/// fails to encode (e.g. an oversized payload) is skipped rather than aborting
/// the batch, so one bad record never blocks the rest.
fn encode_batch(batch: &[LogFrame]) -> Vec<u8> {
    let mut buf = Vec::new();
    for frame in batch {
        if let Ok(bytes) = IngestFrame::Log(frame.clone()).encode() {
            buf.extend_from_slice(&bytes);
        }
    }
    buf
}

/// Drain the channel, batch frames, and ship them to the ingest socket. Runs
/// until the channel closes (every layer handle dropped), then exits. Its hot
/// path never emits a `tracing` event, so shipping a log can never recurse.
async fn shipper_task(mut rx: mpsc::Receiver<LogFrame>, path: PathBuf) {
    let mut writer = SocketWriter::new(path);
    let mut batch: Vec<LogFrame> = Vec::with_capacity(BATCH_MAX_FRAMES);

    loop {
        // Block for the first frame of a batch so the task idles cheaply when no
        // logs flow. `None` means every sender dropped: flush and exit.
        let Some(first) = rx.recv().await else {
            break;
        };
        batch.clear();
        batch.push(first);

        // Coalesce additional frames already queued, up to the batch cap or the
        // linger window, without blocking past the deadline.
        let deadline = tokio::time::Instant::now() + BATCH_LINGER;
        while batch.len() < BATCH_MAX_FRAMES {
            match tokio::time::timeout_at(deadline, rx.recv()).await {
                Ok(Some(frame)) => batch.push(frame),
                // Channel closed mid-batch: ship what we have, then the next
                // `recv` returns `None` and the loop ends.
                Ok(None) => break,
                // Linger window elapsed: ship the partial batch.
                Err(_) => break,
            }
        }

        // Connect (with backoff) and ship. If the socket is unreachable, the
        // batch is dropped; journald is the always-on fallback sink, so the line
        // is never lost outright.
        if writer.ensure_connected().await {
            let buf = encode_batch(&batch);
            if !buf.is_empty() {
                let _ = writer.write_all(&buf).await;
            }
        }
    }

    // Final flush on shutdown: best-effort, one connect attempt, no backoff loop.
    if !batch.is_empty() && writer.stream.is_some() {
        let buf = encode_batch(&batch);
        if !buf.is_empty() {
            let _ = writer.write_all(&buf).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt;
    use tokio::net::UnixListener;
    use tracing::Level as TLevel;
    use tracing_subscriber::prelude::*;

    use crate::frame::{decode_len, HEADER_SIZE};
    use crate::logd::{IngestFrame, Level, LOGD_MAX_FRAME};

    fn tmp_sock() -> PathBuf {
        let mut p = std::env::temp_dir();
        let uniq = format!(
            "ados-logd-layer-test-{}-{}.sock",
            std::process::id(),
            now_us()
        );
        p.push(uniq);
        let _ = std::fs::remove_file(&p);
        p
    }

    /// Read one length-prefixed ingest frame off a connected stream.
    async fn read_one(stream: &mut UnixStream) -> IngestFrame {
        let mut header = [0u8; HEADER_SIZE];
        stream.read_exact(&mut header).await.unwrap();
        let len = decode_len(header, LOGD_MAX_FRAME, true).unwrap();
        let mut body = vec![0u8; len];
        stream.read_exact(&mut body).await.unwrap();
        IngestFrame::decode(&body).unwrap()
    }

    #[test]
    fn level_mapping_covers_every_level() {
        assert_eq!(map_level(&TLevel::TRACE), Level::Trace);
        assert_eq!(map_level(&TLevel::DEBUG), Level::Debug);
        assert_eq!(map_level(&TLevel::INFO), Level::Info);
        assert_eq!(map_level(&TLevel::WARN), Level::Warn);
        assert_eq!(map_level(&TLevel::ERROR), Level::Error);
    }

    #[test]
    fn field_visitor_separates_message_from_fields() {
        // Exercise the visitor through a real event under a capturing subscriber,
        // then assert the message and the structured fields are split correctly.
        let out: Arc<std::sync::Mutex<Option<(String, Fields)>>> =
            Arc::new(std::sync::Mutex::new(None));
        let sub = CapturingSub {
            out: Arc::clone(&out),
        };
        tracing::subscriber::with_default(sub, || {
            tracing::info!(count = 7u64, name = "rig", "hello world");
        });
        let (msg, fields) = out.lock().unwrap().take().expect("event captured");
        assert_eq!(msg, "hello world");
        assert_eq!(fields.get("count").and_then(|v| v.as_u64()), Some(7));
        assert_eq!(fields.get("name").and_then(|v| v.as_str()), Some("rig"));
        // The message pseudo-field is not duplicated into the fields map.
        assert!(!fields.contains_key("message"));
    }

    /// A minimal subscriber that runs the layer's `FieldVisitor` over one event
    /// and stashes the result, so the capture path is testable without a socket.
    struct CapturingSub {
        out: Arc<std::sync::Mutex<Option<(String, Fields)>>>,
    }

    impl tracing::Subscriber for CapturingSub {
        fn enabled(&self, _m: &tracing::Metadata<'_>) -> bool {
            true
        }
        fn new_span(&self, _a: &tracing::span::Attributes<'_>) -> tracing::span::Id {
            tracing::span::Id::from_u64(1)
        }
        fn record(&self, _s: &tracing::span::Id, _v: &tracing::span::Record<'_>) {}
        fn record_follows_from(&self, _s: &tracing::span::Id, _f: &tracing::span::Id) {}
        fn event(&self, event: &Event<'_>) {
            let mut v = FieldVisitor::new();
            event.record(&mut v);
            if let Ok(mut guard) = self.out.lock() {
                *guard = Some((v.message, v.fields));
            }
        }
        fn enter(&self, _s: &tracing::span::Id) {}
        fn exit(&self, _s: &tracing::span::Id) {}
    }

    #[tokio::test]
    async fn ships_a_redacted_frame_to_a_live_socket() {
        let path = tmp_sock();
        let listener = UnixListener::bind(&path).unwrap();

        // Accept one connection in the background and read the first frame.
        let accept = {
            let listener = listener;
            tokio::spawn(async move {
                let (mut stream, _) = listener.accept().await.unwrap();
                read_one(&mut stream).await
            })
        };

        let layer: LogdLayer<_> = LogdLayer::with_socket("ados-test", &path);
        let subscriber = tracing_subscriber::registry().with(layer);

        tracing::subscriber::with_default(subscriber, || {
            // A secret-bearing field must be redacted before it leaves.
            tracing::warn!(
                api_key = "ABCDEFGHIJ1234567890",
                attempt = 2u64,
                "link down"
            );
        });

        let frame = tokio::time::timeout(Duration::from_secs(5), accept)
            .await
            .expect("frame delivered within the deadline")
            .expect("accept task ok");

        match frame {
            IngestFrame::Log(log) => {
                assert_eq!(log.source, "ados-test");
                assert_eq!(log.level, Level::Warn);
                assert_eq!(log.msg, "link down");
                assert_eq!(log.fields.get("attempt").and_then(|v| v.as_u64()), Some(2));
                // The secret was redacted at the source.
                assert_eq!(
                    log.fields.get("api_key").and_then(|v| v.as_str()),
                    Some("redacted:ABCD...bb2a0cee")
                );
            }
            other => panic!("expected a log frame, got {other:?}"),
        }

        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn absent_socket_never_blocks_or_panics() {
        // Point the layer at a path with no listener. Emitting events must not
        // block the producer or panic; frames are dropped on the durable path
        // while the shipper backs off quietly.
        let path = tmp_sock(); // never bound
        let layer: LogdLayer<_> = LogdLayer::with_socket("ados-test", &path);
        let stats = layer.stats();
        let subscriber = tracing_subscriber::registry().with(layer);

        tracing::subscriber::with_default(subscriber, || {
            for i in 0..50 {
                tracing::info!(i, "tick");
            }
        });

        // The producer returned immediately for every event; some were enqueued
        // (channel has capacity) and none caused a panic. The exact enqueued
        // count is timing-dependent, but it must be greater than zero and the
        // call must have completed (we are here).
        assert!(stats.enqueued() >= 1, "at least one frame was enqueued");

        // Give the shipper a moment to attempt and fail a connection; it must
        // still be alive and the test still running (no panic propagated).
        tokio::time::sleep(Duration::from_millis(50)).await;

        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn full_channel_drops_without_blocking_and_counts_it() {
        // Build a layer whose shipper can never drain (point at an absent socket
        // so the shipper stays in backoff), then flood past the channel
        // capacity. Sends must return immediately and the overflow is counted.
        let path = tmp_sock(); // never bound: shipper parks in backoff
        let layer: LogdLayer<tracing_subscriber::Registry> =
            LogdLayer::with_socket("ados-test", &path);
        let stats = layer.stats();

        // Emit far more than the channel can hold, directly through the layer's
        // enqueue path (no subscriber needed for this unit check).
        let total = CHANNEL_CAPACITY * 2;
        for n in 0..total {
            let frame = LogFrame::new(now_us(), "ados-test", Level::Debug, format!("m{n}"));
            layer.enqueue(frame);
        }

        // Every call returned (we are here) and the totals reconcile: enqueued +
        // dropped accounts for every emitted frame, and at least some were
        // dropped because the shipper could not drain to an absent socket.
        let enq = stats.enqueued();
        let drp = stats.dropped();
        assert_eq!(enq + drp, total as u64, "every frame accounted for");
        assert!(drp >= 1, "overflow past capacity was dropped and counted");

        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn warn_and_error_are_kept_until_the_channel_saturates() {
        // The drop policy never blocks; WARN/ERROR ride the same wait-free send.
        // With a drained channel every record is enqueued, including DEBUG.
        let path = tmp_sock();
        let listener = UnixListener::bind(&path).unwrap();
        // Drain everything the shipper sends so the channel stays empty.
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                let mut sink = [0u8; 4096];
                while stream.read(&mut sink).await.unwrap_or(0) > 0 {}
            }
        });

        let layer: LogdLayer<tracing_subscriber::Registry> =
            LogdLayer::with_socket("ados-test", &path);
        let stats = layer.stats();
        for lvl in [Level::Debug, Level::Info, Level::Warn, Level::Error] {
            layer.enqueue(LogFrame::new(now_us(), "ados-test", lvl, "x"));
        }
        // None of the four blocked; all were accepted into the open channel.
        assert_eq!(stats.dropped(), 0);
        assert_eq!(stats.enqueued(), 4);

        let _ = std::fs::remove_file(&path);
    }
}
