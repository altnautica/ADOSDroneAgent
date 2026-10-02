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
}

impl AuxUplinkSender {
    /// Queue `data` for the aux uplink. Best-effort: a full queue means the
    /// uplink is genuinely falling behind the client, and blocking the
    /// caller here would stall the connection handler that owns this byte
    /// stream, so an over-full queue drops the frame rather than back
    /// pressuring the caller.
    pub fn send(&self, data: &[u8]) {
        if self.tx.try_send(data.to_vec()).is_err() {
            tracing::warn!(len = data.len(), "aux_uplink_queue_full_dropped_frame");
        }
    }

    /// Frames dropped because they were addressed to a system id two linked
    /// aircraft share. Cumulative.
    pub fn blocked_sysid_conflict(&self) -> u64 {
        self.blocked_sysid_conflict.load(Ordering::Relaxed)
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
    let filter = ConflictFilter::new(conflict_sidecar, blocked_sysid_conflict.clone());
    tokio::spawn(run(rx, target_port, filter));
    AuxUplinkSender {
        tx,
        blocked_sysid_conflict,
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
/// A client's bytes arrive as raw TCP reads of up to several KB, so a mission
/// or parameter burst can exceed the aux payload ceiling. Such a chunk used to
/// be handed to the encoder whole, rejected, and dropped with a warning and no
/// counter — a silent loss of exactly the traffic an operator is most likely to
/// be watching.
///
/// The split is on MAVLink frame boundaries, not arbitrary byte offsets: the
/// receiver splits an aux payload back into frames by their own headers, so
/// cutting mid-frame would deliver two fragments that each fail CRC and read as
/// line noise. A chunk that yields no whole frame is passed through unchanged
/// and left for the encoder to reject, because guessing at a boundary is worse
/// than an honest failure.
fn split_oversize(chunk: &[u8]) -> Vec<&[u8]> {
    if chunk.len() <= AUX_MAX_PAYLOAD {
        return vec![chunk];
    }
    let frames = aux_mux::split_frames(chunk);
    if frames.is_empty() {
        return vec![chunk];
    }
    frames
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

async fn run(mut rx: mpsc::Receiver<Vec<u8>>, target_port: u16, mut filter: ConflictFilter) {
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
                    for piece in split_oversize(&f) {
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
mod tests {
    use super::*;
    use tokio::net::UdpSocket as TestSocket;

    /// A MAVLink v2 frame carrying `payload_len` bytes.
    fn mav2(payload_len: u8, seq: u8) -> Vec<u8> {
        let mut f = vec![0xFD, payload_len, 0, 0, seq, 1, 1, 0, 0, 0];
        f.extend(std::iter::repeat_n(0xAB, payload_len as usize));
        f.extend_from_slice(&[0x00, 0x00]); // checksum
        f
    }

    #[test]
    fn a_chunk_that_fits_is_passed_through_whole() {
        let c = mav2(10, 1);
        assert_eq!(split_oversize(&c), vec![c.as_slice()]);
    }

    #[test]
    fn an_oversize_chunk_is_split_on_frame_boundaries_rather_than_dropped() {
        // A mission or parameter burst arrives as one TCP read and used to be
        // handed to the encoder whole, rejected, and dropped with a warning and
        // no counter — a silent loss of exactly the traffic an operator is most
        // likely to be watching at the time.
        let mut chunk = Vec::new();
        let mut expected = 0usize;
        while chunk.len() <= AUX_MAX_PAYLOAD {
            chunk.extend_from_slice(&mav2(200, expected as u8));
            expected += 1;
        }
        let pieces = split_oversize(&chunk);
        assert_eq!(pieces.len(), expected, "every frame must survive the split");
        for p in &pieces {
            assert!(
                p.len() <= AUX_MAX_PAYLOAD,
                "a piece still cannot be encoded"
            );
            assert_eq!(p[0], 0xFD, "each piece starts on a frame boundary");
        }
    }

    #[test]
    fn an_unparseable_oversize_chunk_is_passed_through_rather_than_guessed_at() {
        // Cutting at an arbitrary offset would deliver two halves that each
        // fail CRC on the far side and read as line noise. An honest encoder
        // rejection is better than a fabricated boundary.
        let chunk = vec![0x00u8; AUX_MAX_PAYLOAD + 50];
        assert_eq!(split_oversize(&chunk).len(), 1);
    }

    /// `start_paused` drives virtual time, so a steady stream can be simulated
    /// faster than the batch window without the test sleeping for real.
    #[tokio::test(start_paused = true)]
    async fn a_steady_stream_still_flushes_on_the_window() {
        // The regression: the window was rebuilt on every loop iteration, so
        // each arriving frame reset it. A client sending faster than the window
        // never let it elapse and nothing went out until the batch happened to
        // reach the payload ceiling — seconds of added command latency on a
        // control link, clearing only when the client went quiet.
        let listener = TestSocket::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let sender = spawn(port);

        // Send steadily at half the batch window for well over one window.
        for i in 0..12u8 {
            sender.send(&mav2(4, i));
            tokio::time::sleep(BATCH_WINDOW / 2).await;
        }

        // Something must already have gone out: the window elapsed several
        // times over, and the batch is nowhere near the payload ceiling.
        let mut buf = [0u8; 4096];
        let got = tokio::time::timeout(
            std::time::Duration::from_millis(50),
            listener.recv_from(&mut buf),
        )
        .await;
        assert!(
            got.is_ok(),
            "a steady stream never flushed; the batch window is being reset by \
             every arrival instead of running from the first frame"
        );
    }

    #[tokio::test]
    async fn a_sent_frame_arrives_framed_on_the_target_port() {
        let listener = TestSocket::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let sender = spawn(port);
        sender.send(b"\xfdhello-frame-bytes");

        let mut buf = [0u8; 256];
        let (n, _) = tokio::time::timeout(Duration::from_millis(500), listener.recv_from(&mut buf))
            .await
            .expect("no datagram arrived within the batch window")
            .unwrap();

        let (channel, payload) =
            aux_mux::decode(&buf[..n]).expect("must decode as a valid aux datagram");
        assert_eq!(channel, AuxChannel::Mavlink);
        assert_eq!(payload, b"\xfdhello-frame-bytes");
    }

    #[tokio::test]
    async fn several_quick_frames_batch_into_one_datagram() {
        let listener = TestSocket::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let sender = spawn(port);
        sender.send(b"AAA");
        sender.send(b"BBB");
        sender.send(b"CCC");

        let mut buf = [0u8; 256];
        let (n, _) = tokio::time::timeout(Duration::from_millis(500), listener.recv_from(&mut buf))
            .await
            .expect("no datagram arrived")
            .unwrap();

        let (_, payload) = aux_mux::decode(&buf[..n]).unwrap();
        assert_eq!(payload, b"AAABBBCCC");

        // Only one datagram — the three frames shared one batch window rather
        // than each triggering its own radio transmission.
        let none_more =
            tokio::time::timeout(Duration::from_millis(80), listener.recv_from(&mut buf)).await;
        assert!(none_more.is_err(), "expected no second datagram");
    }
}

#[cfg(test)]
mod conflict_tests {
    use super::*;
    use ados_protocol::mavlink::ardupilotmega::{MavCmd, MavMessage, COMMAND_LONG_DATA};
    use ados_protocol::mavlink::{serialize_v2, MavHeader};

    fn arm_for(target_system: u8) -> Vec<u8> {
        let msg = MavMessage::COMMAND_LONG(COMMAND_LONG_DATA {
            target_system,
            target_component: 1,
            command: MavCmd::MAV_CMD_COMPONENT_ARM_DISARM,
            confirmation: 0,
            param1: 1.0,
            param2: 0.0,
            param3: 0.0,
            param4: 0.0,
            param5: 0.0,
            param6: 0.0,
            param7: 0.0,
        });
        serialize_v2(
            MavHeader {
                system_id: 255,
                component_id: 190,
                sequence: 0,
            },
            &msg,
        )
        .unwrap()
    }

    fn wall_now() -> f64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs_f64()
    }

    fn sidecar(dir: &std::path::Path, written_at: f64, conflicts: &[u8]) -> PathBuf {
        let path = dir.join("relayed-status.json");
        let doc = serde_json::json!({
            "wall_time_unix": written_at,
            "conflicting_system_ids": conflicts,
        });
        std::fs::write(&path, doc.to_string()).unwrap();
        path
    }

    #[test]
    fn a_command_to_a_shared_system_id_is_held_and_counted() {
        let dir = tempfile::tempdir().unwrap();
        let path = sidecar(dir.path(), wall_now(), &[1]);
        let blocked = Arc::new(AtomicU64::new(0));
        let mut filter = ConflictFilter::new(path, blocked.clone());

        let mut chunk = arm_for(1);
        let other = arm_for(2);
        chunk.extend_from_slice(&other);
        assert_eq!(filter.admit(chunk), other, "only system 2's frame leaves");
        assert_eq!(blocked.load(Ordering::Relaxed), 1);

        // Broadcast is not an address two aircraft can share.
        let broadcast = arm_for(0);
        assert_eq!(filter.admit(broadcast.clone()), broadcast);
    }

    #[test]
    fn without_a_live_conflict_report_nothing_is_held() {
        let dir = tempfile::tempdir().unwrap();
        let frame = arm_for(1);

        let none = sidecar(dir.path(), wall_now(), &[]);
        let mut filter = ConflictFilter::new(none, Arc::new(AtomicU64::new(0)));
        assert_eq!(filter.admit(frame.clone()), frame);

        // A conflict written by a receive process that has since stopped.
        let stale = sidecar(
            dir.path(),
            wall_now() - CONFLICT_DOC_STALE_AFTER_S - 5.0,
            &[1],
        );
        let mut filter = ConflictFilter::new(stale, Arc::new(AtomicU64::new(0)));
        assert_eq!(filter.admit(frame.clone()), frame);

        let mut filter =
            ConflictFilter::new(dir.path().join("absent.json"), Arc::new(AtomicU64::new(0)));
        assert_eq!(filter.admit(frame.clone()), frame);
    }
}
