//! MAVLink frame relay over MQTT.
//!
//! Bridges raw MAVLink frames between the local MAVLink IPC socket and the
//! broker:
//! * FC->GCS: frames read from `/run/ados/mavlink.sock` are published to
//!   `ados/{id}/mavlink/tx` at q0.
//! * GCS->FC: payloads received on `ados/{id}/mavlink/rx` (q0) are written back
//!   to the IPC socket toward the flight controller.
//!
//! The hot path's design is load-bearing. A synchronous per-frame publish
//! that blocks when the broker/tunnel is slow would stall the IPC reader, push
//! back through the kernel TCP buffer, stop the serial FC read, overrun the FC
//! transmit buffer, and freeze telemetry. The fix is a bounded queue with a
//! drop-oldest policy (recency beats completeness) draining to a separate
//! publisher, plus a high in-flight ceiling so the MQTT publish path — not the
//! client's internal queue — is the limit. Both bounds are unit-tested below
//! ([`BoundedPublishQueue`]); they are the parity crown jewel.

use std::collections::VecDeque;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use ados_plugin_host::mavlink_client::MavlinkClient;
use tokio::sync::{mpsc, watch};

use super::transport::RumqttcTransport;
use super::{relay_username, topic_mavlink_rx, topic_mavlink_tx};
use crate::mqtt::transport::TransportConfig;

/// Frame queue depth. 2000 frames at ~30 msg/s gives ~66 s of headroom before
/// drops start. The earlier 200-frame size produced a 17.4 % sustained drop
/// rate over a slow WSS tunnel. Mirrors `_QUEUE_MAXSIZE`.
pub const QUEUE_MAXSIZE: usize = 2000;

/// In-flight publish ceiling. The MQTT client's low default in-flight limit was
/// the actual bottleneck at ~30 msg/s over a ~50-150 ms RTT tunnel; the
/// publisher must not have more than this many publishes outstanding so the
/// publish path itself is the limit, not the client's internal queue.
pub const INFLIGHT_LIMIT: usize = 1000;

/// Throughput counters logged periodically and surfaced for diagnostics.
/// Field names mirror the Python `_metrics` dict.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct RelayMetrics {
    pub frames_in: u64,
    pub frames_published: u64,
    pub frames_dropped_queue_full: u64,
    pub frames_dropped_not_connected: u64,
    pub publish_errors: u64,
    pub ipc_send_errors: u64,
}

/// The bounded publish queue + in-flight gate, factored out of the relay so the
/// two load-bearing bounds are unit-testable with no MQTT and no IPC.
///
/// * [`push`](Self::push): enqueue a frame; on a full queue (`QUEUE_MAXSIZE`)
///   drop the OLDEST frame to make room (recency policy), counting the drop.
/// * [`try_take`](Self::try_take): hand the publisher the next frame ONLY when
///   the in-flight count is below `INFLIGHT_LIMIT`; otherwise withhold it so the
///   publisher applies backpressure rather than overrunning the client queue.
/// * [`on_publish_started`](Self::on_publish_started) /
///   [`on_publish_acked`](Self::on_publish_acked): the publisher brackets each
///   send so the in-flight count tracks outstanding publishes.
#[derive(Debug)]
pub struct BoundedPublishQueue {
    queue: VecDeque<Vec<u8>>,
    capacity: usize,
    inflight: usize,
    inflight_limit: usize,
    dropped_oldest: u64,
}

impl BoundedPublishQueue {
    /// A queue with the production bounds (`QUEUE_MAXSIZE` / `INFLIGHT_LIMIT`).
    pub fn new() -> Self {
        Self::with_bounds(QUEUE_MAXSIZE, INFLIGHT_LIMIT)
    }

    /// A queue with explicit bounds (tests use small bounds to exercise the
    /// edges without enqueuing thousands of frames).
    pub fn with_bounds(capacity: usize, inflight_limit: usize) -> Self {
        BoundedPublishQueue {
            queue: VecDeque::new(),
            capacity,
            inflight: 0,
            inflight_limit,
            dropped_oldest: 0,
        }
    }

