//! Client to the MAVLink IPC socket the router serves.
//!
//! Ports the slice of the MAVLink router the plugin host needs: the
//! `MAVLinkRouter` Protocol in `src/ados/plugins/ipc/host_services.py`
//! (`send_bytes` + `subscribe` + `unsubscribe`) bound to the router's
//! `/run/ados/mavlink.sock`. The socket is bidirectional and length-prefixed
//! (4-byte big-endian length + raw frame): a frame written toward the socket is
//! a command toward the flight controller, and FC frames fan out on the same
//! connection. See `ados-mavlink-router/src/main.rs` for the serving side.
//!
//! On connect a reader task drains FC->plugin frames into a broadcast channel
//! (depth matching the router's `MAVLINK_QUEUE_DEPTH`). Each plugin subscription
//! pump gets its own [`broadcast::Receiver`] from [`MavlinkClient::subscribe`];
//! the writer half is held behind an async mutex so `send_bytes` from any task
//! is serialized. Both sides reuse the `ados-protocol` framing primitives; no
//! wire is re-implemented here.

use std::io;
use std::path::Path;
use std::time::Duration;

use ados_protocol::frame::{encode_frame, MAVLINK_MAX_FRAME};
use ados_protocol::ipc::{connect_with_retry, read_length_prefixed};
use tokio::io::AsyncWriteExt;
use tokio::sync::{broadcast, mpsc};
use tokio::task::JoinHandle;

/// Inbound FC-frame fanout depth. Matches the router's `MAVLINK_QUEUE_DEPTH`
/// so a plugin pump that briefly stalls lags rather than wedges the reader.
pub const MAVLINK_BROADCAST_DEPTH: usize = 256;

/// A live connection to the MAVLink router socket.
///
/// FC->plugin frames fan out on a broadcast channel (one receiver per plugin
/// subscription). Plugin->FC commands enqueue on a bounded mpsc that a writer
/// task drains to the socket, so [`send_bytes`](Self::send_bytes) is a plain
/// non-async call usable from the synchronous host trait method (the Python
/// `MAVLinkRouter.send_bytes` is sync too). The send is best-effort: a full
/// queue or a write error is swallowed, matching the Python slice whose
/// `send_bytes` does not surface a failure to the plugin handler.
///
/// Frames are forwarded raw, with no per-message-name byte filtering (the Python
/// pump forwards every queued frame to every active subscriber), matching
/// `src/ados/plugins/ipc/mavlink_pump.py`.
pub struct MavlinkClient {
    /// Plugin->FC command queue; the writer task drains it.
    outbound: mpsc::Sender<Vec<u8>>,
    /// FC->plugin frame fanout.
    inbound: broadcast::Sender<Vec<u8>>,
    reader: JoinHandle<()>,
    writer: JoinHandle<()>,
}

