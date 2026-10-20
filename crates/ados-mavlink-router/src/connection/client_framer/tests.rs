use super::*;

fn heartbeat() -> Vec<u8> {
    use ados_protocol::mavlink::ardupilotmega::{
        MavAutopilot, MavMessage, MavModeFlag, MavState, MavType, HEARTBEAT_DATA,
    };
    let msg = MavMessage::HEARTBEAT(HEARTBEAT_DATA {
        custom_mode: 0,
        mavtype: MavType::MAV_TYPE_GCS,
        autopilot: MavAutopilot::MAV_AUTOPILOT_INVALID,
        base_mode: MavModeFlag::empty(),
        system_status: MavState::MAV_STATE_ACTIVE,
        mavlink_version: 3,
    });
    ados_protocol::mavlink::serialize_v2(
        ados_protocol::mavlink::MavHeader {
            system_id: 255,
            component_id: 190,
            sequence: 0,
        },
        &msg,
    )
    .unwrap()
}

#[test]
fn a_frame_split_across_reads_is_forwarded_whole_once() {
    let frame = heartbeat();
    let mut framer = ClientFramer::default();
    assert!(framer.push(&frame[..7]).is_empty(), "half a frame waits");
    assert_eq!(framer.push(&frame[7..]), vec![frame.clone()]);
}

#[test]
fn many_frames_batch_under_the_write_cap() {
    let frame = heartbeat();
    let mut burst = Vec::new();
    for _ in 0..400 {
        burst.extend_from_slice(&frame);
    }
    let batches = ClientFramer::default().push(&burst);
    assert!(batches.len() > 1);
    assert!(batches.iter().all(|b| b.len() <= FC_MAX_SINGLE_WRITE));
    assert_eq!(batches.iter().map(Vec::len).sum::<usize>(), burst.len());
}

#[test]
fn interleaved_connections_keep_their_partial_frames_apart() {
    let frame = heartbeat();
    let mut framers = ConnFramers::default();
    assert!(framers.push(1, &frame[..5]).is_empty());
    assert!(framers.push(2, &frame[..9]).is_empty());
    assert_eq!(framers.pending(), 2);
    assert_eq!(framers.push(1, &frame[5..]), vec![frame.clone()]);
    assert_eq!(framers.push(2, &frame[9..]), vec![frame.clone()]);
    // A connection holding no partial frame keeps no entry.
    assert_eq!(framers.pending(), 0);
}

#[test]
fn whole_frames_pass_without_an_entry() {
    let frame = heartbeat();
    let mut framers = ConnFramers::default();
    let mut two = frame.clone();
    two.extend_from_slice(&frame);
    assert_eq!(framers.push(7, &two), vec![two.clone()]);
    assert_eq!(framers.pending(), 0);
}

#[test]
fn partial_frames_of_closed_connections_are_bounded() {
    let frame = heartbeat();
    let mut framers = ConnFramers::default();
    for conn in 0..=(MAX_PENDING_CONNS as u64) {
        framers.push(conn, &frame[..4]);
    }
    // Past the cap only the newest writer's partial frame is kept.
    assert_eq!(framers.pending(), 1);
    let last = MAX_PENDING_CONNS as u64;
    assert_eq!(framers.push(last, &frame[4..]), vec![frame.clone()]);
}
