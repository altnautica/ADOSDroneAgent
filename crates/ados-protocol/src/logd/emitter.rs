//! A reusable, non-blocking emitter that ships discrete [`EventFrame`] records
//! from any Rust service to the logging daemon's ingest socket.
//!
//! The log-shipping [`crate::logd::layer`] carries `tracing` *log* events; this
//! emitter carries the structured *events* a service wants to record as
//! first-class, queryable rows (a regulatory-gate verdict, a bind-session
//! lifecycle transition, a received-side link-proof state change). Until this
//! module existed, only log lines reached the daemon; a service had no way to
//! emit a typed event with an open detail map. This closes that gap with the
//! same transport discipline as the log layer:
//!
//! - [`EventEmitter::emit`] redacts any secret-bearing detail field, frames an
//!   [`EventFrame`], and hands it to a bounded channel with a non-blocking send.
//!   A full channel drops the event and counts it — the producer never blocks.
//! - A single background task drains the channel, batches frames, and writes
//!   them to the ingest socket through a reconnecting writer. An absent socket
//!   (the daemon not yet up, or restarting) backs off quietly; it never panics,
//!   never blocks the producer, and never logs an error that could disrupt the
//!   service.
//! - The writer's hot path never emits a `tracing` event, so shipping an event
//!   can never recurse into another shipped log.
//!
//! Unlike the log layer, this module is **not** behind the `tracing-layer`
//! feature: events are a first-class capture path a service reaches for directly
//! (`emitter.emit(...)`), independent of whether the binary installs the log
//! layer. Redaction happens before a frame ever leaves the process, so a secret
//! is never written to the socket or to disk.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::io::AsyncWriteExt;
use tokio::net::UnixStream;
use tokio::sync::mpsc;

use crate::logd::{EventFrame, Fields, IngestFrame, Level};

/// Default ingest socket path. Producers connect here to write framed records.
/// The single source of truth for the path shared by the log layer and the
/// event emitter.
pub const DEFAULT_INGEST_SOCK: &str = "/run/ados/logd.sock";

/// Channel capacity between an emitter and its background shipper. Events are
/// low-rate (a verdict, a lifecycle transition), so a modest buffer rides a
/// brief writer stall without dropping while bounding pinned memory.
pub const EVENT_CHANNEL_CAPACITY: usize = 256;

/// Maximum frames coalesced into one socket write.
const BATCH_MAX_FRAMES: usize = 64;

/// How long the shipper waits to accumulate a batch before flushing what it has.
const BATCH_LINGER: Duration = Duration::from_millis(100);

/// Reconnect backoff schedule (milliseconds). The writer steps through these on
/// repeated connect failures and holds at the last value, so an absent socket
/// produces a slow, quiet retry rather than a hot loop.
const BACKOFF_MS: [u64; 5] = [250, 500, 1000, 2000, 5000];

/// Counters surfaced for diagnostics: events enqueued for shipping and events
/// dropped because the channel was full. A visible drop is better than a silent
/// one. Cheap atomics so the emit path stays lock-free.
#[derive(Debug, Default)]
pub struct EmitterStats {
    enqueued: AtomicU64,
    dropped: AtomicU64,
}

impl EmitterStats {
    fn record_enqueued(&self) {
        self.enqueued.fetch_add(1, Ordering::Relaxed);
    }

    fn record_dropped(&self) {
        self.dropped.fetch_add(1, Ordering::Relaxed);
    }

    /// Total events handed to the shipper channel.
    pub fn enqueued(&self) -> u64 {
        self.enqueued.load(Ordering::Relaxed)
    }

    /// Total events dropped at the channel boundary under backpressure.
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

/// Ships structured events to the logging daemon's ingest socket.
///
/// Construct it with [`EventEmitter::new`] (or [`EventEmitter::with_socket`])
/// inside a running tokio runtime — the background shipper is spawned at
/// construction. Clone it freely: every clone shares the one shipper channel and
/// the one stats handle, so a service can hand a clone to each task that needs to
/// record an event without standing up a second shipper.
#[derive(Clone)]
pub struct EventEmitter {
    source: Arc<str>,
    tx: mpsc::Sender<EventFrame>,
    stats: Arc<EmitterStats>,
}

impl EventEmitter {
    /// Build an emitter for the default ingest socket, tagging every event with
    /// `source` (the binary name). Spawns the background shipper on the current
    /// tokio runtime; must be called from within a runtime context.
    pub fn new(source: impl Into<String>) -> Self {
        Self::with_socket(source, DEFAULT_INGEST_SOCK)
    }

