use super::*;
use ados_protocol::mavlink::ardupilotmega::{
    MavAutopilot, MavModeFlag, MavState, MavType, ATTITUDE_DATA, HEARTBEAT_DATA,
};
use ados_protocol::mavlink::{serialize_v2, MavHeader, MavMessage};

const TS: &str = "2026-07-31T17:00:00+00:00";

fn header(system_id: u8) -> MavHeader {
    MavHeader {
        system_id,
        component_id: 1,
        sequence: 0,
    }
}

fn attitude_frame(system_id: u8, roll: f32, pitch: f32) -> Vec<u8> {
    let msg = MavMessage::ATTITUDE(ATTITUDE_DATA {
        time_boot_ms: 0,
        roll,
        pitch,
        yaw: 0.5,
        rollspeed: 0.0,
        pitchspeed: 0.0,
        yawspeed: 0.0,
    });
    serialize_v2(header(system_id), &msg).unwrap()
}

fn heartbeat_frame(system_id: u8) -> Vec<u8> {
    let msg = MavMessage::HEARTBEAT(HEARTBEAT_DATA {
        custom_mode: 0,
        mavtype: MavType::MAV_TYPE_QUADROTOR,
        autopilot: MavAutopilot::MAV_AUTOPILOT_ARDUPILOTMEGA,
        base_mode: MavModeFlag::empty(),
        system_status: MavState::MAV_STATE_STANDBY,
        mavlink_version: 3,
    });
    serialize_v2(header(system_id), &msg).unwrap()
}

/// A ground station's HEARTBEAT as the drone's flight controller forwards
/// it down the tee: its own system id, no autopilot, base mode 0.
fn gcs_heartbeat_frame() -> Vec<u8> {
    let msg = MavMessage::HEARTBEAT(HEARTBEAT_DATA {
        custom_mode: 0,
        mavtype: MavType::MAV_TYPE_GCS,
        autopilot: MavAutopilot::MAV_AUTOPILOT_INVALID,
        base_mode: MavModeFlag::empty(),
        system_status: MavState::MAV_STATE_ACTIVE,
        mavlink_version: 3,
    });
    let header = MavHeader {
        system_id: 255,
        component_id: 190,
        sequence: 0,
    };
    serialize_v2(header, &msg).unwrap()
}

#[test]
fn a_relayed_attitude_frame_becomes_readable_vehicle_state() {
    // The whole point: frames that were previously published and forgotten
    // now produce a reading a surface can render.
    let mut v = RelayedVehicle::default();
    let now = Instant::now();

    assert!(v.apply_frame(&heartbeat_frame(1), TS, now));
    assert!(v.apply_frame(&attitude_frame(1, 0.25, -0.1), TS, now));

    let wire = v.to_wire(now).expect("a relayed vehicle has a snapshot");
    assert_eq!(wire["source"], "relayed");
    assert_eq!(wire["fresh"], true);
    assert_eq!(wire["system_id"], 1);
    assert_eq!(wire["frames_decoded"], 2);
    let att = &wire["vehicle"]["attitude"];
    assert!((att["roll"].as_f64().unwrap() - 0.25).abs() < 1e-6);
    assert!((att["pitch"].as_f64().unwrap() + 0.1).abs() < 1e-6);
    // The timestamp is this node's, which is what a consumer compares
    // against its own clock.
    assert_eq!(wire["vehicle"]["last_update"], TS);
}

#[test]
fn only_the_vehicle_feeds_the_relayed_reading() {
    // The drone's tee forwards whatever its flight controller emits,
    // including a ground station's HEARTBEAT arriving on the drone's own
    // telemetry radio. It must not disarm the relayed vehicle or replace
    // its system id, and telemetry before the vehicle's HEARTBEAT has no
    // vehicle to belong to.
    let mut v = RelayedVehicle::default();
    let now = Instant::now();
    assert!(!v.apply_frame(&attitude_frame(1, 0.25, -0.1), TS, now));
    assert!(v.apply_frame(&heartbeat_frame(7), TS, now));
    assert!(!v.apply_frame(&gcs_heartbeat_frame(), TS, now));
    assert!(!v.apply_frame(&attitude_frame(3, 0.9, 0.9), TS, now));

    let wire = v.to_wire(now).unwrap();
    assert_eq!(wire["system_id"], 7);
    assert_eq!(wire["frames_decoded"], 1);
    assert_eq!(
        wire["vehicle"]["mav_type"],
        MavType::MAV_TYPE_QUADROTOR as i64
    );
    assert_eq!(wire["vehicle"]["attitude"]["roll"], 0.0);
}