    /// Enqueue a frame. On a full queue, drop the oldest frame first (recency
    /// beats completeness), counting the drop. Returns `true` when an oldest
    /// frame was dropped to make room.
    pub fn push(&mut self, frame: Vec<u8>) -> bool {
        let mut dropped = false;
        if self.queue.len() >= self.capacity {
            // Drop the oldest to make room for the newest.
            self.queue.pop_front();
            self.dropped_oldest += 1;
            dropped = true;
        }
        self.queue.push_back(frame);
        dropped
    }

    /// Take the next frame to publish, but only when below the in-flight
    /// ceiling. Returns `None` when the queue is empty OR the in-flight count is
    /// at the limit (the publisher should wait for an ack before sending more).
    pub fn try_take(&mut self) -> Option<Vec<u8>> {
        if self.inflight >= self.inflight_limit {
            return None;
        }
        self.queue.pop_front()
    }

    /// Mark a publish as started (in-flight). Called by the publisher right
    /// after a `try_take` it is about to send.
    pub fn on_publish_started(&mut self) {
        self.inflight += 1;
    }

    /// Mark a publish as acked/completed (no longer in-flight). For q0 this is
    /// the moment the send returns; for a tracked publish it is the broker ack.
    pub fn on_publish_acked(&mut self) {
        self.inflight = self.inflight.saturating_sub(1);
    }

    /// Current queued (not-yet-taken) frame count.
    pub fn len(&self) -> usize {
        self.queue.len()
    }

    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }

    /// Current in-flight (taken, not-yet-acked) count.
    pub fn inflight(&self) -> usize {
        self.inflight
    }

    /// Total frames dropped-oldest over the life of the queue.
    pub fn dropped_oldest(&self) -> u64 {
        self.dropped_oldest
    }
}

impl Default for BoundedPublishQueue {
    fn default() -> Self {
        Self::new()
    }
}

/// The MAVLink-over-MQTT relay. Owns its own rumqttc client (not the shared
/// transport trait) so the bounded-queue + in-flight gate govern the hot
/// publish path directly.
pub struct MavlinkMqttRelay {
    device_id: String,
    topic_tx: String,
    topic_rx: String,
    transport_config: TransportConfig,
}

impl MavlinkMqttRelay {
    /// Build the relay for a device id + broker dial config. The transport
    /// config's username is the `ados-{id}` relay form and its inflight ceiling
    /// is `INFLIGHT_LIMIT`; callers wire the broker host/port/password.
    pub fn new(device_id: impl Into<String>, transport_config: TransportConfig) -> Self {
        let device_id = device_id.into();
        MavlinkMqttRelay {
            topic_tx: topic_mavlink_tx(&device_id),
            topic_rx: topic_mavlink_rx(&device_id),
            transport_config,
            device_id,
        }
    }

    /// The relay's MQTT username (`ados-{device_id}`), exposed for the dial
    /// config the caller assembles.
    pub fn username(&self) -> String {
        relay_username(&self.device_id)
    }

    /// Run the relay until `shutdown` fires. Connects the broker transport +
    /// the IPC client, subscribes `mavlink/rx` (q0), forwards received payloads
    /// to the FC, and drains the bounded queue of FC frames to `mavlink/tx`
    /// (q0) under the in-flight gate.
    pub async fn run(
        &self,
        ipc_sock: impl AsRef<std::path::Path>,
        shutdown: tokio::sync::watch::Receiver<bool>,
    ) -> anyhow::Result<()> {
        self.run_observed(ipc_sock, shutdown, None).await
    }

