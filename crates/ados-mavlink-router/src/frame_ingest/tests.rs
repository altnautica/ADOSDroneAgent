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

    assert_eq!(consumer.recv().await.unwrap().bytes, frame);
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

    assert_eq!(consumer.recv().await.unwrap().bytes, heartbeat());
    assert_eq!(consumer.recv().await.unwrap().bytes, v1_attitude());
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

    assert_eq!(consumer.recv().await.unwrap().bytes, heartbeat());
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

    assert_eq!(consumer.recv().await.unwrap().bytes, unknown);
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
    let cancel = Shutdown::new();
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
    assert_eq!(consumer.recv().await.unwrap().bytes, heartbeat());

    cancel.trigger();
    tokio::time::timeout(Duration::from_secs(2), task)
        .await
        .expect("cancellation must stop the loop")
        .unwrap();
}

/// A stop that lands while a loop is busy elsewhere (here: before it has
/// first parked on its select) must still end it. The router's shutdown
/// joins every task, so a loop that misses the signal holds the stop until
/// the service manager kills the process.
#[tokio::test]
async fn a_stop_fired_before_the_loop_parks_still_ends_it() {
    let fc = connection();
    let counters = Arc::new(IngestCounters::default());
    let cancel = Shutdown::new();
    let (_tx, rx) = mpsc::channel::<InboundCommand>(8);
    cancel.trigger();

    let task = tokio::spawn(run(rx, fc, counters, relayed(), cancel));
    tokio::time::timeout(Duration::from_secs(2), task)
        .await
        .expect("a stop fired before the loop parked must still end it")
        .unwrap();
}

#[tokio::test]
async fn the_loop_ends_when_the_socket_channel_closes() {
    let fc = connection();
    let counters = Arc::new(IngestCounters::default());
    let cancel = Shutdown::new();
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
