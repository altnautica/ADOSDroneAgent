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

use std::net::SocketAddr;
use std::time::Duration;

use ados_protocol::aux_mux::{self, AuxChannel, AUX_MAX_PAYLOAD};
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

/// A handle to the batching task. Cheap to clone; every clone shares the same
/// outbound queue and background sender.
#[derive(Clone)]
pub struct AuxUplinkSender {
    tx: mpsc::Sender<Vec<u8>>,
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
}

/// Spawn the batching task, targeting the ground station's own aux-uplink
/// loopback ingress on `target_port` (paired with `ados-groundlink`'s
/// `AUX_TX_PORT`, currently 5602 on both sides of the aux pair by
/// convention — see that crate's `wfb_rx::args` for the receiving `wfb_tx`
/// this feeds). `ados-mavlink-router` does not depend on `ados-groundlink`,
/// so the port travels as a plain config value rather than a shared const.
pub fn spawn(target_port: u16) -> AuxUplinkSender {
    let (tx, rx) = mpsc::channel::<Vec<u8>>(QUEUE_DEPTH);
    tokio::spawn(run(rx, target_port));
    AuxUplinkSender { tx }
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

async fn run(mut rx: mpsc::Receiver<Vec<u8>>, target_port: u16) {
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
