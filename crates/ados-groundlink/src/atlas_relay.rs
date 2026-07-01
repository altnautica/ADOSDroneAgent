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
//! aux tunnel is fire-and-forget, so only what this relay
//! actually decodes proves the RF lane carried it.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use ados_atlas_transport::{AtlasBearer, AtlasEvent, LanHttpBearer};
use tokio::net::UdpSocket;
use tokio::sync::Notify;

const BUF_SIZE: usize = 65536;

/// Cadence the relay republishes its live counters at, so the surface stays
/// current even while the aux lane is idle (no datagrams to drive a write).
const SIDECAR_PUBLISH_INTERVAL: Duration = Duration::from_secs(2);

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

/// The relay's published live state, written to `/run/ados/atlas-relay.json`
/// (Contract-E) and shipped to the logging store as a `gs.atlas_relay` event.
///
/// A LAN-paired GCS reads this exact shape to render the GS Atlas relay card
/// local-first (the durable store first, this sidecar as the fallback). The field
/// set is the wire contract the GCS consumes; keep it stable. `up` reflects
/// whether the relay loop is actually running so the surface never reports a
/// stale "running" reading after the loop stops.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct AtlasRelaySidecar {
    /// True while the relay loop is running; false on the teardown write.
    pub up: bool,
    /// Datagrams read off the decoded aux port (received-side liveness proof).
    pub datagrams_seen: u64,
    /// Events decoded and accepted by the compute receiver.
    pub forwarded: u64,
    /// Datagrams that did not decode to an Atlas event (dropped).
    pub malformed: u64,
    /// Events that decoded but the forward POST to the compute node failed.
    pub forward_failed: u64,
    /// The compute node base URL the relay forwards decoded events to.
    pub compute_url: String,
    /// The loopback aux port the decoded datagrams are read from.
    pub listen_port: u16,
    /// Epoch-millis the snapshot was generated.
    pub generated_at_ms: i64,
}

impl AtlasRelaySidecar {
    /// Snapshot the live counters into the publishable shape. `up` reflects
    /// whether the relay loop is still running, so the reading stays truthful.
    fn snapshot(stats: &AtlasRelayStats, compute_url: &str, listen_port: u16, up: bool) -> Self {
        Self {
            up,
            datagrams_seen: stats.datagrams_seen,
            forwarded: stats.forwarded,
            malformed: stats.malformed,
            forward_failed: stats.forward_failed,
            compute_url: compute_url.to_string(),
            listen_port,
            generated_at_ms: crate::mesh_events::now_ms(),
        }
    }

