//! A lidar that keeps scanning but returns nothing. A scan's arrival says the
//! lidar is scanning, never that it sees anything: an all-empty scan is open
//! air, or a blocked window, a failed receiver or a mount facing the sky. It
//! reads `no_returns` and warns, and where open air is not believable (a
//! confined place, or in flight while the surface last seen must still be in
//! the scan) an empty scan is not obstacle data: the last obstacles stay and
//! age to stale, and the pilot holds.

use ados_protocol::pilot::PlaceClass;
use serde_json::Value;

use super::harness::{surface_behind, World};
use crate::modes::copter;

fn avoid_state(w: &World) -> String {
    match serde_json::to_value(w.p.state(w.now).avoid_state).unwrap() {
        Value::String(s) => s,
        other => panic!("{other:?}"),
    }
}

fn lidar_state(w: &World) -> Value {
    let s = w.p.state(w.now).sensors.expect("sensors with a link");
    serde_json::to_value(s.lidar.state).unwrap()
}

/// The `lidar` readiness item as (ok, severity, msg).
fn lidar_item(w: &World) -> Option<(bool, Value, String)> {
    w.p.state(w.now)
        .readiness
        .into_iter()
        .find(|r| r.id == "lidar")
        .map(|r| (r.ok, serde_json::to_value(r.severity).unwrap(), r.msg))
}

fn spec_text(path: &[&str]) -> Value {
    let spec: Value = serde_json::from_str(crate::sensor_spec::SPEC_JSON).unwrap();
    path.iter().fold(spec, |v, k| v[*k].clone())
}

/// One return, 8 m behind: a surface the lidar sees, far from the flight.
fn one_surface() -> Vec<f32> {
    surface_behind(8.0)
}

#[test]
fn scans_without_a_return_read_no_returns_and_warn() {
    let mut w = World::new();
    w.run(1500);
    assert_eq!(
        lidar_state(&w),
        "no_returns",
        "arrivals alone are not health"
    );
    let (ok, severity, msg) = lidar_item(&w).expect("the lidar item");
    assert!(!ok);
    assert_eq!(severity, "warn");
    assert_eq!(
        Value::String(msg),
        spec_text(&["readiness", "lidar", "no_returns"])
    );
    // Open air on the ground: a warning only.
    assert_eq!(avoid_state(&w), "clear");
    // One return clears it at once.
    w.scan_ranges = one_surface();
    w.run(200);
    assert_eq!(lidar_state(&w), "ok");
    assert_eq!(lidar_item(&w).map(|i| i.0), Some(true));
}

#[test]
fn open_air_with_nothing_seen_since_arming_only_warns() {
    let mut w = World::new();
    // A surface before arming does not count for this flight.
    w.scan_ranges = one_surface();
    w.run(500);
    w.scan_ranges = vec![0.0; 360];
    w.airborne(copter::GUIDED);
    w.run(2000);
    assert_eq!(lidar_state(&w), "no_returns");
    assert_eq!(avoid_state(&w), "clear");
}

#[test]
fn nothing_where_a_surface_should_still_be_holds_as_stale() {
    let mut w = World::new();
    w.airborne(copter::GUIDED);
    w.scan_ranges = one_surface();
    w.run(1000);
    assert_eq!(avoid_state(&w), "clear");
    // The returns stop while scans keep arriving, the surface 8 m away still
    // in reach: the empty scans never read as free space, the last obstacle
    // stays, and once it is older than the scan age limit the data is stale.
    w.scan_ranges = vec![0.0; 360];
    w.run(300);
    let kept = w.p.obstacles.nearest_hit_m();
    assert!(
        kept.is_some_and(|r| (r - 8.0).abs() < 0.01),
        "the wall stays: {kept:?}"
    );
    assert_eq!(avoid_state(&w), "clear");
    w.run(400);
    assert_eq!(avoid_state(&w), "stale");
    w.run(400);
    assert_eq!(lidar_state(&w), "no_returns");
    assert_eq!(avoid_state(&w), "stale");
    // A return again: flying resumes.
    w.scan_ranges = one_surface();
    w.run(300);
    assert_eq!(avoid_state(&w), "clear");
}

#[test]
fn nothing_after_leaving_the_surface_behind_is_open_air() {
    // Flown 4 m on from a surface 8 m away: 12 m planar reach, so it may be
    // out of the scan; an empty scan only warns.
    let mut w = World::new();
    w.airborne(copter::GUIDED);
    w.scan_ranges = one_surface();
    w.run(1000);
    w.scan_ranges = vec![0.0; 360];
    w.pos[0] += 4.0;
    w.run(1500);
    assert_eq!(lidar_state(&w), "no_returns");
    assert_eq!(avoid_state(&w), "clear");
    assert_eq!(w.p.obstacles.nearest_hit_m(), None);
    // Climbed out of the scan plane above it: the same.
    let mut w = World::new();
    w.airborne(copter::GUIDED);
    w.scan_ranges = one_surface();
    w.run(1000);
    w.scan_ranges = vec![0.0; 360];
    w.pos[2] -= 1.0;
    w.run(1500);
    assert_eq!(avoid_state(&w), "clear");
}

