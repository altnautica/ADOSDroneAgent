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
//! transmit buffer, and freeze telemetry. Frames therefore land in a bounded
//! queue with a drop-oldest policy (recency beats completeness). The queue is
//! drained only while the client's request channel takes publishes: a refused
//! publish leaves its frames queued for the next drain, and an overflowing
//! queue drops its oldest frame, so the newest telemetry goes out first once
//! the uplink recovers. Frames are batched for up to [`BATCH_WINDOW`] or
//! [`BATCH_MAX_BYTES`] into one publish of concatenated MAVLink frames; the
//! GCS feeds each payload to a stream parser, so a batch parses as the frames
//! it carries.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use ados_plugin_host::frame_link::FrameLink;
use tokio::sync::{mpsc, watch};
use tokio::time::Instant;

use super::transport::{MqttQos, MqttTransport, RumqttcTransport};
use super::{relay_username, topic_mavlink_rx, topic_mavlink_tx};
use crate::mqtt::transport::TransportConfig;

/// Frame queue depth. 2000 frames at ~30 msg/s gives ~66 s of headroom before
/// drops start. The earlier 200-frame size produced a 17.4 % sustained drop
/// rate over a slow WSS tunnel.
pub const QUEUE_MAXSIZE: usize = 2000;

/// The longest a queued frame waits for batch-mates before it is published.
pub const BATCH_WINDOW: Duration = Duration::from_millis(50);

/// The payload size at which a batch is published without waiting for the
/// window, and the cap on one publish's payload (a single larger frame still
/// goes out alone).
pub const BATCH_MAX_BYTES: usize = 4096;

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

/// The bounded drop-oldest frame queue, factored out of the relay so its
/// bounds and batching are unit-testable with no MQTT and no IPC.
///
/// * [`push`](Self::push): enqueue a frame; on a full queue drop the OLDEST
///   frame to make room, counting the drop.
/// * [`peek_batch`](Self::peek_batch) / [`consume`](Self::consume): the
///   publisher builds a batch from the front without removing it, and removes
///   it only once the client accepted the publish.
/// * [`batch_due`](Self::batch_due): the batch window or byte budget is spent.
#[derive(Debug)]
pub struct BoundedPublishQueue {
    queue: VecDeque<Vec<u8>>,
    capacity: usize,
    bytes: usize,
    /// When the oldest frame still queued arrived (reset when the queue
    /// empties), the start of the current batch window.
    window_start: Option<Instant>,
    dropped_oldest: u64,
}

impl BoundedPublishQueue {
    /// A queue with the production bound (`QUEUE_MAXSIZE`).
    pub fn new() -> Self {
        Self::with_capacity(QUEUE_MAXSIZE)
    }

    /// A queue with an explicit frame bound (tests use small bounds to exercise
    /// the edge without enqueuing thousands of frames).
    pub fn with_capacity(capacity: usize) -> Self {
        BoundedPublishQueue {
            queue: VecDeque::new(),
            capacity,
            bytes: 0,
            window_start: None,
            dropped_oldest: 0,
        }
    }

    /// Enqueue a frame. On a full queue, drop the oldest frame first (recency
    /// beats completeness), counting the drop. Returns `true` when an oldest
    /// frame was dropped to make room.
    pub fn push(&mut self, frame: Vec<u8>) -> bool {
        let mut dropped = false;
        if self.queue.len() >= self.capacity {
            if let Some(old) = self.queue.pop_front() {
                self.bytes -= old.len();
            }
            self.dropped_oldest += 1;
            dropped = true;
        }
        if self.window_start.is_none() {
            self.window_start = Some(Instant::now());
        }
        self.bytes += frame.len();
        self.queue.push_back(frame);
        dropped
    }

    /// When the current batch must be published: the window's start plus
    /// [`BATCH_WINDOW`]. `None` while the queue is empty.
    pub fn flush_deadline(&self) -> Option<Instant> {
        self.window_start.map(|t| t + BATCH_WINDOW)
    }

    /// Whether the queued frames are due: the byte budget is reached or the
    /// oldest has waited the whole window.
    pub fn batch_due(&self, now: Instant) -> bool {
        self.bytes >= BATCH_MAX_BYTES || self.flush_deadline().is_some_and(|d| now >= d)
    }

