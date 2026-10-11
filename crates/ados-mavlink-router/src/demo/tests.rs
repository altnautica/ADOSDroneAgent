use super::*;
use crate::state::VehicleState;

const TS: &str = "2026-01-01T00:00:00Z";

#[test]
fn sample_at_zero_matches_demo_seed() {
    let s = sample_at(0.0);
    // angle 0 -> cos 1, sin 0
    assert!((s.lat - (CENTER_LAT + CIRCLE_RADIUS)).abs() < 1e-12);
    assert!((s.lon - CENTER_LON).abs() < 1e-12);
    // alt_rel = 50 + 3*sin(0) = 50, alt_msl = 50 + 920
    assert!((s.alt_rel - 50.0).abs() < 1e-12);
    assert!((s.alt_msl - 970.0).abs() < 1e-12);
    // heading = degrees(pi/2) = 90
    assert!((s.heading - 90.0).abs() < 1e-9);
    // battery starts at 95% -> 25.2 * 0.95
    assert_eq!(s.battery_remaining, 95);
    assert!((s.voltage - 25.2 * 0.95).abs() < 1e-9);
    assert_eq!(s.throttle, 45);
}

/// The MAVLink message id for each demo message variant, used by the test
/// without depending on the dialect's `Message` trait being in scope.
fn variant_id(m: &MavMessage) -> u32 {
    match m {
        MavMessage::HEARTBEAT(_) => 0,
        MavMessage::SYS_STATUS(_) => 1,
        MavMessage::GPS_RAW_INT(_) => 24,
        MavMessage::ATTITUDE(_) => 30,
        MavMessage::GLOBAL_POSITION_INT(_) => 33,
        MavMessage::RC_CHANNELS(_) => 65,
        MavMessage::VFR_HUD(_) => 74,
        MavMessage::BATTERY_STATUS(_) => 147,
        _ => u32::MAX,
    }
}

#[test]
fn demo_builds_eight_distinct_messages() {
    let msgs = demo_messages(3.0);
    assert_eq!(msgs.len(), 8);
    // The message-id set must be exactly the eight telemetry types.
    let ids: std::collections::BTreeSet<u32> = msgs.iter().map(variant_id).collect();
    let expected: std::collections::BTreeSet<u32> =
        [0, 1, 24, 30, 33, 65, 74, 147].into_iter().collect();
    assert_eq!(ids, expected);
}

#[test]
fn decoded_messages_reproduce_the_sample_within_scaling() {
    // Feeding the built messages through the normal decode path must
    // reproduce the flight-state targets (within each message's integer
    // scaling), proving the demo source drives the same snapshot a real FC
    // would.
    let t = 7.5;
    let s = sample_at(t);
    let mut st = VehicleState::default();
    for msg in demo_messages(t) {
        st.update_from_message(&msg, TS);
    }
    assert_eq!(st.mode, "LOITER");
    assert!(st.armed);
    assert_eq!(st.mav_type, MavType::MAV_TYPE_QUADROTOR as i64);
    // lat/lon scale through 1e7 fixed-point.
    assert!((st.lat - s.lat).abs() < 1e-6, "lat {} vs {}", st.lat, s.lat);
    assert!((st.lon - s.lon).abs() < 1e-6, "lon {} vs {}", st.lon, s.lon);
    // alt scales through millimetres.
    assert!((st.alt_rel - s.alt_rel).abs() < 1e-2);
    assert!((st.alt_msl - s.alt_msl).abs() < 1e-2);
    // heading through centidegrees.
    assert!((st.heading - s.heading).abs() < 0.02);
    // velocity through cm/s.
    assert!((st.vz - s.vz).abs() < 0.02);
    assert!((st.climb - s.climb).abs() < 0.02);
    // battery from SYS_STATUS (mV / cA), temperature from BATTERY_STATUS.
    assert_eq!(st.battery_remaining, Some(s.battery_remaining as i64));
    assert!((st.voltage_battery.unwrap() - s.voltage).abs() < 0.01);
    assert!((st.current_battery.unwrap() - s.current).abs() < 0.01);
    assert!((st.battery_temperature.unwrap() - s.battery_temperature).abs() < 0.01);
    // BATTERY_STATUS cells were all sentinel, so the per-cell list is empty.
    assert!(st.battery_voltages.is_empty());
    // RC + throttle.
    assert_eq!(st.rc_channels.len(), 18);
    assert_eq!(st.rc_channels[0], 1500);
    assert_eq!(st.rc_rssi, Some(200));
    assert_eq!(st.throttle, s.throttle as i64);
    assert_eq!(st.gps_satellites, 14);
    assert_eq!(st.gps_fix_type, GpsFixType::GPS_FIX_TYPE_3D_FIX as i64);
}