#[test]
fn a_tilt_that_lifts_the_scan_off_the_surface_is_open_air() {
    // Pitched 10° in place: at 8 m the scan plane moves 1.4 m, so the empty
    // scan may well be over a low surface; it only warns, never freezes.
    let mut w = World::new();
    w.airborne(copter::GUIDED);
    w.scan_ranges = one_surface();
    w.run(1000);
    w.scan_ranges = vec![0.0; 360];
    w.pitch = 10f32.to_radians();
    w.run(1500);
    assert_eq!(lidar_state(&w), "no_returns");
    assert_eq!(avoid_state(&w), "clear");
    // Level again, in place, still empty: the surface must be there.
    w.pitch = 0.0;
    w.scan_ranges = one_surface();
    w.run(300);
    w.scan_ranges = vec![0.0; 360];
    w.run(700);
    assert_eq!(avoid_state(&w), "stale");
}

#[test]
fn a_confined_place_holds_on_scans_without_a_return() {
    let mut w = World::new();
    w.p.settings.pilot.place_class = PlaceClass::Confined;
    w.airborne(copter::GUIDED);
    w.run(1500);
    assert_eq!(lidar_state(&w), "no_returns");
    assert_eq!(avoid_state(&w), "stale");
    w.scan_ranges = one_surface();
    w.run(300);
    assert_eq!(avoid_state(&w), "clear");
}

#[test]
fn one_stray_return_in_open_air_is_not_a_surface() {
    // Rain, dust or an insect: one bin in one scan, then empty scans.
    let mut w = World::new();
    w.airborne(copter::GUIDED);
    w.run(500);
    let stray = serde_json::json!({"frame": "body_frd", "angle_start_rad": 0.0,
        "angle_increment_rad": std::f32::consts::TAU / 360.0,
        "ranges_m": surface_behind(1.5)});
    w.p.on_feed(crate::events::TOPIC_SCAN, &stray, w.now);
    w.run(1500);
    assert_eq!(lidar_state(&w), "no_returns");
    assert_eq!(avoid_state(&w), "clear");
}

#[test]
fn something_small_that_leaves_the_scan_is_not_a_surface() {
    // A person or another drone 3 m ahead for a second, then gone from the
    // scan plane: too narrow to say the lidar went blind, so it stays open
    // air and autonomy keeps flying.
    let mut w = World::new();
    w.airborne(copter::GUIDED);
    w.scan_ranges = super::harness::one_return(0, 3.0);
    w.run(1000);
    w.scan_ranges = vec![0.0; 360];
    w.run(1500);
    assert_eq!(lidar_state(&w), "no_returns");
    assert_eq!(avoid_state(&w), "clear");
    assert_eq!(w.p.lidar.surface(), None);
}

#[test]
fn two_small_things_apart_are_not_one_wide_surface() {
    // Two neighbouring drones 4 m away, 2.5 m apart (36° at 4 m), hovering
    // for a second, then both leave the scan plane: still open air.
    let mut w = World::new();
    w.airborne(copter::GUIDED);
    let mut two = vec![0.0; 360];
    two[0] = 4.0;
    two[36] = 4.0;
    w.scan_ranges = two;
    w.run(1000);
    w.scan_ranges = vec![0.0; 360];
    w.run(1500);
    assert_eq!(avoid_state(&w), "clear");
    assert_eq!(w.p.lidar.surface(), None);
    // A wall across the same bearings (36° at 4 m, 2.5 m long) is one
    // surface, and holds.
    let mut w = World::new();
    w.airborne(copter::GUIDED);
    w.scan_ranges = (0..360).map(|b| if b <= 36 { 4.0 } else { 0.0 }).collect();
    w.run(1000);
    w.scan_ranges = vec![0.0; 360];
    w.run(700);
    assert_eq!(avoid_state(&w), "stale");
}

#[test]
fn the_vertical_stick_climbs_out_of_an_empty_scan_hold() {
    // Held stale because the surface's returns stopped: horizontal sticks
    // ask nothing, but a climb is flown, and once out of the scan plane the
    // empty scans are open air again.
    let mut w = World::new();
    w.airborne(copter::GUIDED);
    w.action("take_control");
    w.scan_ranges = one_surface();
    w.stick([0.0; 4], 1000);
    w.scan_ranges = vec![0.0; 360];
    w.stick([0.0; 4], 700);
    assert_eq!(avoid_state(&w), "stale");
    w.stick([0.0, 1.0, 1.0, 0.0], 1000);
    let sp = w.last_setpoint().expect("a setpoint");
    assert!(sp.vz < -0.1, "climb not flown: vz {}", sp.vz);
    assert!(
        sp.vx.abs() < 0.05 && sp.vy.abs() < 0.05,
        "forward flown blind"
    );
    assert_eq!(avoid_state(&w), "stale");
    w.pos[2] -= 0.6;
    w.stick([0.0, 0.0, 1.0, 0.0], 300);
    assert_eq!(avoid_state(&w), "clear");
}
