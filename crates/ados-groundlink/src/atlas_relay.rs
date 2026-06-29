//! The ground-station Atlas relay: bridge the WFB aux lane onto the LAN.
//!
//! The drone radiates small Atlas events on the aux application stream (radio_id
//! 2); the GS `wfb_rx -p 2` decodes them to a loopback port. This relay reads
//! each decoded datagram (a self-delimiting framed [`AtlasEvent`]) and re-POSTs
//! it onto the LAN into the compute node's event router, so the field WFB lane
//! reaches the same receiver the direct-LAN bearer uses.
//!
//! A garbled or hostile frame off the air must never take the relay down: a
//! malformed datagram is dropped + counted and the loop continues. The
//! received-side counter is the delivery proof — the drone's UDP send into the
//! aux tunnel is fire-and-forget (Rule 37 / DEC-170), so only what this relay
//! actually decodes proves the RF lane carried it.

use std::sync::Arc;
use std::time::Duration;

use ados_atlas_transport::{AtlasBearer, AtlasEvent, LanHttpBearer};
use tokio::net::UdpSocket;
use tokio::sync::Notify;

const BUF_SIZE: usize = 65536;

/// Running counters for the relay, surfaced for observability.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct AtlasRelayStats {
    /// Datagrams read off the decoded aux port (received-side liveness proof).
    pub datagrams_seen: u64,
    /// Events decoded and accepted by the compute receiver.
    pub forwarded: u64,
    /// Datagrams that did not decode to an `AtlasEvent` (dropped).
    pub malformed: u64,
    /// Events that decoded but the forward POST failed.
    pub forward_failed: u64,
}

/// Decode one off-air datagram and forward it to the compute node. Never panics:
/// a non-decoding frame is counted as malformed and dropped.
async fn forward_datagram(bearer: &LanHttpBearer, buf: &[u8], stats: &mut AtlasRelayStats) {
    stats.datagrams_seen += 1;
    match AtlasEvent::from_msgpack(buf) {
        Ok(event) => match bearer.send(&event).await {
            Ok(()) => stats.forwarded += 1,
            Err(e) => {
                stats.forward_failed += 1;
                tracing::debug!(error = %e, topic = %event.topic, "atlas_relay_forward_failed");
            }
        },
        Err(_) => {
            stats.malformed += 1;
            tracing::debug!(bytes = buf.len(), "atlas_relay_malformed_datagram");
        }
    }
}

/// Run the relay until `cancel` is notified. Binds `127.0.0.1:listen_port` (where
/// `wfb_rx -p 2` decodes the aux stream) and forwards each datagram to
/// `compute_base_url` (the compute node's `atlas_event_router`). Returns the
/// final stats.
pub async fn run_atlas_relay(
    listen_port: u16,
    compute_base_url: String,
    cancel: Arc<Notify>,
) -> std::io::Result<AtlasRelayStats> {
    let in_sock = UdpSocket::bind(("127.0.0.1", listen_port)).await?;
    let bearer = LanHttpBearer::new(compute_base_url);
    let mut buf = vec![0u8; BUF_SIZE];
    let mut stats = AtlasRelayStats::default();
    // Consecutive recv errors with no intervening successful read. A transient
    // error (a downstream ICMP-unreachable bouncing back) self-clears; a run of
    // them with zero successes means the socket cannot be read at all, so a short
    // sleep keeps a hard failure from spinning the CPU (the fanout precedent).
    let mut consecutive_errors: u32 = 0;
    tracing::info!(listen_port, "atlas_relay_started");
    loop {
        tokio::select! {
            _ = cancel.notified() => break,
            r = in_sock.recv_from(&mut buf) => match r {
                Ok((n, _)) => {
                    consecutive_errors = 0;
                    forward_datagram(&bearer, &buf[..n], &mut stats).await;
                }
                Err(e) => {
                    consecutive_errors += 1;
                    tracing::warn!(error = %e, consecutive_errors, "atlas_relay_recv_error");
                    if consecutive_errors >= 8 {
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                }
            }
        }
    }
    tracing::info!(?stats, "atlas_relay_stopped");
    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ados_atlas_transport::atlas_event_router;
    use tokio::net::TcpListener;
    use tokio::sync::mpsc;

    /// Spawn the compute receiver on an ephemeral port; return its base URL + the
    /// channel events land on.
    async fn spawn_receiver() -> (String, mpsc::Receiver<AtlasEvent>) {
        let (tx, rx) = mpsc::channel(16);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, atlas_event_router(tx)).await;
        });
        (format!("http://{addr}"), rx)
    }

    fn event(topic: &str, payload: Vec<u8>) -> AtlasEvent {
        AtlasEvent {
            topic: topic.to_string(),
            payload,
        }
    }

    #[tokio::test]
    async fn a_decoded_datagram_is_forwarded_to_the_compute_receiver() {
        let (base, mut rx) = spawn_receiver().await;
        let bearer = LanHttpBearer::new(base);
        let mut stats = AtlasRelayStats::default();

        let ev = event("atlas.occupancy", vec![1, 2, 3]);
        forward_datagram(&bearer, &ev.to_msgpack().unwrap(), &mut stats).await;

        let got = rx.recv().await.unwrap();
        assert_eq!(got, ev);
        assert_eq!(stats.forwarded, 1);
        assert_eq!(stats.malformed, 0);
        assert_eq!(stats.datagrams_seen, 1);
    }

    #[tokio::test]
    async fn a_malformed_datagram_is_dropped_and_counted_not_forwarded() {
        let (base, mut rx) = spawn_receiver().await;
        let bearer = LanHttpBearer::new(base);
        let mut stats = AtlasRelayStats::default();

        forward_datagram(&bearer, b"not msgpack at all \xff\xff", &mut stats).await;

        assert_eq!(stats.malformed, 1);
        assert_eq!(stats.forwarded, 0);
        // Nothing reached the receiver.
        let got = tokio::time::timeout(std::time::Duration::from_millis(50), rx.recv()).await;
        assert!(got.is_err());
    }

    #[tokio::test]
    async fn the_relay_loop_forwards_a_datagram_then_stops_on_cancel() {
        let (base, mut rx) = spawn_receiver().await;
        let cancel = Arc::new(Notify::new());
        // Bind the relay on an ephemeral port by asking the OS, then reuse it.
        let probe = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let listen_port = probe.local_addr().unwrap().port();
        drop(probe);

        let cancel_run = cancel.clone();
        let handle = tokio::spawn(run_atlas_relay(listen_port, base, cancel_run));

        // Give the relay a moment to bind, then send a datagram into its port.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let tx = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        tx.connect(("127.0.0.1", listen_port)).await.unwrap();
        let ev = event("atlas.pose", vec![7]);
        tx.send(&ev.to_msgpack().unwrap()).await.unwrap();

        let got = rx.recv().await.unwrap();
        assert_eq!(got, ev);

        cancel.notify_one();
        let stats = handle.await.unwrap().unwrap();
        assert_eq!(stats.forwarded, 1);
    }
}
