//! Accept MAVLink frames received off-board and publish them to the fan-out.
//!
//! The mirror of [`crate::aux_tee`]. The tee is the drone's half: it copies the
//! flight controller's frames onto the radio's auxiliary lane. This is the
//! ground station's half of the same path: frames the ground data plane decoded
//! off that lane arrive here and are published to this router's frame fan-out,
//! so the transports the router already serves carry the vehicle. A ground
//! control station connected to the ground station then sees the vehicle
//! exactly as it would on a direct link, over the ports it already uses.
//!
//! ## Why a separate socket
//!
//! The router's MAVLink socket already accepts inbound bytes, but that path
//! runs the other way: bytes a client writes there are forwarded to the flight
//! controller. Reusing it would aim a vehicle's own telemetry at an autopilot.
//! The two directions must not share a socket, so this is its own.
//!
//! ## What this does not claim
//!
//! Publishing a frame here does not make the node report a flight controller.
//! [`FcConnection::inject_frame`] publishes to the fan-out and touches nothing
//! else, so a ground station relaying a vehicle still honestly reports that it
//! has no flight controller of its own. A relayed vehicle is not an attached
//! one.
//!
//! ## Bounded by construction
//!
//! The socket's inbound queue is fixed at [`INGEST_QUEUE_DEPTH`]. A producer
//! that outruns this loop is stalled by that queue on its own connection and
//! nowhere else — the flight controller read loop, the vehicle state publisher
//! and the transports share none of it. Publishing itself is a broadcast send,
//! which drops for a slow subscriber rather than blocking the sender, so a
//! stalled ground control station cannot back pressure onto this loop either.
//!
//! ## Frames are checked, not trusted
//!
//! A frame arriving here has crossed a radio and a decode, so it is checked for
//! a readable MAVLink header before it is published. Anything else is counted
//! as rejected rather than passed to a ground control station as if it were
//! telemetry. The check is a header read, not a dialect parse, so a message
//! this build does not know is still forwarded — the alternative would silently
//! drop real traffic from a newer vehicle.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde::Serialize;
use tokio::sync::{mpsc, Notify};

use crate::aux_tee::mavlink_message_id;
use crate::connection::FcConnection;

/// Inbound queue depth for the republish socket.
///
/// Matches the MAVLink socket's depth: the traffic is the same frames at the
/// same rates, so the same backlog allowance is the right one. It is the bound
/// that keeps a fast producer from growing memory here.
pub const INGEST_QUEUE_DEPTH: usize = 256;

/// How often the counters are logged, and only when something changed.
const COUNTER_REPORT_INTERVAL: Duration = Duration::from_secs(60);

/// Live counters for the republish seam.
///
/// Every frame that arrives leaves through exactly one of these, so the tally
/// accounts for the whole stream rather than only its successes.
#[derive(Debug, Default)]
pub struct IngestCounters {
    /// Frames published to the fan-out.
    pub frames_published: AtomicU64,
    /// Bytes those frames accounted for.
    pub bytes_published: AtomicU64,
    /// Frames published while nothing was subscribed to the fan-out, and so
    /// went nowhere. Normal when no ground control station is connected; it is
    /// counted rather than assumed so a silent lane can be told apart from an
    /// idle one.
    pub published_no_subscriber: AtomicU64,
    /// Frames rejected for having no readable MAVLink header. Not forwarded:
    /// handing unrecognised bytes to a ground control station as telemetry is
    /// worse than dropping them, and a non-zero count here is a real signal
    /// that something upstream is corrupting the lane.
    pub rejected_malformed: AtomicU64,
}

impl IngestCounters {
    /// A point-in-time copy for logging and status surfaces.
    pub fn snapshot(&self) -> IngestCountersSnapshot {
        IngestCountersSnapshot {
            frames_published: self.frames_published.load(Ordering::Relaxed),
            bytes_published: self.bytes_published.load(Ordering::Relaxed),
            published_no_subscriber: self.published_no_subscriber.load(Ordering::Relaxed),
            rejected_malformed: self.rejected_malformed.load(Ordering::Relaxed),
        }
    }
}

/// A plain copy of [`IngestCounters`], for logs and the state snapshot.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct IngestCountersSnapshot {
    pub frames_published: u64,
    pub bytes_published: u64,
    pub published_no_subscriber: u64,
    pub rejected_malformed: u64,
}

