//! Ground station's outbound half of the aux MAVLink pair: batch bytes a
//! connected GCS client sends and radiate them on the aux uplink (radio_id 3)
//! instead of writing to a local flight controller that does not exist.
//!
//! The mirror of [`crate::aux_tee`] (drone -> ground), but simpler: the
//! ground's `wfb_tx -p3` process is spawned unconditionally by
//! `ados-groundlink` the moment its receive chain comes up, so unlike the
//! drone's `radio-aux.sock` there is no open/close stream lifecycle to
//! negotiate here — the ingress is always live, so this is just frame, batch,
//! and send.
//!
//! ## Why a client's outbound bytes end up here at all
//!
//! A ground station relaying a linked drone has no flight controller of its
//! own, so [`crate::connection::FcConnection::send_bytes`] used to be a
//! silent no-op for anything a connected GCS sent: the request reached the
//! ground station and went no further. This sender is what
//! `FcConnection::send_bytes` falls back to when no local FC writer is
//! installed, closing the ground-to-drone half of the relay (the drone-to-GCS
//! half has run since the aux downlink lane was wired up).
//!
//! ## Batching
//!
//! Mirrors [`crate::aux_tee`]'s batching window: several small frames (a
//! retry batch of `PARAM_REQUEST_READ`s, say) collapse into one datagram
//! rather than one radio transmission per frame, because this lane's loss
//! tracks packets per second rather than bytes.
//!
//! ## Shared system ids
//!
//! One uplink transmission reaches every drone in the fleet, and each drone
//! hands its flight controller only the frames addressed to that FC's system
//! id. Two linked aircraft heartbeating with the same id are therefore one
//! address: an ARM meant for one arms both. While the receive chain reports
//! such a conflict (`conflicting_system_ids` in the relayed-status sidecar),
//! every frame addressed to a conflicted id is dropped here and counted, until
//! an operator gives one aircraft a unique id.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ados_protocol::aux_mux::{self, AuxChannel, AUX_MAX_PAYLOAD};
use serde_json::Value;
use tokio::net::UdpSocket;
use tokio::sync::mpsc;

/// How long a partial batch waits for more frames before it is flushed as its
/// own datagram. Matches `aux_tee::BATCH_WINDOW` — the same lane, the same
/// packet-rate constraint, in the opposite direction.
const BATCH_WINDOW: Duration = Duration::from_millis(50);

/// Outbound queue depth. A client sending faster than the aux lane can carry
/// is a real backpressure condition on a lossy radio link, not a bug to size
/// away; bound it so a stalled uplink cannot grow this queue without limit.
const QUEUE_DEPTH: usize = 256;

/// How often the relayed-status sidecar is re-read for the conflict set. The
/// writer refreshes it every two seconds.
const CONFLICT_REFRESH: Duration = Duration::from_secs(1);

/// A sidecar older than this is from a stopped receive process; its conflict
/// set is not trusted, and nothing is blocked on it.
const CONFLICT_DOC_STALE_AFTER_S: f64 = 20.0;

/// A handle to the batching task. Cheap to clone; every clone shares the same
/// outbound queue and background sender.
#[derive(Clone)]
pub struct AuxUplinkSender {
    tx: mpsc::Sender<Vec<u8>>,
    blocked_sysid_conflict: Arc<AtomicU64>,
    queue_full_dropped: Arc<AtomicU64>,
    partial_dropped: Arc<AtomicU64>,
}

impl AuxUplinkSender {
    /// Queue `data` for the aux uplink. Returns whether it was queued.
    /// Best-effort: a full queue means the uplink is genuinely falling behind
    /// the client, and blocking the caller here would stall the connection
    /// handler that owns this byte stream, so an over-full queue drops the
    /// frame, counts it, and reports `false` rather than back pressuring the
    /// caller.
    pub fn send(&self, data: &[u8]) -> bool {
        if self.tx.try_send(data.to_vec()).is_err() {
            self.queue_full_dropped.fetch_add(1, Ordering::Relaxed);
            tracing::warn!(len = data.len(), "aux_uplink_queue_full_dropped_frame");
            return false;
        }
        true
    }

    /// Frames dropped because they were addressed to a system id two linked
    /// aircraft share. Cumulative.
    pub fn blocked_sysid_conflict(&self) -> u64 {
        self.blocked_sysid_conflict.load(Ordering::Relaxed)
    }

    /// Chunks dropped because the outbound queue was full. Cumulative.
    pub fn queue_full_dropped(&self) -> u64 {
        self.queue_full_dropped.load(Ordering::Relaxed)
    }

    /// Trailing partial frames dropped because they could not fit one aux
    /// datagram. Cumulative.
    pub fn partial_dropped(&self) -> u64 {
        self.partial_dropped.load(Ordering::Relaxed)
    }