    /// Build an emitter that ships to an explicit socket path (used by tests).
    /// Spawns the background shipper on the current tokio runtime.
    pub fn with_socket(source: impl Into<String>, socket: impl AsRef<Path>) -> Self {
        let (tx, rx) = mpsc::channel::<EventFrame>(EVENT_CHANNEL_CAPACITY);
        let stats = Arc::new(EmitterStats::default());
        let path = socket.as_ref().to_path_buf();
        tokio::spawn(shipper_task(rx, path));
        Self {
            source: source.into().into(),
            tx,
            stats,
        }
    }

    /// The diagnostic counters for this emitter (enqueued / dropped).
    pub fn stats(&self) -> Arc<EmitterStats> {
        Arc::clone(&self.stats)
    }

    /// Record one event: `kind` is the dotted classifier, `severity` the level,
    /// `detail` the open field map (redacted in place before it leaves the
    /// process). Wait-free: a full channel drops the event and counts it, a
    /// closed channel (shipper gone) is a silent drop. Never blocks the caller,
    /// so an event recorded from a hot loop or a heartbeat tick cannot stall.
    pub fn emit(&self, kind: impl Into<String>, severity: Level, detail: Fields) {
        let mut frame = EventFrame::new(now_us(), kind.into(), self.source.to_string(), severity);
        frame.detail = detail;
        // Redact before the frame leaves the process: a secret-bearing detail
        // field is hashed at the source so a raw value never reaches the socket
        // or disk (the daemon redacts again at ingest as belt-and-suspenders).
        frame.redact_detail();
        match self.tx.try_send(frame) {
            Ok(()) => self.stats.record_enqueued(),
            Err(mpsc::error::TrySendError::Full(_)) => self.stats.record_dropped(),
            Err(mpsc::error::TrySendError::Closed(_)) => self.stats.record_dropped(),
        }
    }
}

/// Microsecond epoch timestamp for the current instant.
fn now_us() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_micros() as i64)
        .unwrap_or(0)
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
                // lost on the durable path, but journald still has the line the
                // service logged alongside the event.
                self.stream = None;
                false
            }
        }
    }
}

/// Encode a batch of event frames into one length-prefixed byte buffer. A frame
/// that fails to encode (e.g. an oversized payload) is skipped rather than
/// aborting the batch, so one bad record never blocks the rest.
fn encode_batch(batch: &[EventFrame]) -> Vec<u8> {
    let mut buf = Vec::new();
    for frame in batch {
        if let Ok(bytes) = IngestFrame::Event(frame.clone()).encode() {
            buf.extend_from_slice(&bytes);
        }
    }
    buf
}