/// Whether a buffer carries a readable MAVLink frame header.
///
/// A header read rather than a dialect parse, on purpose: a full parse fails on
/// any message this build's dialect does not carry, which would silently drop
/// real traffic from a vehicle running a newer or wider dialect than ours.
pub fn is_mavlink_frame(frame: &[u8]) -> bool {
    mavlink_message_id(frame).is_some()
}

/// Log the counters when they have moved since the last report.
fn report(counters: &IngestCounters, last: IngestCountersSnapshot) -> IngestCountersSnapshot {
    let now = counters.snapshot();
    if now != last {
        tracing::info!(
            frames_published = now.frames_published,
            bytes_published = now.bytes_published,
            published_no_subscriber = now.published_no_subscriber,
            rejected_malformed = now.rejected_malformed,
            "mavlink_frame_ingest_counters"
        );
    }
    now
}

/// Publish one received frame, updating the counters for whichever way it went.
///
/// Split out from the run loop so the accept-and-publish decision is testable
/// without a socket.
pub fn publish(fc: &Arc<FcConnection>, counters: &IngestCounters, frame: Vec<u8>) {
    if !is_mavlink_frame(&frame) {
        counters.rejected_malformed.fetch_add(1, Ordering::Relaxed);
        return;
    }
    let len = frame.len() as u64;
    let delivered = fc.inject_frame(frame);
    counters.frames_published.fetch_add(1, Ordering::Relaxed);
    counters.bytes_published.fetch_add(len, Ordering::Relaxed);
    if !delivered {
        counters
            .published_no_subscriber
            .fetch_add(1, Ordering::Relaxed);
    }
}