    /// A sender whose queue nothing drains, for tests that need a full queue.
    #[cfg(test)]
    pub(crate) fn detached(depth: usize) -> (Self, mpsc::Receiver<Vec<u8>>) {
        let (tx, rx) = mpsc::channel(depth);
        let sender = Self {
            tx,
            blocked_sysid_conflict: Arc::new(AtomicU64::new(0)),
            queue_full_dropped: Arc::new(AtomicU64::new(0)),
            partial_dropped: Arc::new(AtomicU64::new(0)),
        };
        (sender, rx)
    }
}

/// Spawn the batching task, targeting the ground station's own aux-uplink
/// loopback ingress on `target_port` (paired with `ados-groundlink`'s
/// `AUX_TX_PORT`, currently 5602 on both sides of the aux pair by
/// convention — see that crate's `wfb_rx::args` for the receiving `wfb_tx`
/// this feeds). `ados-mavlink-router` does not depend on `ados-groundlink`,
/// so the port travels as a plain config value rather than a shared const,
/// and the conflict set is read from the sidecar under the run dir.
pub fn spawn(target_port: u16) -> AuxUplinkSender {
    let run_dir = std::env::var("ADOS_RUN_DIR").unwrap_or_else(|_| "/run/ados".to_string());
    spawn_with(
        target_port,
        PathBuf::from(run_dir).join("relayed-status.json"),
    )
}

fn spawn_with(target_port: u16, conflict_sidecar: PathBuf) -> AuxUplinkSender {
    let (tx, rx) = mpsc::channel::<Vec<u8>>(QUEUE_DEPTH);
    let blocked_sysid_conflict = Arc::new(AtomicU64::new(0));
    let partial_dropped = Arc::new(AtomicU64::new(0));
    let filter = ConflictFilter::new(conflict_sidecar, blocked_sysid_conflict.clone());
    tokio::spawn(run(rx, target_port, filter, partial_dropped.clone()));
    AuxUplinkSender {
        tx,
        blocked_sysid_conflict,
        queue_full_dropped: Arc::new(AtomicU64::new(0)),
        partial_dropped,
    }
}

/// The `target_system` a MAVLink frame is addressed to, when the frame decodes
/// and its message carries one.
pub(crate) fn frame_target_system(frame: &[u8]) -> Option<u8> {
    use ados_protocol::mavlink::Message as _;
    let (_, msg) = ados_protocol::mavlink::parse_any(frame).ok()?;
    msg.target_system_id()
}

/// The system ids the receive chain reports two linked slots sharing.
fn read_conflicts(path: &std::path::Path, now_unix: f64) -> Vec<u8> {
    let Some(doc) = std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
    else {
        return Vec::new();
    };
    let written_at = doc
        .get("wall_time_unix")
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    let age = now_unix - written_at;
    if written_at <= 0.0 || !(-1.0..=CONFLICT_DOC_STALE_AFTER_S).contains(&age) {
        return Vec::new();
    }
    doc.get("conflicting_system_ids")
        .and_then(Value::as_array)
        .map(|ids| {
            ids.iter()
                .filter_map(Value::as_u64)
                .filter_map(|id| u8::try_from(id).ok())
                .filter(|id| *id != 0)
                .collect()
        })
        .unwrap_or_default()
}

/// Drops frames addressed to a conflicted system id, re-reading the conflict
/// set at most once per [`CONFLICT_REFRESH`].
struct ConflictFilter {
    path: PathBuf,
    conflicts: Vec<u8>,
    read_at: Option<Instant>,
    blocked: Arc<AtomicU64>,
}

impl ConflictFilter {
    fn new(path: PathBuf, blocked: Arc<AtomicU64>) -> Self {
        Self {
            path,
            conflicts: Vec::new(),
            read_at: None,
            blocked,
        }
    }

    fn refresh(&mut self) {
        let now = Instant::now();
        if self
            .read_at
            .is_some_and(|at| now.duration_since(at) < CONFLICT_REFRESH)
        {
            return;
        }
        self.read_at = Some(now);
        let wall = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0);
        let next = read_conflicts(&self.path, wall);
        if next != self.conflicts {
            if next.is_empty() {
                tracing::info!("aux_uplink_sysid_conflict_cleared");
            } else {
                tracing::warn!(system_ids = ?next, "aux_uplink_sysid_conflict_blocking");
            }
            self.conflicts = next;
        }
    }

    /// The bytes of `chunk` that may go on the uplink.
    ///
    /// With no conflict the chunk passes untouched. During a conflict it is
    /// split into whole frames and each addressed to a conflicted id is
    /// dropped. A trailing remainder that is not a whole frame cannot be
    /// shown to be safe, so it is dropped too and counted with the rest.
    fn admit(&mut self, chunk: Vec<u8>) -> Vec<u8> {
        self.refresh();
        if self.conflicts.is_empty() {
            return chunk;
        }
        let frames = aux_mux::split_frames(&chunk);
        let whole: usize = frames.iter().map(|f| f.len()).sum();
        let mut out = Vec::with_capacity(chunk.len());
        let mut dropped = 0u64;
        for frame in frames {
            match frame_target_system(frame) {
                Some(t) if self.conflicts.contains(&t) => dropped += 1,
                _ => out.extend_from_slice(frame),
            }
        }
        if whole < chunk.len() {
            dropped += 1;
        }
        if dropped > 0 {
            self.blocked.fetch_add(dropped, Ordering::Relaxed);
        }
        out
    }
}