impl MavlinkClient {
    /// Connect to the router socket, then spawn the reader that fans FC frames
    /// out and the writer that drains the command queue. Mirrors the Python
    /// connection setup: bounded connect-with-retry, then a read loop draining
    /// length-prefixed frames plus a send path toward the FC.
    pub async fn connect(sock_path: impl AsRef<Path>) -> io::Result<Self> {
        let stream = connect_with_retry(sock_path, 50, Duration::from_millis(20)).await?;
        let (mut read_half, mut write_half) = stream.into_split();

        let (inbound, _rx) = broadcast::channel(MAVLINK_BROADCAST_DEPTH);
        let tx = inbound.clone();
        let reader = tokio::spawn(async move {
            // The MAVLink contract permits zero-length frames (reject_zero =
            // false); the router caps at MAVLINK_MAX_FRAME. A clean EOF or a
            // malformed/oversized header (Ok(None) / Err) stops the loop; the
            // socket is gone or the peer misbehaved, and existing receivers see
            // the channel close on their next recv.
            while let Ok(Some(frame)) =
                read_length_prefixed(&mut read_half, MAVLINK_MAX_FRAME, false).await
            {
                // A send with no receivers returns Err; that is fine, the next
                // subscriber resumes at the tail.
                let _ = tx.send(frame);
            }
        });

        let (outbound, mut out_rx) = mpsc::channel::<Vec<u8>>(MAVLINK_BROADCAST_DEPTH);
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

    /// Frame `data` and enqueue it toward the flight controller. Best-effort:
    /// framing failures and a full queue are swallowed, matching the Python
    /// router slice whose `send_bytes` does not surface a failure to the plugin
    /// handler (the handler logs and returns `send_failed` only if the call
    /// raises, which the best-effort path never does once the bytes are
    /// validated). Synchronous so the host trait method can call it directly.
    pub fn send_bytes(&self, data: &[u8]) {
        let frame = match encode_frame(data, MAVLINK_MAX_FRAME) {
            Ok(f) => f,
            Err(_) => return,
        };
        let _ = self.outbound.try_send(frame);
    }

    /// Tell the router that everything this connection writes is being
    /// forwarded from off the node rather than produced on it.
    ///
    /// For a caller that is a bridge — the cloud relay is the one that exists —
    /// this is the only way the router can tell its traffic apart from a local
    /// producer's, because the on-box socket's own reasoning ("not reachable off
    /// the node") describes the socket and not the bytes crossing it. The
    /// declaration latches for the life of the connection and only ever removes
    /// benefit of the doubt, so sending it is never the wrong call for a bridge.
    ///
    /// Send it before any traffic: the writer task drains its queue in order, so
    /// a declaration enqueued first is read first.
    pub fn declare_off_box_source(&self) {
        self.send_bytes(ados_protocol::ipc::IPC_DECLARE_OFF_BOX_SOURCE);
    }

    /// Declare this connection an AUTONOMOUS INJECTOR, subjecting everything it
    /// writes toward the FC to the PIC arbiter: an operator holding the
    /// manual-control claim refuses these commands, and a not-reporting arbiter
    /// fails closed. One-way and downward like [`Self::declare_off_box_source`] —
    /// it can only ever remove authority, never grant it, so an UNDECLARED
    /// connection stays the never-gated operator path (the load-bearing
    /// invariant). Sent only when injector arbitration is armed; the host is inert
    /// by default.
    ///
    /// `ticket` attests `client_id` against the pairing key (mint it with
    /// `ados_protocol::ws_ticket::mint_scoped_ticket` + `crsf_inject_scope`);
    /// `None` on an unpaired node, where the router's verifier accepts the
    /// asserted id, matching every other native surface while unclaimed. Send it
    /// before any traffic — the writer drains its queue in order.
    pub fn declare_injector(&self, client_id: &str, ticket: Option<&str>) {
        self.send_bytes(&ados_protocol::ipc::encode_injector_declaration(
            client_id, ticket,
        ));
    }

    /// A fresh receiver for the FC->plugin frame fanout. Each plugin pump holds
    /// its own receiver; a slow pump lags rather than blocking the reader.
    pub fn subscribe(&self) -> broadcast::Receiver<Vec<u8>> {
        self.inbound.subscribe()
    }
}

impl Drop for MavlinkClient {
    fn drop(&mut self) {
        // Stop both tasks so neither survives the client.
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
            "ados-mavclient-test-{}-{}.sock",
            std::process::id(),
            name
        ));
        let _ = std::fs::remove_file(&p);
        p
    }

    #[tokio::test]
    async fn fc_frames_fan_out_to_a_subscriber() {
        let path = temp_sock("fanout");
        // The router side: a bidirectional, 256-deep broadcast with an inbound
        // command channel, exactly as ados-mavlink-router binds it.
        let (server, _inbound) =
            IpcBroadcast::bind(&path, MAVLINK_BROADCAST_DEPTH, false, Some(256))
                .await
                .unwrap();

        let client = MavlinkClient::connect(&path).await.unwrap();
        let mut rx = client.subscribe();
        // Let the server register the client connection before broadcasting.
        tokio::time::sleep(Duration::from_millis(50)).await;

        let payload = b"\xfd\x09\x00\x00\x00\x01\x01\x00\x00\x00rest";
        server
            .broadcast(encode_frame(payload, MAVLINK_MAX_FRAME).unwrap())
            .await;

        let got = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("frame within timeout")
            .expect("frame, not lagged/closed");
        assert_eq!(got, payload);
    }

    #[tokio::test]
    async fn send_bytes_reaches_the_router_inbound() {
        let path = temp_sock("inbound");
        let (_server, inbound) =
            IpcBroadcast::bind(&path, MAVLINK_BROADCAST_DEPTH, false, Some(16))
                .await
                .unwrap();
        let mut inbound = inbound.expect("inbound channel requested");

        let client = MavlinkClient::connect(&path).await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;

        client.send_bytes(b"command-toward-fc");

        let got = tokio::time::timeout(Duration::from_millis(500), inbound.recv())
            .await
            .unwrap();
        assert_eq!(
            got.map(|c| c.payload).as_deref(),
            Some(&b"command-toward-fc"[..])
        );
    }

    /// A bridge declares itself once and every command it forwards afterwards
    /// carries that reading. Without it the router saw the same thing it sees
    /// from a local producer, which is how broker traffic came to be recorded
    /// as on-box.
    #[tokio::test]
    async fn a_declared_bridge_marks_every_command_it_forwards() {
        let path = temp_sock("declare");
        let (_server, inbound) =
            IpcBroadcast::bind(&path, MAVLINK_BROADCAST_DEPTH, false, Some(16))
                .await
                .unwrap();
        let mut inbound = inbound.expect("inbound channel requested");

        let client = MavlinkClient::connect(&path).await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;

        client.declare_off_box_source();
        client.send_bytes(b"forwarded-from-the-broker");

        let got = tokio::time::timeout(Duration::from_millis(500), inbound.recv())
            .await
            .unwrap()
            .expect("a command");
        assert_eq!(got.payload, b"forwarded-from-the-broker");
        assert!(
            got.peer.off_box_source,
            "a declared bridge's traffic must not read as locally produced"
        );
    }

    /// A declared injector's every command carries the claim the router's PIC
    /// gate consumes. Arming is exactly this: without the declaration the router
    /// sees an operator/human writer and never gates it.
    #[tokio::test]
    async fn a_declared_injector_marks_every_command_with_its_claim() {
        let path = temp_sock("declare-injector");
        let (_server, inbound) =
            IpcBroadcast::bind(&path, MAVLINK_BROADCAST_DEPTH, false, Some(16))
                .await
                .unwrap();
        let mut inbound = inbound.expect("inbound channel requested");

        let client = MavlinkClient::connect(&path).await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;

        client.declare_injector("plugin-host", Some("a-ticket"));
        client.send_bytes(b"guided-setpoint");

        let got = tokio::time::timeout(Duration::from_millis(500), inbound.recv())
            .await
            .unwrap()
            .expect("a command");
        assert_eq!(got.payload, b"guided-setpoint");
        assert_eq!(
            got.peer.injector,
            Some(ados_protocol::ipc::InjectorClaim {
                client_id: "plugin-host".into(),
                ticket: Some("a-ticket".into()),
            }),
            "a declared injector's traffic must carry the claim the PIC gate reads"
        );
    }

    #[tokio::test]
    async fn send_bytes_swallows_an_oversized_frame() {
        let path = temp_sock("oversize");
        let (_server, _inbound) =
            IpcBroadcast::bind(&path, MAVLINK_BROADCAST_DEPTH, false, Some(16))
                .await
                .unwrap();
        let client = MavlinkClient::connect(&path).await.unwrap();
        // Larger than MAVLINK_MAX_FRAME: encode_frame errors, send_bytes is a
        // no-op rather than panicking, matching the best-effort Python slice.
        let oversize = vec![0u8; MAVLINK_MAX_FRAME + 1];
        client.send_bytes(&oversize);
    }
}