/// Run the republish seam until cancelled or the socket closes.
///
/// Takes the inbound frame receiver rather than binding the socket itself, so
/// the whole loop is exercisable over a plain channel with no socket and no
/// flight controller.
pub async fn run(
    mut inbound: mpsc::Receiver<Vec<u8>>,
    fc: Arc<FcConnection>,
    counters: Arc<IngestCounters>,
    cancel: Arc<Notify>,
) {
    let mut last_report = IngestCountersSnapshot::default();
    let mut report_tick = tokio::time::interval_at(
        tokio::time::Instant::now() + COUNTER_REPORT_INTERVAL,
        COUNTER_REPORT_INTERVAL,
    );
    report_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    tracing::info!(
        queue_depth = INGEST_QUEUE_DEPTH,
        "mavlink_frame_ingest_started"
    );

    loop {
        tokio::select! {
            biased;
            _ = cancel.notified() => break,
            _ = report_tick.tick() => {
                last_report = report(&counters, last_report);
            }
            received = inbound.recv() => match received {
                Some(frame) => publish(&fc, &counters, frame),
                None => break,
            },
        }
    }

    report(&counters, last_report);
    tracing::info!("mavlink_frame_ingest_stopped");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::MavlinkConfig;
    use crate::param_cache::ParamCache;
    use crate::state::VehicleState;
    use tokio::sync::Mutex;

    /// A heartbeat frame: v2 magic, then enough header for the message id at
    /// bytes 7 through 9 to read as 0.
    fn heartbeat() -> Vec<u8> {
        vec![
            0xFD, 0x09, 0x00, 0x00, 0x07, 0x01, 0x01, 0x00, 0x00, 0x00, 0xAA, 0xBB,
        ]
    }

    /// A v1 frame carrying message id 30 (attitude).
    fn v1_attitude() -> Vec<u8> {
        vec![0xFE, 0x1C, 0x05, 0x01, 0x01, 30, 0x00, 0x00]
    }

    fn connection() -> Arc<FcConnection> {
        FcConnection::new(
            MavlinkConfig::default(),
            Arc::new(Mutex::new(VehicleState::default())),
            Arc::new(Mutex::new(ParamCache::default_path())),
        )
    }

    #[tokio::test]
    async fn a_received_frame_reaches_every_fan_out_consumer_byte_for_byte() {
        // The fan-out is what the IPC socket and all three transports read, so
        // a subscriber seeing the exact bytes is the republish working.
        let fc = connection();
        let counters = Arc::new(IngestCounters::default());
        let mut consumer = fc.subscribe();
        let frame = heartbeat();

        publish(&fc, &counters, frame.clone());

        assert_eq!(consumer.recv().await.unwrap(), frame);
        let snap = counters.snapshot();
        assert_eq!(snap.frames_published, 1);
        assert_eq!(snap.bytes_published, frame.len() as u64);
        assert_eq!(snap.published_no_subscriber, 0);
    }

    #[tokio::test]
    async fn both_mavlink_versions_are_republished() {
        let fc = connection();
        let counters = Arc::new(IngestCounters::default());
        let mut consumer = fc.subscribe();

        publish(&fc, &counters, heartbeat());
        publish(&fc, &counters, v1_attitude());

        assert_eq!(consumer.recv().await.unwrap(), heartbeat());
        assert_eq!(consumer.recv().await.unwrap(), v1_attitude());
        assert_eq!(counters.snapshot().frames_published, 2);
    }

    #[tokio::test]
    async fn garbage_is_rejected_rather_than_handed_on_as_telemetry() {
        let fc = connection();
        let counters = Arc::new(IngestCounters::default());
        let mut consumer = fc.subscribe();

        publish(&fc, &counters, b"not a frame".to_vec());
        publish(&fc, &counters, Vec::new());
        // The good frame still gets through, so the reject is per-frame and not
        // a lane that latches shut on one bad input.
        publish(&fc, &counters, heartbeat());

        assert_eq!(consumer.recv().await.unwrap(), heartbeat());
        let snap = counters.snapshot();
        assert_eq!(snap.rejected_malformed, 2);
        assert_eq!(snap.frames_published, 1);
    }

    #[tokio::test]
    async fn a_frame_with_no_listener_is_counted_not_assumed_delivered() {
        // No subscriber is the normal idle state. It must read as "went
        // nowhere", never as a successful delivery.
        let fc = connection();
        let counters = Arc::new(IngestCounters::default());

        publish(&fc, &counters, heartbeat());

        let snap = counters.snapshot();
        assert_eq!(snap.frames_published, 1);
        assert_eq!(snap.published_no_subscriber, 1);
    }

    #[tokio::test]
    async fn publishing_never_claims_a_flight_controller_is_attached() {
        // The whole point of the seam: relaying someone else's vehicle is not
        // the same claim as having one attached.
        let fc = connection();
        let counters = Arc::new(IngestCounters::default());

        publish(&fc, &counters, heartbeat());

        assert!(!fc.transport_open(), "no transport was opened");
        assert!(!fc.mavlink_alive().await, "no local link came alive");
        assert!(
            fc.heartbeat_age_s().await.is_none(),
            "the heartbeat clock belongs to a local link and must stay unset"
        );
    }

    #[tokio::test]
    async fn the_loop_forwards_from_the_socket_channel_and_stops_on_cancel() {
        let fc = connection();
        let counters = Arc::new(IngestCounters::default());
        let cancel = Arc::new(Notify::new());
        let (tx, rx) = mpsc::channel(8);
        let mut consumer = fc.subscribe();

        let task = tokio::spawn(run(rx, fc.clone(), counters.clone(), cancel.clone()));
        tx.send(heartbeat()).await.unwrap();
        assert_eq!(consumer.recv().await.unwrap(), heartbeat());

        cancel.notify_waiters();
        tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .expect("cancellation must stop the loop")
            .unwrap();
    }

    #[tokio::test]
    async fn the_loop_ends_when_the_socket_channel_closes() {
        let fc = connection();
        let counters = Arc::new(IngestCounters::default());
        let cancel = Arc::new(Notify::new());
        let (tx, rx) = mpsc::channel::<Vec<u8>>(8);

        let task = tokio::spawn(run(rx, fc, counters, cancel));
        drop(tx);

        tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .expect("a closed channel must end the loop")
            .unwrap();
    }

    #[test]
    fn a_header_read_accepts_a_message_this_build_does_not_know() {
        // A dialect parse would reject an unknown message and drop real traffic
        // from a newer vehicle. The header read must not.
        let mut unknown = heartbeat();
        unknown[7] = 0xFE;
        unknown[8] = 0xFF;
        assert!(is_mavlink_frame(&unknown));
        assert!(!is_mavlink_frame(b"\xFD\x01"));
        assert!(!is_mavlink_frame(&[]));
    }
}