/// Break a chunk that cannot fit one datagram into pieces that can.
///
/// A client's bytes can exceed the aux payload ceiling (a mission or parameter
/// burst), and such a chunk used to be handed to the encoder whole, rejected,
/// and dropped with a warning and no counter.
///
/// The split is on MAVLink frame boundaries, not arbitrary byte offsets: the
/// receiver splits an aux payload back into frames by their own headers, so
/// cutting mid-frame would deliver two fragments that each fail CRC and read as
/// line noise. Returns the pieces plus the trailing bytes after the last whole
/// frame, which the caller carries as its own piece when it fits a datagram
/// and otherwise drops and counts. A chunk that yields no whole frame is passed
/// through unchanged and left for the encoder to reject, because guessing at a
/// boundary is worse than an honest failure.
fn split_oversize(chunk: &[u8]) -> (Vec<&[u8]>, &[u8]) {
    if chunk.len() <= AUX_MAX_PAYLOAD {
        return (vec![chunk], &[]);
    }
    let frames = aux_mux::split_frames(chunk);
    if frames.is_empty() {
        return (vec![chunk], &[]);
    }
    let whole: usize = frames.iter().map(|f| f.len()).sum();
    (frames, &chunk[whole..])
}

async fn flush(sock: &UdpSocket, target: SocketAddr, batch: &mut Vec<u8>) {
    if batch.is_empty() {
        return;
    }
    match aux_mux::encode(AuxChannel::Mavlink, batch) {
        Some(datagram) => {
            if let Err(e) = sock.send_to(&datagram, target).await {
                tracing::warn!(error = %e, "aux_uplink_send_failed");
            }
        }
        None => tracing::warn!(len = batch.len(), "aux_uplink_encode_failed"),
    }
    batch.clear();
}

/// The pieces of `chunk` to batch: the frame-boundary split plus the trailing
/// remainder carried as its own piece when it fits one datagram. A remainder
/// too large to carry is dropped and counted in `partial_dropped`.
fn pieces_to_send<'a>(chunk: &'a [u8], partial_dropped: &AtomicU64) -> Vec<&'a [u8]> {
    let (mut pieces, remainder) = split_oversize(chunk);
    if !remainder.is_empty() {
        if remainder.len() <= AUX_MAX_PAYLOAD {
            pieces.push(remainder);
        } else {
            partial_dropped.fetch_add(1, Ordering::Relaxed);
            tracing::warn!(len = remainder.len(), "aux_uplink_partial_dropped");
        }
    }
    pieces
}

async fn run(
    mut rx: mpsc::Receiver<Vec<u8>>,
    target_port: u16,
    mut filter: ConflictFilter,
    partial_dropped: Arc<AtomicU64>,
) {
    let sock = match UdpSocket::bind(("127.0.0.1", 0)).await {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(error = %e, "aux_uplink_bind_failed");
            return;
        }
    };
    let target: SocketAddr = ([127, 0, 0, 1], target_port).into();
    let mut batch: Vec<u8> = Vec::new();
    // An ABSOLUTE deadline for the batch currently being filled, set when the
    // first frame lands in it.
    //
    // It used to be rebuilt on every loop iteration, which meant every arriving
    // frame reset the window: a client sending steadily faster than the window
    // never let it elapse, so nothing went out until the batch happened to
    // reach the payload ceiling. On a control link that is seconds of added
    // command latency, and it only clears when the client goes quiet.
    let mut batch_deadline: Option<tokio::time::Instant> = None;

    loop {
        let sleep_until =
            batch_deadline.unwrap_or_else(|| tokio::time::Instant::now() + BATCH_WINDOW);
        let deadline = tokio::time::sleep_until(sleep_until);
        tokio::pin!(deadline);
        tokio::select! {
            frame = rx.recv() => match frame {
                Some(f) => {
                    let f = filter.admit(f);
                    if f.is_empty() {
                        continue;
                    }
                    for piece in pieces_to_send(&f, &partial_dropped) {
                        if batch.len() + piece.len() > AUX_MAX_PAYLOAD {
                            flush(&sock, target, &mut batch).await;
                            batch_deadline = None;
                        }
                        batch.extend_from_slice(piece);
                        if batch_deadline.is_none() {
                            batch_deadline = Some(tokio::time::Instant::now() + BATCH_WINDOW);
                        }
                    }
                }
                None => break,
            },
            _ = &mut deadline => {
                flush(&sock, target, &mut batch).await;
                batch_deadline = None;
            }
        }
    }
    // Drain whatever the queue was holding before the channel closed rather
    // than discarding a client's last request on shutdown.
    flush(&sock, target, &mut batch).await;
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod conflict_tests;
