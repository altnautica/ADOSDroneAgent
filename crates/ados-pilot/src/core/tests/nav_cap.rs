use ados_protocol::mavlink::ardupilotmega::MavMessage;
use serde_json::json;

use super::harness::World;
use crate::modes::copter;

const CAP: f32 = 2.0;
const ACCEL: f32 = 1.5;
const TICK_S: f32 = 0.02;

fn ext_nav_world() -> World {
    let mut w = World::new();
    w.p.fc.params.insert("EK3_SRC1_POSXY".into(), 3.0);
    w.p.fc.params.insert("EK3_SRC2_POSXY".into(), 6.0);
    w.airborne(copter::GUIDED);
    w
}

/// vision-nav reports source set 2 (external nav) active.
fn ext_nav(w: &mut World) {
    w.p.on_feed("nav.state", &json!({"active_set": 2, "gps_fix": 0}), w.now);
}

/// Run `ms` with vision-nav reporting external nav every 500 ms.
fn run_ext_nav(w: &mut World, ms: u64) {
    for _ in 0..ms / 500 {
        ext_nav(w);
        w.run(500);
    }
}

/// Run `ms` as `run_ext_nav`, the skill re-sending its goal to 40 m north
/// each time so its lease stays alive.
fn fly_goal_ext_nav(w: &mut World, lease_id: u64, ms: u64) {
    for _ in 0..ms / 500 {
        ext_nav(w);
        w.goal(lease_id, 40.0, 0.0);
        w.run(500);
    }
    assert_eq!(w.p.owner(), ados_protocol::pilot::Owner::Skill);
}

/// Horizontal speed of every SET_POSITION_TARGET_LOCAL_NED sent from `from`.
fn speeds(w: &World, from: usize) -> Vec<f32> {
    w.sent[from..]
        .iter()
        .filter_map(|m| match m {
            MavMessage::SET_POSITION_TARGET_LOCAL_NED(s) => Some(s.vx.hypot(s.vy)),
            _ => None,
        })
        .collect()
}

/// Horizontal distance the FC's position loop adds: the setpoint position
/// away from the vehicle.
fn pos_pull(w: &World) -> f32 {
    let sp = w.last_setpoint().unwrap();
    if sp.type_mask.bits() == crate::fc::MASK_VELOCITY {
        return 0.0;
    }
    (sp.x - w.pos[0]).hypot(sp.y - w.pos[1])
}

#[test]
fn the_harness_flies_uncapped_with_its_3d_fix() {
    let mut w = World::new();
    w.airborne(copter::GUIDED);
    assert!(w.p.nav_cap(w.now).is_none());
}

#[test]
fn goal_speed_is_capped_while_the_active_source_is_external_nav() {
    let mut w = ext_nav_world();
    ext_nav(&mut w);
    let id = w.lease();
    w.goal(id, 40.0, 0.0); // the harness goal asks 5 m/s
    let from = w.sent.len();
    fly_goal_ext_nav(&mut w, id, 3000);
    for v in speeds(&w, from) {
        assert!(v <= CAP + 1e-3, "{v} m/s");
    }
    assert!(pos_pull(&w) < 1e-3, "position pull {} m", pos_pull(&w));
    let cap = w.p.nav_cap(w.now).unwrap();
    assert_eq!(cap.reason, "gnss-denied");
    assert_eq!(cap.v_h_mps, CAP);
    assert_eq!(w.p.state(w.now).speed_cap, Some(cap));
}

#[test]
fn capped_goal_velocity_changes_no_faster_than_the_accel_cap() {
    let mut w = ext_nav_world();
    ext_nav(&mut w);
    let id = w.lease();
    w.goal(id, 40.0, 0.0);
    let from = w.sent.len();
    fly_goal_ext_nav(&mut w, id, 3000);
    let v = speeds(&w, from);
    assert!(v.len() > 100);
    let mut prev = 0.0;
    for s in v {
        assert!(s - prev <= ACCEL * TICK_S + 1e-3, "{prev} -> {s}");
        prev = s;
    }
    assert!((prev - CAP).abs() < 1e-3, "settles at the cap, not {prev}");
}

#[test]
fn gps_source_is_not_capped() {
    let mut w = ext_nav_world();
    w.p.on_feed("nav.state", &json!({"active_set": 1, "gps_fix": 3}), w.now);
    assert!(w.p.nav_cap(w.now).is_none());
    assert!(w.p.state(w.now).speed_cap.is_none());
}