/// Drain the channel, batch frames, and ship them to the ingest socket. Runs
/// until the channel closes (every emitter handle dropped), then exits. Its hot
/// path never emits a `tracing` event, so shipping an event can never recurse.
async fn shipper_task(mut rx: mpsc::Receiver<EventFrame>, path: PathBuf) {
    let mut writer = SocketWriter::new(path);
    let mut batch: Vec<EventFrame> = Vec::with_capacity(BATCH_MAX_FRAMES);

    loop {
        // Block for the first frame of a batch so the task idles cheaply when no
        // events flow. `None` means every sender dropped: flush and exit.
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
                Ok(None) => break,
                Err(_) => break,
            }
        }

        // Connect (with backoff) and ship. If the socket is unreachable, the
        // batch is dropped; the service's own log line is the always-on fallback.
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
    use rmpv::Value as MpVal;
    use tokio::io::AsyncReadExt;
    use tokio::net::{UnixListener, UnixStream};

    use crate::frame::{decode_len, HEADER_SIZE};
    use crate::logd::LOGD_MAX_FRAME;

    fn tmp_sock() -> PathBuf {
        // A process-unique monotonic counter avoids the same-microsecond
        // collision two concurrent tests would otherwise hit on `now_us()`.
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let mut p = std::env::temp_dir();
        let uniq = format!(
            "ados-logd-emitter-test-{}-{}-{}.sock",
            std::process::id(),
            now_us(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        );
        p.push(uniq);
        let _ = std::fs::remove_file(&p);
        p
    }

    fn detail(pairs: &[(&str, MpVal)]) -> Fields {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect()
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

    #[tokio::test]
    async fn ships_a_redacted_event_to_a_live_socket() {
        let path = tmp_sock();
        let listener = UnixListener::bind(&path).unwrap();
        let accept = {
            let listener = listener;
            tokio::spawn(async move {
                let (mut stream, _) = listener.accept().await.unwrap();
                read_one(&mut stream).await
            })
        };

        let emitter = EventEmitter::with_socket("ados-test", &path);
        emitter.emit(
            "radio.reg_gate",
            Level::Warn,
            detail(&[
                ("result", MpVal::from("blocked")),
                ("channel", MpVal::from(149u64)),
                // A secret-bearing detail field must be redacted before it leaves.
                ("session_token", MpVal::from("tok_supersecretvalue")),
            ]),
        );

        let frame = tokio::time::timeout(Duration::from_secs(5), accept)
            .await
            .expect("event delivered within the deadline")
            .expect("accept task ok");

        match frame {
            IngestFrame::Event(evt) => {
                assert_eq!(evt.source, "ados-test");
                assert_eq!(evt.kind, "radio.reg_gate");
                assert_eq!(evt.severity, Level::Warn);
                assert_eq!(
                    evt.detail.get("result").and_then(|v| v.as_str()),
                    Some("blocked")
                );
                assert_eq!(
                    evt.detail.get("channel").and_then(|v| v.as_u64()),
                    Some(149)
                );
                assert_eq!(
                    evt.detail.get("session_token").and_then(|v| v.as_str()),
                    Some("redacted:tok_...160e465f")
                );
            }
            other => panic!("expected an event frame, got {other:?}"),
        }
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn absent_socket_never_blocks_or_panics() {
        // Point the emitter at a path with no listener. Recording events must not
        // block the producer or panic; frames are dropped on the durable path
        // while the shipper backs off quietly.
        let path = tmp_sock(); // never bound
        let emitter = EventEmitter::with_socket("ados-test", &path);
        for i in 0..20 {
            emitter.emit("radio.bind", Level::Info, detail(&[("n", MpVal::from(i))]));
        }
        assert!(emitter.stats().enqueued() >= 1, "at least one enqueued");
        // Give the shipper a moment to attempt and fail a connection; it must
        // still be alive and the test still running (no panic propagated).
        tokio::time::sleep(Duration::from_millis(50)).await;
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn full_channel_drops_without_blocking_and_counts_it() {
        // Point at an absent socket so the shipper parks in backoff and never
        // drains, then flood past the channel capacity. Emits must return
        // immediately and the overflow is counted.
        let path = tmp_sock(); // never bound
        let emitter = EventEmitter::with_socket("ados-test", &path);
        let total = EVENT_CHANNEL_CAPACITY * 2;
        for n in 0..total {
            emitter.emit(
                "radio.rf_unverified",
                Level::Warn,
                detail(&[("n", MpVal::from(n as u64))]),
            );
        }
        let enq = emitter.stats().enqueued();
        let drp = emitter.stats().dropped();
        assert_eq!(enq + drp, total as u64, "every event accounted for");
        assert!(drp >= 1, "overflow past capacity was dropped and counted");
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn a_clone_shares_the_shipper_and_stats() {
        let path = tmp_sock();
        let listener = UnixListener::bind(&path).unwrap();
        // Drain everything so the channel stays empty across both handles.
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                let mut sink = [0u8; 4096];
                while stream.read(&mut sink).await.unwrap_or(0) > 0 {}
            }
        });

        let emitter = EventEmitter::with_socket("ados-test", &path);
        let clone = emitter.clone();
        emitter.emit("radio.bind", Level::Info, Fields::new());
        clone.emit("radio.bind", Level::Info, Fields::new());
        // Both handles report through the one shared stats counter.
        assert_eq!(emitter.stats().enqueued(), 2);
        assert_eq!(clone.stats().dropped(), 0);
        let _ = std::fs::remove_file(&path);
    }
}
