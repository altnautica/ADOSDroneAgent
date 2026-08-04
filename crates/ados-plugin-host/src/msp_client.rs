//! Client to the MSP IPC socket the MAVLink router serves at
//! `/run/ados/msp.sock`.
//!
//! The sibling byte plane to [`crate::mavlink_client`]: an MSP flight controller
//! (Betaflight / iNav / KISS) speaks no MAVLink, so the router carries its raw
//! MSP bytes on this socket instead. The framing is identical to the MAVLink
//! socket — 4-byte big-endian length + raw bytes, bidirectional, 256-deep — so
//! this client is the MAVLink client minus the MAVLink-only off-box declaration
//! (MSP has no router-side origin concept). A frame written toward the socket is
//! a command toward the FC; FC->host bytes fan out on the same connection. The
//! router never parses the stream, and neither does this client: the MSP codec
//! (`ados_protocol::msp`) lives at the plugin/SDK edge, not here.
//!
//! On connect a reader task drains FC->host frames into a broadcast channel; each
//! plugin `msp.subscribe` pump takes its own [`broadcast::Receiver`]. Plugin->FC
//! commands enqueue on a bounded mpsc a writer task drains, so
//! [`send_bytes`](Self::send_bytes) is a plain non-async call.

use std::io;
use std::path::Path;
use std::time::Duration;

use ados_protocol::frame::{encode_frame, MAVLINK_MAX_FRAME};
use ados_protocol::ipc::{connect_with_retry, read_length_prefixed};
use tokio::io::AsyncWriteExt;
use tokio::sync::{broadcast, mpsc};
use tokio::task::JoinHandle;

/// Inbound FC-frame fanout depth. Matches the router's queue depth so a plugin
/// pump that briefly stalls lags rather than wedging the reader. The socket is a
/// byte plane, not MSP-framed, so a single "frame" here is one length-prefixed
/// chunk the router forwarded, not necessarily one whole MSP message.
pub const MSP_BROADCAST_DEPTH: usize = 256;

/// A live connection to the MSP router socket. FC->host bytes fan out on a
/// broadcast channel (one receiver per `msp.subscribe`); host->FC commands
/// enqueue on a bounded mpsc a writer task drains. Best-effort send, matching the
/// MAVLink client.
pub struct MspClient {
    outbound: mpsc::Sender<Vec<u8>>,
    inbound: broadcast::Sender<Vec<u8>>,
    reader: JoinHandle<()>,
    writer: JoinHandle<()>,
}

impl MspClient {
    /// Connect to the MSP router socket, then spawn the reader that fans FC bytes
    /// out and the writer that drains the command queue.
    pub async fn connect(sock_path: impl AsRef<Path>) -> io::Result<Self> {
        let stream = connect_with_retry(sock_path, 50, Duration::from_millis(20)).await?;
        let (mut read_half, mut write_half) = stream.into_split();

        let (inbound, _rx) = broadcast::channel(MSP_BROADCAST_DEPTH);
        let tx = inbound.clone();
        let reader = tokio::spawn(async move {
            // Zero-length chunks are permitted (reject_zero = false); the router
            // caps at MAVLINK_MAX_FRAME (the shared transport chunk cap, not an
            // MSP-semantic limit). A clean EOF or a malformed header stops the loop.
            while let Ok(Some(frame)) =
                read_length_prefixed(&mut read_half, MAVLINK_MAX_FRAME, false).await
            {
                let _ = tx.send(frame);
            }
        });

        let (outbound, mut out_rx) = mpsc::channel::<Vec<u8>>(MSP_BROADCAST_DEPTH);
        let writer = tokio::spawn(async move {
            while let Some(frame) = out_rx.recv().await {
                if write_half.write_all(&frame).await.is_err() {
                    break;
                }
                if write_half.flush().await.is_err() {
                    break;
                }
            }
        });

        Ok(Self {
            outbound,
            inbound,
            reader,
            writer,
        })
    }

    /// Frame `data` (already-encoded MSP bytes) and enqueue it toward the FC.
    /// Best-effort: framing failures and a full queue are swallowed, matching the
    /// MAVLink client. Synchronous so the host trait method can call it directly.
    pub fn send_bytes(&self, data: &[u8]) {
        let frame = match encode_frame(data, MAVLINK_MAX_FRAME) {
            Ok(f) => f,
            Err(_) => return,
        };
        let _ = self.outbound.try_send(frame);
    }

    /// A fresh receiver for the FC->host byte fanout. Each `msp.subscribe` pump
    /// holds its own receiver; a slow pump lags rather than blocking the reader.
    pub fn subscribe(&self) -> broadcast::Receiver<Vec<u8>> {
        self.inbound.subscribe()
    }
}

impl Drop for MspClient {
    fn drop(&mut self) {
        self.reader.abort();
        self.writer.abort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ados_protocol::ipc::IpcBroadcast;

    fn temp_sock(name: &str) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "ados-mspclient-test-{}-{}.sock",
            std::process::id(),
            name
        ));
        let _ = std::fs::remove_file(&p);
        p
    }

    #[tokio::test]
    async fn send_reaches_the_socket_and_fc_bytes_fan_out() {
        let sock = temp_sock("roundtrip");
        // Router side: broadcast FC->host, receive host->FC on the inbound channel.
        let (server, inbound) = IpcBroadcast::bind(&sock, 64, false, Some(64))
            .await
            .unwrap();
        let mut inbound = inbound.expect("inbound requested");
        let client = MspClient::connect(&sock).await.unwrap();
        tokio::time::sleep(Duration::from_millis(30)).await;

        // Host->FC: send an MSP frame; the router receives the raw bytes.
        let frame = ados_protocol::msp::set_raw_rc(&[1500, 1500], true).unwrap();
        client.send_bytes(&frame);
        let got = tokio::time::timeout(Duration::from_secs(1), inbound.recv())
            .await
            .expect("no inbound")
            .expect("channel closed");
        assert_eq!(
            got.payload, frame,
            "the FC must receive the exact MSP bytes"
        );

        // FC->host: the router broadcasts a reply as a length-prefixed frame; the
        // client decodes it and the subscriber sees the raw MSP bytes.
        let mut rx = client.subscribe();
        tokio::time::sleep(Duration::from_millis(30)).await; // let the subscription register
        let reply = ados_protocol::msp::encode_v2(101, &[1, 2, 3]);
        server
            .broadcast(encode_frame(&reply, MAVLINK_MAX_FRAME).unwrap())
            .await;
        let delivered = tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("no delivery")
            .expect("channel closed");
        assert_eq!(
            delivered, reply,
            "the plugin must receive the exact FC bytes"
        );
    }
}