#[test]
fn a_stale_nav_state_keeps_the_last_reported_set() {
    let mut w = ext_nav_world();
    ext_nav(&mut w);
    w.run(5000);
    // The FC keeps the set until told otherwise: the 3-D fix the harness
    // feeds does not lift the cap.
    assert!(w.p.nav_cap(w.now).is_some());
    w.p.on_feed(
        "nav.state",
        &json!({"active_set": null, "gps_fix": 3}),
        w.now,
    );
    assert!(
        w.p.nav_cap(w.now).is_some(),
        "no set named: the last one holds"
    );
    w.p.on_feed("nav.state", &json!({"active_set": 1, "gps_fix": 3}), w.now);
    assert!(w.p.nav_cap(w.now).is_none(), "set 1 is GPS");
}

#[test]
fn a_non_gps_boot_set_is_capped_despite_a_3d_fix() {
    // No nav.state yet: the FC flies source set 1, here external nav.
    let mut w = World::new();
    w.p.fc.params.insert("EK3_SRC1_POSXY".into(), 6.0);
    w.airborne(copter::GUIDED);
    assert!(w.p.nav_cap(w.now).is_some());
}

#[test]
fn a_gps_source_set_without_a_fresh_3d_fix_is_capped() {
    let mut w = World::new();
    w.p.fc.params.insert("EK3_SRC1_POSXY".into(), 3.0);
    w.gps_fix = None;
    w.airborne(copter::GUIDED);
    w.run(3100); // the fix World::new fed goes stale
    assert!(w.p.nav_cap(w.now).is_some());
}

#[test]
fn an_unknown_source_set_is_capped() {
    let mut w = ext_nav_world();
    w.p.on_feed("nav.state", &json!({"active_set": 7, "gps_fix": 3}), w.now);
    assert!(w.p.nav_cap(w.now).is_some());
}

#[test]
fn no_nav_state_and_no_3d_fix_fails_closed_to_the_cap() {
    let mut w = World::new();
    w.gps_fix = None;
    w.airborne(copter::GUIDED);
    w.run(3100); // the fix World::new fed goes stale
    assert_eq!(w.p.nav_cap(w.now).unwrap().reason, "gnss-denied");
    let mut w = World::new();
    w.gps_fix = Some(2);
    w.airborne(copter::GUIDED);
    assert!(w.p.nav_cap(w.now).is_some());
}

#[test]
fn gotos_are_capped_too() {
    let mut w = ext_nav_world();
    ext_nav(&mut w);
    w.action("take_control");
    w.req(ados_protocol::pilot::Request::Goto {
        n: 40.0,
        e: 0.0,
        d: -4.0,
        yaw: None,
    });
    let from = w.sent.len();
    run_ext_nav(&mut w, 3000);
    let v = speeds(&w, from);
    assert!(!v.is_empty());
    for s in v {
        assert!(s <= CAP + 1e-3, "{s} m/s");
    }
    assert!(pos_pull(&w) < 1e-3);
}

#[test]
fn stick_flight_is_capped_too() {
    let mut w = ext_nav_world();
    ext_nav(&mut w);
    w.action("take_control");
    w.stick([0.0; 4], 600); // pass the gate
    let from = w.sent.len();
    for _ in 0..8 {
        ext_nav(&mut w);
        w.stick([0.0, 1.0, 0.0, 0.0], 500);
    }
    let v = speeds(&w, from);
    assert!(v.iter().any(|s| *s > 1.0), "sticks fly: {v:?}");
    for s in v {
        assert!(s <= CAP + 1e-3, "{s} m/s");
    }
}

/// Lease and fly the capped goal 40 m north until its setpoint is at the cap.
fn capped_goal_at_speed(w: &mut World) -> u64 {
    ext_nav(w);
    let id = w.lease();
    fly_goal_ext_nav(w, id, 2000);
    let sp = w.last_setpoint().unwrap();
    assert!((sp.vx - CAP).abs() < 1e-3, "at the cap: {}", sp.vx);
    w.vel = [sp.vx, sp.vy, 0.0];
    id
}

#[test]
fn a_capped_goal_obeys_the_obstacle_brake_at_once() {
    let mut w = ext_nav_world();
    let id = capped_goal_at_speed(&mut w);
    // A wall 3 m ahead and a goal 1.4 m ahead: the plan stays clear, and the
    // brake allows about 1.1 m/s at 2 m/s inside the 1.5 m margin.
    w.p.on_feed(
        "plugin.world-engine.scan",
        &super::safety::wall_scan(3.0),
        w.now,
    );
    w.goal(id, 1.4, 0.0);
    w.run(20);
    let sp = w.last_setpoint().unwrap();
    assert!(sp.vx <= 1.5, "toward the wall at {} m/s", sp.vx);
}

#[test]
fn a_capped_goal_stops_at_once_at_the_geofence() {
    let mut w = ext_nav_world();
    w.p.settings.pilot.max_dist_m = 20.0;
    let id = capped_goal_at_speed(&mut w);
    w.pos[0] = 20.0;
    w.goal(id, 40.0, 0.0);
    w.run(20);
    let sp = w.last_setpoint().unwrap();
    assert!(sp.vx <= 1e-3, "outward at {} m/s", sp.vx);
}

