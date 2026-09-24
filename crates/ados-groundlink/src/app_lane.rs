//! The plugin application lane on a ground station: `radio-aux.sock`.
//!
//! A drone's radio service serves the auxiliary-stream command socket
//! ([`ados_radio::aux_cmd`]) that the plugin host's `radio.aux_stream.*` methods
//! forward to. This serves the same socket, with the same protocol
//! implementation, from the ground station's receive plane, so a plugin half
//! runs unchanged on either end of the link:
//!
//! - inbound, every `AppStream` datagram the per-slot aux consumers decode is
//!   delivered in-process through the [`AppStreamSink`] below into the socket's
//!   subscriber fan-out. The consumers already own the decoded aux ports, so
//!   this adds a destination on the existing demultiplexer, never a second bind;
//! - outbound, `send` goes through an egress connected to the ground uplink
//!   transmit ingress (the receive chain's aux `wfb_tx`), limited to the two
//!   application channels.

use std::path::{Path, PathBuf};

use ados_protocol::aux_mux::AuxChannel;
use ados_protocol::shutdown::Shutdown;
use ados_radio::aux_cmd::{self, AuxCmdState};

use crate::aux_consumer::AppStreamSink;

/// Broadcast depth of the subscriber fan-out; the drone's radio service uses
/// the same. Application frames are lossy-tolerant, so a subscriber this far
/// behind drops oldest first.
pub const APP_FANOUT_DEPTH: usize = 256;

/// The socket path, resolved exactly as the drone's radio service resolves it
/// (the run dir, honouring `ADOS_RUN_DIR`), so the plugin host finds it at the
/// same place on either profile.
pub fn socket_path() -> PathBuf {
    PathBuf::from(ados_radio::paths::run_path("radio-aux.sock"))
}

/// Delivers a decoded `AppStream` payload to every attached subscriber. The
/// publish reply is not needed here: a refusal means the operator disabled the
/// lane, and zero subscribers is the normal state of a node with no plugin on
/// the lane.
impl AppStreamSink for AuxCmdState {
    fn deliver(&self, payload: &[u8]) {
        let _ = self.publish(AuxChannel::AppStream as u8, payload.to_vec());
    }
}

/// Serve the socket until `shutdown` fires or the listener fails to bind.
pub async fn serve(state: AuxCmdState, sock_path: &Path, shutdown: Shutdown) {
    tokio::select! {
        r = aux_cmd::serve(state, sock_path) => {
            if let Err(e) = r {
                tracing::warn!(path = %sock_path.display(), error = %e, "ground_app_lane_serve_ended");
            }
        }
        _ = shutdown.wait() => {}
    }
}

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;
    use std::sync::Arc;
    use std::time::Duration;

    use ados_protocol::aux_mux;
    use ados_protocol::mavlink_ingest::MavlinkIngest;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::{UdpSocket, UnixStream};

    use super::*;
    use crate::aux_consumer::{run_aux_consumer, AuxCounters, AuxSinksOwned};
    use crate::aux_peers::AuxPeerCache;

    /// A loopback port nothing is bound to, for the consumer to take.
    async fn free_port() -> u16 {
        let probe = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        probe.local_addr().unwrap().port()
    }

    /// One request, one newline-terminated reply, as the plugin host sends it.
    async fn one_shot(path: &Path, request: &str) -> serde_json::Value {
        let mut stream = UnixStream::connect(path).await.unwrap();
        stream
            .write_all(format!("{request}\n").as_bytes())
            .await
            .unwrap();
        let mut line = String::new();
        BufReader::new(stream).read_line(&mut line).await.unwrap();
        serde_json::from_str(&line).unwrap()
    }

    #[tokio::test]
    async fn the_socket_carries_app_traffic_both_ways_over_the_ground_lane() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("radio-aux.sock");
        // Stand-in for the receive chain's aux uplink `wfb_tx` ingress.
        let uplink = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let (fanout, _) = tokio::sync::broadcast::channel(APP_FANOUT_DEPTH);
        let state = AuxCmdState::ground(uplink.local_addr().unwrap().port(), true, fanout)
            .await
            .unwrap();
        let shutdown = Shutdown::new();
        let server = tokio::spawn({
            let (state, sock, shutdown) = (state.clone(), sock.clone(), shutdown.clone());
            async move { serve(state, &sock, shutdown).await }
        });

        // The real decode path: an aux consumer on a drone slot's port, with
        // this state registered as its application-stream sink.
        let rx_port = free_port().await;
        let consumer = tokio::spawn(run_aux_consumer(
            1,
            rx_port,
            AuxSinksOwned {
                mavlink: Arc::new(MavlinkIngest::new(dir.path().join("unused.sock"))),
                rpc_response: None,
                config_tunnel: None,
                app_stream: Some(Arc::new(state)),
                local_plugins: None,
            },
            AuxCounters::new(),
            AuxPeerCache::new(),
            shutdown.clone(),
        ));
        tokio::time::sleep(Duration::from_millis(50)).await;

        // A plugin subscribes exactly as the host's aux reader does.
        let mut sub = UnixStream::connect(&sock).await.unwrap();
        sub.write_all(b"{\"op\":\"subscribe\"}\n").await.unwrap();
        let mut sub = BufReader::new(sub).lines();
        assert_eq!(sub.next_line().await.unwrap().unwrap(), r#"{"ok":true}"#);

        // The drone radiates an AppStream datagram; it decodes on the slot port.
        let air = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let datagram = aux_mux::encode(AuxChannel::AppStream, b"world").unwrap();
        air.send_to(&datagram, (Ipv4Addr::LOCALHOST, rx_port))
            .await
            .unwrap();
        let line = tokio::time::timeout(Duration::from_secs(2), sub.next_line())
            .await
            .expect("the subscriber must receive the payload")
            .unwrap()
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["channel"], AuxChannel::AppStream as u8);
        assert_eq!(v["payload"], serde_json::json!(b"world".to_vec()));

        // Outbound: open, then an AppCommand frame reaches the uplink ingress.
        assert_eq!(one_shot(&sock, r#"{"op":"open"}"#).await["ok"], true);
        let frame = aux_mux::encode(AuxChannel::AppCommand, b"go").unwrap();
        let request = serde_json::json!({"op": "send", "frame": frame}).to_string();
        assert_eq!(
            one_shot(&sock, &request).await,
            serde_json::json!({"ok": true})
        );
        let mut buf = [0u8; 64];
        let (n, _) = tokio::time::timeout(Duration::from_secs(2), uplink.recv_from(&mut buf))
            .await
            .expect("the frame must reach the uplink")
            .unwrap();
        assert_eq!(&buf[..n], frame.as_slice());

        shutdown.trigger();
        server.await.unwrap();
        consumer.await.unwrap().unwrap();
    }
}