    /// The concatenation of the front frames that fits `max_bytes` (at least
    /// one frame), and how many frames it holds. Nothing is removed.
    pub fn peek_batch(&self, max_bytes: usize) -> Option<(Vec<u8>, usize)> {
        let first = self.queue.front()?;
        let mut take = 1;
        let mut size = first.len();
        for f in self.queue.iter().skip(1) {
            if size + f.len() > max_bytes {
                break;
            }
            size += f.len();
            take += 1;
        }
        let mut payload = Vec::with_capacity(size);
        for f in self.queue.iter().take(take) {
            payload.extend_from_slice(f);
        }
        Some((payload, take))
    }

    /// Remove the `n` front frames a publish carried.
    pub fn consume(&mut self, n: usize) {
        for f in self.queue.drain(..n.min(self.queue.len())) {
            self.bytes -= f.len();
        }
        if self.queue.is_empty() {
            self.window_start = None;
        }
    }

    /// Start a fresh batch window for the frames still queued, so a refused
    /// drain is retried one window later instead of on every loop turn.
    pub fn restart_window(&mut self) {
        if !self.queue.is_empty() {
            self.window_start = Some(Instant::now());
        }
    }

    /// Drop every queued frame, returning how many were dropped.
    pub fn clear(&mut self) -> usize {
        let n = self.queue.len();
        self.queue.clear();
        self.bytes = 0;
        self.window_start = None;
        n
    }

    /// Current queued frame count.
    pub fn len(&self) -> usize {
        self.queue.len()
    }

    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }

    /// Total queued payload bytes.
    pub fn bytes(&self) -> usize {
        self.bytes
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

/// The non-blocking publish the drain hands batches to. [`rumqttc::AsyncClient`]
/// is the production impl; an `Err` means the client did not take the payload
/// (its request channel is full or gone).
pub(crate) trait FramePublisher {
    fn try_publish_frame(&self, topic: &str, payload: Vec<u8>) -> Result<(), ()>;
}

impl FramePublisher for rumqttc::AsyncClient {
    fn try_publish_frame(&self, topic: &str, payload: Vec<u8>) -> Result<(), ()> {
        self.try_publish(topic, rumqttc::QoS::AtMostOnce, false, payload)
            .map_err(|_| ())
    }
}

/// The MAVLink-over-MQTT relay. Owns its own rumqttc client (not the shared
/// transport trait) so the bounded queue and batching govern the hot publish
/// path directly.
pub struct MavlinkMqttRelay {
    device_id: String,
    topic_tx: String,
    topic_rx: String,
    transport_config: TransportConfig,
}

impl MavlinkMqttRelay {
    /// Build the relay for a device id + broker dial config. The transport
    /// config's username is the `ados-{id}` relay form; callers wire the broker
    /// host/port/password.
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
        let transport = RumqttcTransport::connect(&self.transport_config)?;
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

        // The IPC link (FC frames in, commands out). It reconnects on its own,
        // so a router restart heals without restarting the relay. Its one
        // declaration says what this connection is and is written before any
        // command on every connection: every command that follows arrived over
        // the broker, so the router must not read it as produced on the node.
        let ipc = std::sync::Arc::new(FrameLink::spawn(
            ipc_sock,
            Box::new(|| vec![ados_protocol::ipc::IPC_DECLARE_OFF_BOX_SOURCE.to_vec()]),
        ));

        // GCS->FC: subscribe rx and write received payloads to the IPC socket.
        // Taken through the transport (NOT the raw client) so the topic is
        // recorded for replay: the broker discards the subscription on every
        // fresh session, and without the replay this uplink dies at the first
        // reconnect while `mavlink/tx` keeps flowing and the link still reads
        // connected.
        if let Err(e) = transport
            .subscribe(&self.topic_rx, MqttQos::AtMostOnce)
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
        let connected = transport.connected_handle();

        loop {
            let flush_at = queue.flush_deadline();
            tokio::select! {
                _ = shutdown.changed() => {
                    if *shutdown.borrow() {
                        break;
                    }
                }
                // The oldest queued frame has waited the batch window.
                _ = tokio::time::sleep_until(flush_at.unwrap_or_else(Instant::now)), if flush_at.is_some() => {}
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
                            if let Err(e) = ipc.send(&m.payload) {
                                tracing::debug!(reason = e.reason(), "mavlink relay: command not forwarded");
                            }
                        }
                        Some(_) => {}
                        // The transport's event loop is gone: there is no broker
                        // session left to relay over. End the run so the
                        // supervisor respawns a fresh one.
                        None => break,
                    }
                }
            }

            if queue.batch_due(Instant::now()) {
                drain_publish_queue(
                    &mut queue,
                    &mut metrics,
                    &client,
                    &connected,
                    &self.topic_tx,
                );
            }
        }

        reader.abort();
        tracing::info!(
            frames_in = metrics.frames_in,
            frames_published = metrics.frames_published,
            frames_dropped_queue_full = metrics.frames_dropped_queue_full,
            frames_dropped_not_connected = metrics.frames_dropped_not_connected,
            publish_errors = metrics.publish_errors,
            "mavlink relay stopped"
        );
        Ok(())
    }
}