#[test]
fn a_cap_starting_at_speed_commands_no_more_than_the_cap() {
    let mut w = ext_nav_world();
    w.vel = [8.0, 0.0, 0.0];
    ext_nav(&mut w);
    let id = w.lease();
    let from = w.sent.len();
    fly_goal_ext_nav(&mut w, id, 1000);
    let v = speeds(&w, from);
    assert!(!v.is_empty());
    for s in v {
        assert!(s <= CAP + 1e-3, "{s} m/s");
    }
}

#[test]
fn stick_flight_at_speed_drops_to_the_cap_when_it_switches_on() {
    let mut w = World::new();
    w.p.fc.params.insert("EK3_SRC1_POSXY".into(), 3.0);
    w.p.fc.params.insert("EK3_SRC2_POSXY".into(), 6.0);
    // The brake off, so only the cap can hold the sticks to 2 m/s.
    w.p.settings.pilot.avoid = ados_protocol::pilot::AvoidMode::Off;
    w.airborne(copter::GUIDED);
    w.action("take_control");
    w.stick([0.0; 4], 600); // pass the gate
                            // Full forward on GPS (source set 1): the stick model reaches 8 m/s.
    w.stick([0.0, 1.0, 0.0, 0.0], 5000);
    let sp = w.last_setpoint().unwrap();
    assert!(sp.vx > 7.5, "uncapped stick flight at {} m/s", sp.vx);
    // The EKF moves to external nav mid-flight.
    ext_nav(&mut w);
    let from = w.sent.len();
    w.stick([0.0, 1.0, 0.0, 0.0], 500);
    let v = speeds(&w, from);
    assert!(!v.is_empty());
    for s in v {
        assert!(s <= CAP + 1e-3, "{s} m/s");
    }
}

/// Fly forward at the cap, then centre the sticks (a STOP) and return the
/// commanded speeds of the slow-down.
fn capped_stop(w: &mut World) -> Vec<f32> {
    w.action("take_control");
    w.stick([0.0; 4], 600); // pass the gate
    for _ in 0..8 {
        ext_nav(w);
        w.stick([0.0, 1.0, 0.0, 0.0], 500);
    }
    let from = w.sent.len();
    for _ in 0..4 {
        ext_nav(w);
        w.stick([0.0; 4], 500);
    }
    speeds(w, from)
}

#[test]
fn a_stop_under_the_cap_slows_at_the_profiles_acceleration() {
    let mut w = ext_nav_world();
    w.p.settings.pilot.avoid = ados_protocol::pilot::AvoidMode::Off;
    ext_nav(&mut w);
    let v = capped_stop(&mut w);
    assert!(v[0] > CAP - 0.2, "at the cap before the stop: {v:?}");
    let profile = crate::model::limits(w.p.settings.pilot.profile).a_h;
    assert!(
        profile > ACCEL,
        "the test needs a profile faster than the cap"
    );
    let decel = v
        .windows(2)
        // The last step to the position hold drops to zero at once.
        .filter(|p| p[1] > crate::model::HOLD_CAPTURE_MPS)
        .map(|p| (p[0] - p[1]) / TICK_S)
        .fold(0.0f32, f32::max);
    assert!(
        decel > ACCEL + 0.25,
        "slow-down limited to the cap's {ACCEL} m/s²: peak {decel} m/s²"
    );
    assert!(decel <= profile + 0.05, "{decel} m/s²");
}

#[test]
fn the_cap_is_shown_only_while_the_pilot_flies_under_it() {
    // On the ground: nothing flies, nothing is capped.
    let mut w = World::new();
    w.p.fc.params.insert("EK3_SRC1_POSXY".into(), 3.0);
    w.p.fc.params.insert("EK3_SRC2_POSXY".into(), 6.0);
    ext_nav(&mut w);
    w.run(200);
    assert!(w.p.nav_capped(w.now));
    assert!(w.p.state(w.now).speed_cap.is_none(), "on the ground");

    // In the air under the pilot's stick flight: shown.
    w.airborne(copter::GUIDED);
    run_ext_nav(&mut w, 1000);
    w.action("take_control");
    w.stick([0.0; 4], 600);
    assert!(w.p.state(w.now).speed_cap.is_some(), "pilot flight");

    // The FC unheard: the pilot sends nothing it could cap.
    assert!(w.p.state(w.now + 3000).speed_cap.is_none(), "FC unheard");

    // An FC RTL flies at the FC's own speeds.
    w.mode = copter::RTL;
    run_ext_nav(&mut w, 1000);
    assert!(w.p.state(w.now).speed_cap.is_none(), "FC RTL");

    // An FC mission likewise.
    w.mode = copter::AUTO;
    run_ext_nav(&mut w, 1000);
    assert!(w.p.state(w.now).speed_cap.is_none(), "FC mission");
}