    /// Atomically write the snapshot to the Contract-E sidecar AND ship the same
    /// body to the logging store as a single `gs.atlas_relay` event. Best-effort:
    /// a failed file write is logged and dropped, and an absent logging daemon
    /// drops the event without disturbing the relay loop. Honours `ADOS_RUN_DIR`
    /// via [`crate::paths::run_path`].
    fn write_and_emit(&self, ingest: Option<&ados_protocol::logd::emitter::IngestEmitter>) {
        let path = crate::paths::run_path("atlas-relay.json");
        if let Err(e) = crate::sidecars::write_json_atomic(Path::new(&path), self, 0o644) {
            tracing::debug!(error = %e, "atlas_relay_sidecar_write_failed");
        }
        if let Some(em) = ingest {
            let v = serde_json::to_value(self).unwrap_or(serde_json::Value::Null);
            em.emit_event(
                "gs.atlas_relay",
                ados_protocol::logd::Level::Info,
                crate::wfb_rx::stats::json_object_to_fields(&v),
            );
        }
    }
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
/// `compute_base_url` (the compute node's `atlas_event_router`). Publishes the
/// live counters to the `atlas-relay.json` Contract-E sidecar and the logging
/// store on a fixed cadence and after each forward, so the GS Atlas relay card
/// reads a current, truthful view; a final `up=false` snapshot is persisted on
/// teardown. Returns the final stats.
pub async fn run_atlas_relay(
    listen_port: u16,
    compute_base_url: String,
    cancel: Arc<Notify>,
    ingest: Option<ados_protocol::logd::emitter::IngestEmitter>,
) -> std::io::Result<AtlasRelayStats> {
    let in_sock = UdpSocket::bind(("127.0.0.1", listen_port)).await?;
    let compute_url = compute_base_url.clone();
    let bearer = LanHttpBearer::new(compute_base_url);
    let mut buf = vec![0u8; BUF_SIZE];
    let mut stats = AtlasRelayStats::default();
    // Consecutive recv errors with no intervening successful read. A transient
    // error (a downstream ICMP-unreachable bouncing back) self-clears; a run of
    // them with zero successes means the socket cannot be read at all, so a short
    // sleep keeps a hard failure from spinning the CPU (the fanout precedent).
    let mut consecutive_errors: u32 = 0;
    // Periodic publish so the card stays live on an idle aux lane. The first tick
    // fires immediately, so the surface reads "running" the moment the loop binds.
    let mut publish = tokio::time::interval(SIDECAR_PUBLISH_INTERVAL);
    tracing::info!(listen_port, "atlas_relay_started");
    loop {
        tokio::select! {
            _ = cancel.notified() => break,
            _ = publish.tick() => {
                AtlasRelaySidecar::snapshot(&stats, &compute_url, listen_port, true)
                    .write_and_emit(ingest.as_ref());
            }
            r = in_sock.recv_from(&mut buf) => match r {
                Ok((n, _)) => {
                    consecutive_errors = 0;
                    forward_datagram(&bearer, &buf[..n], &mut stats).await;
                    AtlasRelaySidecar::snapshot(&stats, &compute_url, listen_port, true)
                        .write_and_emit(ingest.as_ref());
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
    // The loop is stopping; persist a final `up=false` snapshot so the surface
    // reflects reality instead of a stale "running" reading.
    AtlasRelaySidecar::snapshot(&stats, &compute_url, listen_port, false)
        .write_and_emit(ingest.as_ref());
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
            device_id: None,
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
        let handle = tokio::spawn(run_atlas_relay(listen_port, base, cancel_run, None));

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

    #[test]
    fn sidecar_snapshot_maps_the_counters_and_carries_the_wire_contract_keys() {
        let stats = AtlasRelayStats {
            datagrams_seen: 7,
            forwarded: 5,
            malformed: 1,
            forward_failed: 1,
        };
        let s = AtlasRelaySidecar::snapshot(&stats, "http://compute-node.local:8092", 5603, true);
        assert!(s.up);
        assert_eq!(s.datagrams_seen, 7);
        assert_eq!(s.forwarded, 5);
        assert_eq!(s.malformed, 1);
        assert_eq!(s.forward_failed, 1);
        assert_eq!(s.compute_url, "http://compute-node.local:8092");
        assert_eq!(s.listen_port, 5603);

        // The published JSON carries exactly the keys the GCS half consumes;
        // pin the set so a rename of the wire contract is caught at test time.
        let v = serde_json::to_value(&s).unwrap();
        let mut keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "compute_url",
                "datagrams_seen",
                "forward_failed",
                "forwarded",
                "generated_at_ms",
                "listen_port",
                "malformed",
                "up",
            ]
        );
    }

    #[tokio::test]
    async fn write_and_emit_enqueues_one_gs_atlas_relay_event_with_an_emitter_and_none_without() {
        // The emitting write ships exactly one gs.atlas_relay event when an
        // emitter is supplied and nothing with None, regardless of whether the
        // best-effort file write succeeds. The emitter records every enqueue
        // independent of a listening daemon. This avoids the process-wide
        // ADOS_RUN_DIR mutation so it never races the run-dir tests.
        let dir = tempfile::tempdir().unwrap();
        let stats = AtlasRelayStats {
            datagrams_seen: 3,
            forwarded: 2,
            malformed: 1,
            forward_failed: 0,
        };
        let s = AtlasRelaySidecar::snapshot(&stats, "http://compute-node.local:8092", 5603, true);

        let emitter = ados_protocol::logd::emitter::IngestEmitter::with_socket(
            "ados-groundlink",
            dir.path().join("ingest.sock"),
        );
        let em_stats = emitter.stats();
        s.write_and_emit(Some(&emitter));
        assert_eq!(em_stats.enqueued(), 1);

        let none_emitter = ados_protocol::logd::emitter::IngestEmitter::with_socket(
            "ados-groundlink",
            dir.path().join("ingest2.sock"),
        );
        let none_stats = none_emitter.stats();
        s.write_and_emit(None);
        assert_eq!(none_stats.enqueued(), 0);
    }
}