    /// Run the relay, publishing the transport's CONFIRMED broker-connection
    /// flag to `connected_out` once the transport is dialed. A supervisor reads
    /// that flag to report whether the broker session is actually up — the relay
    /// task staying alive is NOT proof of a connection (rumqttc dials lazily and
    /// retries a down broker forever), so a supervisor that wants a truthful
    /// `mqttConnected` must observe this flag, not the task handle.
    pub async fn run_observed(
        &self,
        ipc_sock: impl AsRef<std::path::Path>,
        shutdown: tokio::sync::watch::Receiver<bool>,
        connected_out: Option<&watch::Sender<Option<Arc<AtomicBool>>>>,
    ) -> anyhow::Result<()> {
        let transport = RumqttcTransport::connect(&self.transport_config);
        // Hand the supervisor the live connection flag (set on ConnAck, cleared
        // on Disconnect/error). Until ConnAck the flag reads false, so the
        // supervisor never reports a connection the broker has not granted.
        if let Some(sink) = connected_out {
            let _ = sink.send(Some(transport.connected_handle()));
        }
        let mut incoming = transport
            .take_incoming()
            .await
            .ok_or_else(|| anyhow::anyhow!("transport incoming channel already taken"))?;

        // Connect the IPC client (FC frames in, commands out). Best-effort: a
        // missing socket is logged and the relay exits so systemd restarts it,
        // matching the Python relay's behavior.
        let ipc = match MavlinkClient::connect(ipc_sock).await {
            Ok(c) => std::sync::Arc::new(c),
            Err(e) => {
                tracing::warn!(error = %e, "mavlink relay: ipc unavailable");
                return Ok(());
            }
        };

        // GCS->FC: subscribe rx and write received payloads to the IPC socket.
        if let Err(e) = transport
            .client()
            .subscribe(self.topic_rx.clone(), rumqttc::QoS::AtMostOnce)
            .await
        {
            tracing::warn!(error = %e, "mavlink relay: rx subscribe failed");
        }

        // FC->GCS: a BOUNDED channel between the IPC frame stream and the
        // publisher. If the publisher stalls (a wedged broker / a teardown that
        // has not yet aborted), an unbounded channel here would buffer FC frames
        // without limit and OOM the process — the downstream BoundedPublishQueue
        // only caps what the publisher has already pulled. The reader drops the
        // NEWEST frame when the channel is full (recency is preserved by the
        // drop-oldest BoundedPublishQueue the publisher drains into).
        let (frame_tx, mut frame_rx) = mpsc::channel::<Vec<u8>>(QUEUE_MAXSIZE);
        let mut fc_frames = ipc.subscribe();
        let reader = tokio::spawn(async move {
            loop {
                match fc_frames.recv().await {
                    Ok(frame) => match frame_tx.try_send(frame) {
                        Ok(()) => {}
                        Err(mpsc::error::TrySendError::Full(_)) => {} // drop newest
                        Err(mpsc::error::TrySendError::Closed(_)) => break,
                    },
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        });

        let mut queue = BoundedPublishQueue::new();
        let mut metrics = RelayMetrics::default();
        let mut shutdown = shutdown;
        let client = transport.client().clone();

        loop {
            tokio::select! {
                _ = shutdown.changed() => {
                    if *shutdown.borrow() {
                        break;
                    }
                }
                // FC frame in: enqueue (drop-oldest on full).
                frame = frame_rx.recv() => {
                    match frame {
                        Some(f) => {
                            metrics.frames_in += 1;
                            if queue.push(f) {
                                metrics.frames_dropped_queue_full += 1;
                            }
                        }
                        None => break,
                    }
                }
                // GCS->FC command in: write to the IPC socket toward the FC.
                msg = incoming.recv() => {
                    match msg {
                        Some(m) if m.topic == self.topic_rx && !m.payload.is_empty() => {
                            ipc.send_bytes(&m.payload);
                        }
                        Some(_) => {}
                        None => {}
                    }
                }
            }

            // Drain the queue under the in-flight gate. q0 publishes are
            // fire-and-forget, so a send that returns is treated as acked
            // immediately (the in-flight gate still bounds a slow client because
            // a blocked send holds the slot until it returns).
            while let Some(frame) = queue.try_take() {
                queue.on_publish_started();
                let r = client
                    .publish(
                        self.topic_tx.clone(),
                        rumqttc::QoS::AtMostOnce,
                        false,
                        frame,
                    )
                    .await;
                queue.on_publish_acked();
                match r {
                    Ok(()) => metrics.frames_published += 1,
                    Err(_) => metrics.publish_errors += 1,
                }
            }
        }

        reader.abort();
        tracing::info!(
            frames_in = metrics.frames_in,
            frames_published = metrics.frames_published,
            frames_dropped_queue_full = metrics.frames_dropped_queue_full,
            "mavlink relay stopped"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- the parity crown jewel: drop-oldest at QUEUE_MAXSIZE ----

    #[test]
    fn drop_oldest_kicks_in_at_capacity() {
        // Small capacity to exercise the edge: fill to capacity, then one more
        // push drops the OLDEST and keeps the newest.
        let mut q = BoundedPublishQueue::with_bounds(3, 100);
        assert!(!q.push(b"a".to_vec()));
        assert!(!q.push(b"b".to_vec()));
        assert!(!q.push(b"c".to_vec()));
        assert_eq!(q.len(), 3);
        // The 4th push is over capacity: oldest ("a") dropped, "d" appended.
        assert!(q.push(b"d".to_vec()));
        assert_eq!(q.len(), 3);
        assert_eq!(q.dropped_oldest(), 1);
        // Drain order: b, c, d (a was dropped).
        assert_eq!(q.try_take().as_deref(), Some(&b"b"[..]));
        assert_eq!(q.try_take().as_deref(), Some(&b"c"[..]));
        assert_eq!(q.try_take().as_deref(), Some(&b"d"[..]));
        assert!(q.try_take().is_none());
    }

    #[test]
    fn drop_oldest_at_the_production_2000_bound() {
        // At exactly QUEUE_MAXSIZE the queue is full; the next push drops one.
        let mut q = BoundedPublishQueue::new();
        for i in 0..QUEUE_MAXSIZE {
            assert!(!q.push(vec![i as u8]), "no drop until full");
        }
        assert_eq!(q.len(), QUEUE_MAXSIZE);
        assert_eq!(q.dropped_oldest(), 0);
        // The 2001st frame drops the oldest.
        assert!(q.push(vec![0xFF]));
        assert_eq!(q.len(), QUEUE_MAXSIZE);
        assert_eq!(q.dropped_oldest(), 1);
    }

    // ---- the parity crown jewel: inflight gate at INFLIGHT_LIMIT ----

    #[test]
    fn inflight_gate_withholds_at_the_limit() {
        // With a tiny inflight limit, try_take withholds frames once that many
        // publishes are outstanding, and resumes after acks.
        let mut q = BoundedPublishQueue::with_bounds(100, 2);
        for i in 0..5u8 {
            q.push(vec![i]);
        }
        // Take up to the inflight limit (2), marking each started.
        assert!(q.try_take().is_some());
        q.on_publish_started();
        assert!(q.try_take().is_some());
        q.on_publish_started();
        assert_eq!(q.inflight(), 2);
        // At the limit: try_take withholds even though frames remain queued.
        assert!(q.try_take().is_none());
        assert_eq!(q.len(), 3);
        // An ack frees a slot; try_take resumes.
        q.on_publish_acked();
        assert_eq!(q.inflight(), 1);
        assert!(q.try_take().is_some());
    }

    #[test]
    fn inflight_gate_at_the_production_1000_bound() {
        let mut q = BoundedPublishQueue::with_bounds(QUEUE_MAXSIZE, INFLIGHT_LIMIT);
        // Enqueue more than the inflight limit.
        for i in 0..(INFLIGHT_LIMIT + 50) {
            q.push(vec![(i & 0xFF) as u8]);
        }
        // Take exactly INFLIGHT_LIMIT, marking each started.
        for _ in 0..INFLIGHT_LIMIT {
            assert!(q.try_take().is_some());
            q.on_publish_started();
        }
        assert_eq!(q.inflight(), INFLIGHT_LIMIT);
        // The next take is withheld: at the inflight ceiling.
        assert!(q.try_take().is_none());
        // Acking one frees exactly one slot.
        q.on_publish_acked();
        assert!(q.try_take().is_some());
    }

    #[test]
    fn empty_queue_takes_none_even_below_inflight() {
        let mut q = BoundedPublishQueue::new();
        assert!(q.try_take().is_none());
    }
}
