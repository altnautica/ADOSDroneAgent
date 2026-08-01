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
//! ## What it does now project
//!
//! Publishing blind left the relayed vehicle readable by a ground control
//! station on the transports (which take raw frames) and invisible to every
//! surface on the node itself (which read decoded vehicle state) — the on-box
//! cockpit included, which rendered a healthy relayed aircraft as no aircraft.
//! So alongside the republish, a frame also feeds a [`RelayedVehicle`]: a
//! decoded reading kept deliberately separate from the attached-FC state, and
//! stamped with its own provenance. See [`crate::relayed`] for why that
//! separation is the whole design. The claim above is unchanged — the relayed
//! projection sets no connected flag and no heartbeat clock.
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
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::Serialize;
use tokio::sync::{mpsc, Notify};

use ados_protocol::ipc::InboundCommand;

use crate::aux_tee::mavlink_message_id;
use crate::connection::FcConnection;
use crate::relayed::RelayedVehicle;

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
///
/// `relayed` is the decoded projection of the vehicle on the other end of the
/// lane. It is fed from the same frame that is republished, and it is a strictly
/// additive read: the decode runs first only because the frame is then moved
/// into the fan-out, and it cannot prevent the republish. [`RelayedVehicle::apply_frame`]
/// is total — an unreadable header, an id this projection does not read, and a
/// frame this build's dialect cannot parse are each an early return, never a
/// panic — and a poisoned lock is recovered rather than propagated. So every
/// frame reaches a ground control station exactly as it did before.
pub fn publish(
    fc: &Arc<FcConnection>,
    counters: &IngestCounters,
    relayed: &Mutex<RelayedVehicle>,
    frame: Vec<u8>,
) {
    if !is_mavlink_frame(&frame) {
        counters.rejected_malformed.fetch_add(1, Ordering::Relaxed);
        return;
    }
    let len = frame.len() as u64;

    // Decode before the frame is moved into the fan-out. A poisoned lock here
    // must not take the republish down with it: the transports are the
    // load-bearing path and the projection is the read-only extra.
    {
        let now = Instant::now();
        let now_iso = crate::connection::transport::now_iso();
        match relayed.lock() {
            Ok(mut r) => {
                r.apply_frame(&frame, &now_iso, now);
            }
            Err(poisoned) => {
                // Recover rather than propagate: dropping the projection for
                // the process lifetime would silently blank every surface that
                // reads it, which is the failure this whole path exists to fix.
                poisoned.into_inner().apply_frame(&frame, &now_iso, now);
            }
        }
    }

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
    mut inbound: mpsc::Receiver<InboundCommand>,
    fc: Arc<FcConnection>,
    counters: Arc<IngestCounters>,
    relayed: Arc<Mutex<RelayedVehicle>>,
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
                // The ingest socket runs the other way from the command
                // socket: these are frames a vehicle sent, not commands for
                // one. They already crossed a radio, so the writer identity the
                // socket records adds nothing here and only the bytes are read.
                Some(cmd) => publish(&fc, &counters, &relayed, cmd.payload),
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

    /// A fully-formed, parseable HEARTBEAT frame.
    ///
    /// Distinct from [`heartbeat`] above, which is a hand-built header with a
    /// deliberately invalid checksum: that is all the republish path needs (it
    /// reads the header and never parses), but the relayed projection does
    /// parse, so exercising it needs a frame that survives a real decode.
    fn decodable_heartbeat() -> Vec<u8> {
        use ados_protocol::mavlink::ardupilotmega::{
            MavAutopilot, MavModeFlag, MavState, MavType, HEARTBEAT_DATA,
        };
        use ados_protocol::mavlink::{serialize_v2, MavHeader, MavMessage};
        let msg = MavMessage::HEARTBEAT(HEARTBEAT_DATA {
            custom_mode: 0,
            mavtype: MavType::MAV_TYPE_QUADROTOR,
            autopilot: MavAutopilot::MAV_AUTOPILOT_ARDUPILOTMEGA,
            base_mode: MavModeFlag::empty(),
            system_status: MavState::MAV_STATE_STANDBY,
            mavlink_version: 3,
        });
        serialize_v2(
            MavHeader {
                system_id: 1,
                component_id: 1,
                sequence: 0,
            },
            &msg,
        )
        .unwrap()
    }

    /// Wrap raw bytes as the socket would hand them to the loop.
    fn ingested(payload: Vec<u8>) -> InboundCommand {
        InboundCommand {
            payload,
            peer: Default::default(),
        }
    }

    /// The tests' relayed projection. Spelled with the full path because this
    /// module's test scope also imports tokio's `Mutex` for the FC state, and
    /// the two are distinct types.
    fn relayed() -> Arc<std::sync::Mutex<RelayedVehicle>> {
        Arc::new(std::sync::Mutex::new(RelayedVehicle::default()))
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

        publish(&fc, &counters, &relayed(), frame.clone());

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

        publish(&fc, &counters, &relayed(), heartbeat());
        publish(&fc, &counters, &relayed(), v1_attitude());

        assert_eq!(consumer.recv().await.unwrap(), heartbeat());
        assert_eq!(consumer.recv().await.unwrap(), v1_attitude());
        assert_eq!(counters.snapshot().frames_published, 2);
    }

    #[tokio::test]
    async fn garbage_is_rejected_rather_than_handed_on_as_telemetry() {
        let fc = connection();
        let counters = Arc::new(IngestCounters::default());
        let mut consumer = fc.subscribe();

        publish(&fc, &counters, &relayed(), b"not a frame".to_vec());
        publish(&fc, &counters, &relayed(), Vec::new());
        // The good frame still gets through, so the reject is per-frame and not
        // a lane that latches shut on one bad input.
        publish(&fc, &counters, &relayed(), heartbeat());

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

        publish(&fc, &counters, &relayed(), heartbeat());

        let snap = counters.snapshot();
        assert_eq!(snap.frames_published, 1);
        assert_eq!(snap.published_no_subscriber, 1);
    }

    #[tokio::test]
    async fn a_published_frame_also_becomes_readable_vehicle_state() {
        // The regression this seam was missing: frames reached every transport
        // but nothing on the node decoded them, so the on-box cockpit rendered
        // a healthy relayed aircraft as no aircraft at all.
        let fc = connection();
        let counters = Arc::new(IngestCounters::default());
        let relayed = relayed();

        publish(&fc, &counters, &relayed, decodable_heartbeat());

        let wire = relayed
            .lock()
            .unwrap()
            .to_wire(Instant::now())
            .expect("a relayed frame produces a readable snapshot");
        assert_eq!(wire["source"], "relayed");
        assert_eq!(wire["fresh"], true);
        assert_eq!(wire["frames_decoded"], 1);
    }

    #[tokio::test]
    async fn a_frame_the_projection_does_not_read_is_still_republished() {
        // The projection is strictly additive. A frame this projection does not
        // read must still reach a ground control station byte for byte, or
        // adding the read would have narrowed the relay.
        //
        // This is the id-filter path, not the parse-failure path: the id below
        // is filtered before the parser is ever reached, which is why nothing
        // is counted as corrupt. The parse-failure path — an id this projection
        // DOES read whose body will not decode — is covered where the counter
        // lives, by `relayed::tests::a_corrupt_frame_of_a_read_id_is_counted_as_undecodable`.
        let fc = connection();
        let counters = Arc::new(IngestCounters::default());
        let relayed = relayed();
        let mut consumer = fc.subscribe();
        // A readable header carrying a message id no dialect names.
        let mut unknown = heartbeat();
        unknown[7] = 0xFE;
        unknown[8] = 0xFF;

        publish(&fc, &counters, &relayed, unknown.clone());

        assert_eq!(consumer.recv().await.unwrap(), unknown);
        assert_eq!(counters.snapshot().frames_published, 1);
        // It fed nothing, and was not miscounted as corruption.
        assert_eq!(relayed.lock().unwrap().frames_undecodable(), 0);
        assert!(relayed.lock().unwrap().to_wire(Instant::now()).is_none());
    }

    #[tokio::test]
    async fn publishing_never_claims_a_flight_controller_is_attached() {
        // The whole point of the seam: relaying someone else's vehicle is not
        // the same claim as having one attached. Asserted with the projection
        // populated, because that is the case where the two could be confused —
        // the node now has a full decoded reading of an aircraft and must still
        // report no flight controller of its own.
        let fc = connection();
        let counters = Arc::new(IngestCounters::default());
        let relayed = relayed();

        publish(&fc, &counters, &relayed, decodable_heartbeat());

        assert!(
            relayed.lock().unwrap().is_fresh(Instant::now()),
            "the projection is populated, so this is the confusable case"
        );
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

        let task = tokio::spawn(run(
            rx,
            fc.clone(),
            counters.clone(),
            relayed(),
            cancel.clone(),
        ));
        tx.send(ingested(heartbeat())).await.unwrap();
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
        let (tx, rx) = mpsc::channel::<InboundCommand>(8);

        let task = tokio::spawn(run(rx, fc, counters, relayed(), cancel));
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