/// Drain the queue onto `topic` at q0 in batches of up to [`BATCH_MAX_BYTES`],
/// without ever blocking the relay loop.
///
/// Frames queued while the broker session is not confirmed up are dropped and
/// counted: q0 telemetry queued inside the client during an outage would be
/// delivered minutes late on reconnect as if it were live. While up, the drain
/// continues only while the client takes each publish; the first refusal (a
/// full request channel) stops it and leaves the frames queued, so the
/// drop-oldest bound decides what survives the congestion. Shared by the
/// MAVLink and MSP relays.
pub(crate) fn drain_publish_queue<P: FramePublisher + ?Sized>(
    queue: &mut BoundedPublishQueue,
    metrics: &mut RelayMetrics,
    client: &P,
    connected: &AtomicBool,
    topic: &str,
) {
    if !connected.load(Ordering::Acquire) {
        metrics.frames_dropped_not_connected += queue.clear() as u64;
        return;
    }
    while let Some((payload, frames)) = queue.peek_batch(BATCH_MAX_BYTES) {
        match client.try_publish_frame(topic, payload) {
            Ok(()) => {
                queue.consume(frames);
                metrics.frames_published += frames as u64;
            }
            Err(()) => {
                metrics.publish_errors += 1;
                queue.restart_window();
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};

    /// A publisher with a fixed number of free request slots, recording what
    /// it accepted. `free` models the client's request channel room.
    #[derive(Default)]
    struct FakeClient {
        free: Cell<usize>,
        published: RefCell<Vec<Vec<u8>>>,
    }

    impl FramePublisher for FakeClient {
        fn try_publish_frame(&self, _topic: &str, payload: Vec<u8>) -> Result<(), ()> {
            if self.free.get() == 0 {
                return Err(());
            }
            self.free.set(self.free.get() - 1);
            self.published.borrow_mut().push(payload);
            Ok(())
        }
    }

    const TOPIC: &str = "ados/d/mavlink/tx";

    #[tokio::test]
    async fn frames_queued_while_the_broker_is_down_are_dropped_not_published() {
        // During an outage the relay must not hand telemetry to the client: it
        // would sit in the request queue and reach the GCS minutes late, as if
        // live, once the session came back.
        let client = FakeClient::default();
        client.free.set(100);
        let mut queue = BoundedPublishQueue::with_capacity(10);
        for i in 0..3u8 {
            queue.push(vec![i]);
        }
        let mut metrics = RelayMetrics::default();
        let down = AtomicBool::new(false);
        drain_publish_queue(&mut queue, &mut metrics, &client, &down, TOPIC);
        assert_eq!(metrics.frames_dropped_not_connected, 3);
        assert!(client.published.borrow().is_empty());
        assert!(queue.is_empty());
    }

    #[tokio::test]
    async fn queue_publishes_newest_after_backpressure() {
        // The client takes nothing: frames stay queued and the bound keeps the
        // newest. Once the client drains, the newest frames are what go out.
        let client = FakeClient::default();
        let capacity = 4usize;
        let mut queue = BoundedPublishQueue::with_capacity(capacity);
        let mut metrics = RelayMetrics::default();
        let up = AtomicBool::new(true);
        for i in 0..(3 * capacity as u8) {
            if queue.push(vec![i]) {
                metrics.frames_dropped_queue_full += 1;
            }
            drain_publish_queue(&mut queue, &mut metrics, &client, &up, TOPIC);
        }
        assert!(client.published.borrow().is_empty());
        assert_eq!(queue.len(), capacity);
        assert_eq!(metrics.frames_dropped_queue_full, 2 * capacity as u64);

        client.free.set(10);
        drain_publish_queue(&mut queue, &mut metrics, &client, &up, TOPIC);
        assert_eq!(*client.published.borrow(), vec![vec![8u8, 9, 10, 11]]);
        assert_eq!(metrics.frames_published, capacity as u64);
        assert!(queue.is_empty());
    }

    #[tokio::test]
    async fn a_refused_publish_keeps_its_frames_for_the_next_drain() {
        let client = FakeClient::default();
        let mut queue = BoundedPublishQueue::with_capacity(10);
        let mut metrics = RelayMetrics::default();
        let up = AtomicBool::new(true);
        queue.push(vec![1, 2]);
        drain_publish_queue(&mut queue, &mut metrics, &client, &up, TOPIC);
        assert_eq!(queue.len(), 1);
        assert_eq!(metrics.publish_errors, 1);
        client.free.set(1);
        drain_publish_queue(&mut queue, &mut metrics, &client, &up, TOPIC);
        assert_eq!(*client.published.borrow(), vec![vec![1u8, 2]]);
    }

    #[tokio::test(start_paused = true)]
    async fn batching_concatenates_within_50ms() {
        let client = FakeClient::default();
        client.free.set(10);
        let mut queue = BoundedPublishQueue::with_capacity(100);
        let mut metrics = RelayMetrics::default();
        let up = AtomicBool::new(true);
        let start = Instant::now();
        queue.push(vec![0xFD, 1]);
        tokio::time::advance(Duration::from_millis(10)).await;
        queue.push(vec![0xFD, 2]);
        tokio::time::advance(Duration::from_millis(10)).await;
        queue.push(vec![0xFE, 3]);
        // Inside the window and under the byte budget: not due yet.
        assert!(!queue.batch_due(Instant::now()));
        assert_eq!(queue.flush_deadline(), Some(start + BATCH_WINDOW));
        tokio::time::advance(Duration::from_millis(30)).await;
        assert!(queue.batch_due(Instant::now()));
        drain_publish_queue(&mut queue, &mut metrics, &client, &up, TOPIC);
        assert_eq!(
            *client.published.borrow(),
            vec![vec![0xFD, 1, 0xFD, 2, 0xFE, 3]]
        );
        assert_eq!(metrics.frames_published, 3);
        assert_eq!(queue.flush_deadline(), None);
    }

    #[tokio::test]
    async fn a_full_byte_budget_is_due_at_once_and_splits_publishes() {
        let client = FakeClient::default();
        client.free.set(10);
        let mut queue = BoundedPublishQueue::with_capacity(100);
        let mut metrics = RelayMetrics::default();
        let up = AtomicBool::new(true);
        let frame = vec![0u8; 1500];
        for _ in 0..3 {
            queue.push(frame.clone());
        }
        assert!(queue.batch_due(Instant::now()));
        drain_publish_queue(&mut queue, &mut metrics, &client, &up, TOPIC);
        let sizes: Vec<usize> = client.published.borrow().iter().map(Vec::len).collect();
        assert_eq!(sizes, vec![3000, 1500]);
        assert!(sizes.iter().all(|s| *s <= BATCH_MAX_BYTES));
    }

    #[test]
    fn drop_oldest_kicks_in_at_capacity() {
        let mut q = BoundedPublishQueue::with_capacity(3);
        assert!(!q.push(b"a".to_vec()));
        assert!(!q.push(b"b".to_vec()));
        assert!(!q.push(b"c".to_vec()));
        // The 4th push is over capacity: oldest ("a") dropped, "d" appended.
        assert!(q.push(b"d".to_vec()));
        assert_eq!(q.len(), 3);
        assert_eq!(q.bytes(), 3);
        assert_eq!(q.dropped_oldest(), 1);
        assert_eq!(q.peek_batch(usize::MAX), Some((b"bcd".to_vec(), 3)));
    }

    #[test]
    fn drop_oldest_at_the_production_2000_bound() {
        let mut q = BoundedPublishQueue::new();
        for i in 0..QUEUE_MAXSIZE {
            assert!(!q.push(vec![i as u8]), "no drop until full");
        }
        assert_eq!(q.dropped_oldest(), 0);
        assert!(q.push(vec![0xFF]));
        assert_eq!(q.len(), QUEUE_MAXSIZE);
        assert_eq!(q.dropped_oldest(), 1);
    }

    #[tokio::test]
    async fn the_real_client_takes_a_publish_without_a_poller() {
        // An unpolled rumqttc client accepts into its request channel until it
        // is full and never blocks the caller.
        let opts = rumqttc::MqttOptions::new("ados-test", ("127.0.0.1", 1));
        let (client, _eventloop) = rumqttc::AsyncClient::builder(opts).build();
        let mut queue = BoundedPublishQueue::with_capacity(10);
        queue.push(vec![1]);
        let mut metrics = RelayMetrics::default();
        let up = AtomicBool::new(true);
        drain_publish_queue(&mut queue, &mut metrics, &client, &up, TOPIC);
        assert_eq!(metrics.frames_published + metrics.publish_errors, 1);
    }
}
