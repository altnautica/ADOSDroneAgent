//! The beacon's way onto the plugin bus: the plugin host's event export socket
//! (seam K, contract `plugin.events`), publish op.
//!
//! The connection is opened when the first beacon arrives and kept; each
//! beacon is one `{"v":1,"op":"publish","topic","payload"}` line. The host
//! answers only a refused line, so a reader task drains those answers into
//! the refusal count. With no plugin host (a node that runs none, or one that
//! is restarting) a beacon is dropped and counted, and the next connect is
//! tried after [`RETRY`]: a beacon is only worth its freshness, so nothing is
//! queued for later.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use ados_protocol::follow_beacon::{FollowBeacon, FOLLOW_BEACON_TOPIC};
use serde::Serialize;
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::net::unix::OwnedWriteHalf;
use tokio::net::UnixStream;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
// Reconnect pacing is wall time: it paces a local socket, not a control lane.
use tokio::time::Instant;

/// The `plugin.events` wire version this client speaks.
const EXPORT_VERSION: u32 = 1;

/// Wait between connect attempts while the socket is absent.
pub(super) const RETRY: Duration = Duration::from_millis(500);

/// Bound on one line write; a host that stops reading is dropped.
const WRITE_DEADLINE: Duration = Duration::from_millis(500);

/// Longest refusal line read back from the host.
const REPLY_MAX: usize = 4096;

/// Delivery counts. Never reset.
#[derive(Debug, Default)]
pub struct ExportStats {
    /// Beacons written to the socket.
    pub exported: AtomicU64,
    /// Beacons dropped: no socket, a failed write, or a full queue.
    pub dropped: AtomicU64,
    /// Lines the host refused.
    pub refused: AtomicU64,
    /// A connection is open.
    pub connected: AtomicBool,
}

/// A point-in-time copy of [`ExportStats`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct ExportSnapshot {
    pub exported: u64,
    pub export_dropped: u64,
    pub export_refused: u64,
    pub export_connected: bool,
}

impl ExportStats {
    pub fn snapshot(&self) -> ExportSnapshot {
        ExportSnapshot {
            exported: self.exported.load(Ordering::Relaxed),
            export_dropped: self.dropped.load(Ordering::Relaxed),
            export_refused: self.refused.load(Ordering::Relaxed),
            export_connected: self.connected.load(Ordering::Relaxed),
        }
    }
}

/// The export socket: `ADOS_PLUGIN_HOST_DIR` when set, else the registered
/// `plugin-events` socket under the run dir.
pub fn events_socket() -> PathBuf {
    match std::env::var_os("ADOS_PLUGIN_HOST_DIR").filter(|d| !d.is_empty()) {
        Some(dir) => PathBuf::from(dir).join(ados_protocol::paths::PLUGIN_EVENTS_SOCK),
        None => ados_protocol::paths::run_path("plugin-events"),
    }
}

/// One beacon as its publish line (newline included).
pub fn publish_line(beacon: &FollowBeacon) -> Vec<u8> {
    let mut line = serde_json::to_vec(&serde_json::json!({
        "v": EXPORT_VERSION,
        "op": "publish",
        "topic": FOLLOW_BEACON_TOPIC,
        "payload": beacon,
    }))
    .unwrap_or_default();
    line.push(b'\n');
    line
}

struct Conn {
    writer: OwnedWriteHalf,
    reader: JoinHandle<()>,
}

impl Drop for Conn {
    fn drop(&mut self) {
        self.reader.abort();
    }
}

async fn connect(path: &PathBuf, stats: &Arc<ExportStats>) -> Option<Conn> {
    let stream = match UnixStream::connect(path).await {
        Ok(s) => s,
        Err(e) => {
            tracing::debug!(error = %e, path = %path.display(), "follow_beacon_export_unreachable");
            return None;
        }
    };
    let (read_half, writer) = stream.into_split();
    let reader_stats = stats.clone();
    let reader = tokio::spawn(async move {
        let mut reader = BufReader::new(read_half);
        while let Ok(Some(line)) =
            ados_protocol::ipc::read_newline_line(&mut reader, REPLY_MAX).await
        {
            reader_stats.refused.fetch_add(1, Ordering::Relaxed);
            tracing::warn!(
                reply = %String::from_utf8_lossy(&line),
                "follow_beacon_export_refused"
            );
        }
    });
    stats.connected.store(true, Ordering::Relaxed);
    tracing::info!(path = %path.display(), "follow_beacon_export_connected");
    Some(Conn { writer, reader })
}

/// Deliver beacons from `rx` to the export socket at `path` until `rx` closes.
pub async fn run(path: PathBuf, mut rx: mpsc::Receiver<FollowBeacon>, stats: Arc<ExportStats>) {
    let mut conn: Option<Conn> = None;
    let mut retry_at: Option<Instant> = None;
    while let Some(beacon) = rx.recv().await {
        if conn.is_none() && retry_at.is_none_or(|t| Instant::now() >= t) {
            conn = connect(&path, &stats).await;
            retry_at = conn.is_none().then(|| Instant::now() + RETRY);
        }
        let Some(c) = conn.as_mut() else {
            stats.dropped.fetch_add(1, Ordering::Relaxed);
            continue;
        };
        let line = publish_line(&beacon);
        let wrote = matches!(
            tokio::time::timeout(WRITE_DEADLINE, c.writer.write_all(&line)).await,
            Ok(Ok(()))
        );
        if wrote {
            stats.exported.fetch_add(1, Ordering::Relaxed);
        } else {
            stats.dropped.fetch_add(1, Ordering::Relaxed);
            stats.connected.store(false, Ordering::Relaxed);
            tracing::warn!("follow_beacon_export_write_failed");
            conn = None;
            retry_at = Some(Instant::now() + RETRY);
        }
    }
}