#[test]
fn the_wire_shape_matches_what_the_control_surface_reads() {
    // This is a cross-crate contract with no compiler between its halves:
    // `ados-control`'s `routes/status.rs` reaches into this snapshot by key
    // name to decide whether to project the reading and what provenance to
    // stamp on it. Renaming a key here would silently stop the projection —
    // the route would find nothing, withhold the vehicle fields exactly as
    // it did before this existed, and the cockpit would go back to a dead
    // horizon with no test failing anywhere. So the names are pinned, and
    // this test is the thing that fails instead.
    let mut v = RelayedVehicle::default();
    let now = Instant::now();
    v.apply_frame(&heartbeat_frame(1), TS, now);
    v.apply_frame(&attitude_frame(1, 0.25, -0.1), TS, now);
    let wire = v.to_wire(now).unwrap();
    let obj = wire.as_object().unwrap();

    // Read by `project_telemetry` to gate and stamp the projection.
    assert!(obj.contains_key("fresh"), "the freshness gate reads this");
    assert!(
        obj.contains_key("vehicle"),
        "the projected fields live here"
    );
    // Copied verbatim into the `relayed_link` provenance summary.
    for key in [
        "age_s",
        "stale_after_s",
        "system_id",
        "frames_decoded",
        "frames_undecodable",
    ] {
        assert!(obj.contains_key(key), "{key} rides the provenance summary");
    }
    // The vehicle block must be the aggregator's own wire shape, because
    // the route lifts its keys straight up into the telemetry body.
    let vehicle = obj["vehicle"].as_object().unwrap();
    for key in ["attitude", "position", "battery", "gps", "mode", "armed"] {
        assert!(vehicle.contains_key(key), "{key} is part of the HUD's read");
    }
}

#[test]
fn a_node_that_has_never_relayed_a_vehicle_has_no_snapshot_at_all() {
    // Absent, not present-and-zeroed: a zeroed attitude is a lie that reads
    // as a level aircraft.
    let v = RelayedVehicle::default();
    assert!(v.to_wire(Instant::now()).is_none());
    assert!(v.age(Instant::now()).is_none());
    assert!(!v.is_fresh(Instant::now()));
}

#[test]
fn a_quiet_lane_goes_stale_rather_than_holding_its_last_reading() {
    let mut v = RelayedVehicle::default();
    let start = Instant::now();
    v.apply_frame(&heartbeat_frame(1), TS, start);

    assert!(v.is_fresh(start + Duration::from_millis(500)));
    let past = start + RELAYED_STALE_AFTER + Duration::from_millis(1);
    assert!(!v.is_fresh(past));

    // The snapshot still exists (the vehicle was seen) but says so honestly.
    let wire = v.to_wire(past).unwrap();
    assert_eq!(wire["fresh"], false);
    assert!(wire["age_s"].as_f64().unwrap() >= RELAYED_STALE_AFTER.as_secs_f64());
}

#[test]
fn a_heartbeat_carries_mode_and_arming_through() {
    let mut v = RelayedVehicle::default();
    let now = Instant::now();
    v.apply_frame(&heartbeat_frame(7), TS, now);

    let wire = v.to_wire(now).unwrap();
    assert_eq!(wire["system_id"], 7);
    assert_eq!(wire["vehicle"]["armed"], false);
    assert_eq!(wire["vehicle"]["mode"], "STABILIZE");
    assert_eq!(wire["vehicle"]["last_heartbeat"], TS);
}

#[test]
fn a_message_this_projection_does_not_read_is_skipped_not_counted_as_broken() {
    // An unread message id must not inflate the undecodable counter, or a
    // healthy lane reads as a corrupt one.
    let mut v = RelayedVehicle::default();
    let now = Instant::now();
    // Message id 22 (PARAM_VALUE) is deliberately excluded.
    let mut frame = heartbeat_frame(1);
    frame[7] = 22;

    assert!(!v.apply_frame(&frame, TS, now));
    assert_eq!(v.frames_undecodable(), 0);
    assert_eq!(v.frames_decoded(), 0);
    assert!(v.to_wire(now).is_none());
}

#[test]
fn a_corrupt_frame_of_a_read_id_is_counted_as_undecodable() {
    // Truncating a frame whose id we DO read is the real corruption signal.
    let mut v = RelayedVehicle::default();
    let now = Instant::now();
    let full = attitude_frame(1, 0.1, 0.1);
    let truncated = &full[..full.len() - 3];
    assert!(v.apply_frame(&heartbeat_frame(1), TS, now));

    assert!(!v.apply_frame(truncated, TS, now));
    assert_eq!(v.frames_undecodable(), 1);
    assert_eq!(v.frames_decoded(), 1);
    // One bad frame does not latch the lane shut.
    assert!(v.apply_frame(&full, TS, now));
    assert_eq!(v.frames_decoded(), 2);
}

#[test]
fn garbage_never_reaches_the_parser() {
    let mut v = RelayedVehicle::default();
    let now = Instant::now();
    assert!(!v.apply_frame(b"not a frame", TS, now));
    assert!(!v.apply_frame(&[], TS, now));
    assert_eq!(v.frames_undecodable(), 0);
}
