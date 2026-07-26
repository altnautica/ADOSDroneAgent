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

    loop {
        let deadline = tokio::time::sleep(BATCH_WINDOW);
        tokio::pin!(deadline);
        tokio::select! {
            frame = rx.recv() => match frame {
                Some(f) => {
                    if batch.len() + f.len() > AUX_MAX_PAYLOAD {
                        flush(&sock, target, &mut batch).await;
                    }
                    batch.extend_from_slice(&f);
                }
                None => break,
            },
            _ = &mut deadline => {
                flush(&sock, target, &mut batch).await;
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
