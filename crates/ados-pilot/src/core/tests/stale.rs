//! Obstacle data that stops arriving in flight with avoid on: the pilot
//! treats it as unknown, never clear. It holds, reports `stale` and flies no
//! motion command until the scans are fresh again; STOP, Pause, Land and RTL
//! still act, and only `avoid = off` flies without scans. A vehicle that never
//! saw a scan has no obstacle sensor: it reports `unavailable`, sticks fly,
//! Fly Here is refused and a pre-arm warning names it.

use ados_protocol::pilot::{AvoidMode, Owner, Request};
use serde_json::{json, Value};

use super::harness::{World, FC_COMP, FC_SYS};
use crate::core::Reply;
use crate::fc::{MASK_POS_VEL_YAW, MASK_VELOCITY};
use crate::modes::copter;

/// The `hold_reason` wire string while obstacle data is stale.
const STALE: &str = "obstacle-data-stale";

fn avoid_state(w: &World) -> String {
    let s = w.p.state(w.now);
    match serde_json::to_value(s.avoid_state).unwrap() {
        Value::String(s) => s,
        other => panic!("{other:?}"),
    }
}

fn hold_reason(w: &World) -> Option<String> {
    w.p.state(w.now).hold_reason
}

fn err(r: &Reply) -> Option<String> {
    match r {
        Reply::Action(a) => a.err.clone(),
        other => panic!("{other:?}"),
    }
}

/// Horizontal speed of the last setpoint, m/s.
fn sp_speed(w: &World) -> f32 {
    let sp = w.last_setpoint().unwrap();
    sp.vx.hypot(sp.vy)
}

/// The lidar stops; the last scan goes stale (older than 500 ms).
fn lidar_stops(w: &mut World) {
    w.lidar = false;
    w.run(700);
}

fn home(w: &mut World) {
    use ados_protocol::mavlink::ardupilotmega::{MavMessage, HOME_POSITION_DATA};
    let m = MavMessage::HOME_POSITION(HOME_POSITION_DATA {
        latitude: 120_000_000,
        longitude: 770_000_000,
        ..Default::default()
    });
    w.p.on_fc(FC_SYS, FC_COMP, &m, w.now);
}

#[test]
fn stale_scans_mid_stick_flight_hold_and_report_stale() {
    let mut w = World::new();
    w.airborne(copter::GUIDED);
    w.action("take_control");
    w.stick([0.0; 4], 600);
    let fwd = [0.0, 1.0, 0.0, 0.0];
    w.stick(fwd, 1500);
    assert!(sp_speed(&w) > 1.0, "flies on fresh scans: {}", sp_speed(&w));
    assert_eq!(avoid_state(&w), "clear");
    // The stick stays forward while the lidar is silent: once the scan is
    // 500 ms old the ask drops to nothing and the shaper slows to a hold.
    w.lidar = false;
    w.stick(fwd, 1000);
    assert!(
        sp_speed(&w) < 7.5,
        "no slowing once stale: {}",
        sp_speed(&w)
    );
    assert_eq!(avoid_state(&w), "stale");
    w.stick(fwd, 3500);
    let sp = w.last_setpoint().unwrap();
    assert!(
        sp.type_mask.bits() == MASK_POS_VEL_YAW || sp_speed(&w) < 0.05,
        "still flying blind at {} m/s",
        sp_speed(&w)
    );
    assert_eq!(avoid_state(&w), "stale");
    assert_eq!(hold_reason(&w).as_deref(), Some(STALE));
    assert_eq!(w.p.owner(), Owner::Pilot);
    // Fresh scans again: the sticks fly and the hold reason clears.
    w.lidar = true;
    w.stick(fwd, 1500);
    assert!(sp_speed(&w) > 1.0, "sticks dead after recovery");
    assert_eq!(avoid_state(&w), "clear");
    assert_eq!(hold_reason(&w), None);
}

#[test]
fn stale_scans_mid_goto_hold_where_the_vehicle_is() {
    let mut w = World::new();
    w.airborne(copter::GUIDED);
    w.req(Request::Goto {
        n: 20.0,
        e: 0.0,
        d: -5.0,
        yaw: None,
    });
    w.run(600);
    assert_eq!(w.p.owner(), Owner::Goto);
    assert!(w.last_setpoint().unwrap().vx > 0.5, "goto not flown");
    w.pos = [2.0, 0.0, -5.0];
    lidar_stops(&mut w);
    let sp = w.last_setpoint().unwrap().clone();
    assert_eq!(sp.type_mask.bits(), MASK_POS_VEL_YAW, "not a hold");
    assert!(
        (sp.x - 2.0).abs() < 0.3 && sp.y.abs() < 0.3,
        "flies toward the goal blind: {} {}",
        sp.x,
        sp.y
    );
    assert!(
        sp.vx.hypot(sp.vy) < 1e-3,
        "hold with a push: {} {}",
        sp.vx,
        sp.vy
    );
    assert_eq!(avoid_state(&w), "stale");
    assert_eq!(hold_reason(&w).as_deref(), Some(STALE));
    // Fresh scans: the goto goes on.
    w.lidar = true;
    w.run(600);
    assert_eq!(w.p.owner(), Owner::Goto);
    assert!(w.last_setpoint().unwrap().vx > 0.5, "goto not resumed");
    assert_eq!(hold_reason(&w), None);
}

#[test]
fn stale_scans_refuse_a_new_goto() {
    let mut w = World::new();
    home(&mut w);
    w.airborne(copter::GUIDED);
    w.action("take_control");
    lidar_stops(&mut w);
    let point = json!({"lat": 12.0009, "lon": 77.0, "alt_m": 6.0});
    assert_eq!(
        err(&w.action_args("goto", point.clone())).as_deref(),
        Some("obstacle_data_stale")
    );
    w.req(Request::Goto {
        n: 20.0,
        e: 0.0,
        d: -5.0,
        yaw: None,
    });
    w.run(200);
    assert_eq!(w.p.owner(), Owner::Pilot, "a redirected goto started blind");
    assert!(sp_speed(&w) < 1e-3);
    w.lidar = true;
    w.run(200);
    assert_eq!(err(&w.action_args("goto", point)), None);
}

#[test]
fn stale_scans_hold_a_skill_goal_and_refuse_swarm_goals() {
    let mut w = World::new();
    w.airborne(copter::GUIDED);
    let id = w.lease();
    w.goal(id, 10.0, 0.0);
    w.run(400);
    assert!(w.last_setpoint().unwrap().vx > 0.5, "goal not flown");
    w.pos = [1.0, 0.0, -5.0];
    // The skill keeps streaming goals while the lidar is silent.
    w.lidar = false;
    for _ in 0..9 {
        w.goal(id, 12.0, 0.0);
        w.run(100);
    }
    let sp = w.last_setpoint().unwrap().clone();
    assert_eq!(w.p.owner(), Owner::Skill);
    assert!(
        (sp.x - 1.0).abs() < 0.3 && sp.vx.hypot(sp.vy) < 1e-3,
        "skill goal flown blind: x {} vx {}",
        sp.x,
        sp.vx
    );
    assert_eq!(avoid_state(&w), "stale");
    assert_eq!(hold_reason(&w).as_deref(), Some(STALE));

    let mut w = World::new();
    w.airborne(copter::GUIDED);
    w.action("take_control");
    w.stick([0.0; 4], 600);
    lidar_stops(&mut w);
    for _ in 0..10 {
        w.req(Request::SwarmGoal {
            vn: 2.0,
            ve: 0.0,
            vd: 0.0,
            yaw: None,
        });
        w.run(20);
    }
    assert_ne!(w.p.owner(), Owner::Swarm);
    assert!(sp_speed(&w) < 1e-3, "swarm flown blind: {}", sp_speed(&w));
    assert_eq!(hold_reason(&w).as_deref(), Some(STALE));
}

/// A vantage goal plans over the obstacle map, so stale scans hold it like
/// any skill goal: no point is chosen over obstacles the pilot can no
/// longer see, and the holder hears no vantage.
#[test]
fn stale_scans_hold_a_vantage_goal() {
    use ados_protocol::pilot::VantageState;
    let mut w = World::new();
    w.airborne(copter::GUIDED);
    let id = w.lease();
    w.hold_vantage(id, [20.0, 0.0], [0.0, 0.0], 600);
    assert_ne!(w.goal_status.last().unwrap().state, VantageState::NoVantage);
    w.pos = [1.0, 0.0, -5.0];
    w.lidar = false;
    w.hold_vantage(id, [20.0, 0.0], [0.0, 0.0], 1000);
    let sp = w.last_setpoint().unwrap().clone();
    assert_eq!(w.p.owner(), Owner::Skill);
    assert!(
        (sp.x - 1.0).abs() < 0.3 && sp.vx.hypot(sp.vy) < 1e-3,
        "vantage flown blind: x {} vx {}",
        sp.x,
        sp.vx
    );
    assert_eq!(avoid_state(&w), "stale");
    assert_eq!(hold_reason(&w).as_deref(), Some(STALE));
    let st = w.goal_status.last().unwrap();
    assert_eq!(st.state, VantageState::NoVantage);
    assert_eq!(st.vantage_ne, None);
    assert_eq!(st.coverage, 0.0);
}

/// The vantage goal's last status and the vehicle's horizontal setpoint
/// speed after holding it with no scan ever seen.
fn vantage_without_scans(mut w: World) -> World {
    w.airborne(copter::GUIDED);
    let id = w.lease();
    w.hold_vantage(id, [7.0, 0.0], [0.0, 0.0], 1000);
    w
}

fn assert_no_vantage_held(w: &World) {
    use ados_protocol::pilot::VantageState;
    let st = w.goal_status.last().expect("a goal status");
    assert_eq!(st.state, VantageState::NoVantage, "{st:?}");
    assert_eq!(st.vantage_ne, None);
    assert_eq!(st.coverage, 0.0, "coverage from an empty map");
    assert_eq!(hold_reason(w).as_deref(), Some("no-vantage"));
    assert!(sp_speed(w) < 1e-3, "flown: {}", sp_speed(w));
}

/// No lidar fitted: the empty map would see the subject from anywhere, so
/// the goal holds as no vantage instead of reading arrived.
#[test]
fn no_lidar_holds_a_vantage_goal_as_no_vantage() {
    let w = vantage_without_scans(no_lidar());
    assert_eq!(avoid_state(&w), "unavailable");
    assert_no_vantage_held(&w);
}

/// Avoidance off and no scan: the same, reported as off.
#[test]
fn avoid_off_without_scans_holds_a_vantage_goal_as_no_vantage() {
    let mut w = no_lidar();
    w.p.settings.pilot.avoid = AvoidMode::Off;
    let w = vantage_without_scans(w);
    assert_eq!(avoid_state(&w), "off");
    assert_no_vantage_held(&w);
}

#[test]
fn land_rtl_and_pause_still_act_on_stale_scans() {
    let mut w = World::new();
    w.airborne(copter::GUIDED);
    w.action("take_control");
    w.stick([0.0; 4], 600);
    lidar_stops(&mut w);
    assert_eq!(err(&w.action("land")), None);
    w.run(500);
    let sp = w.last_setpoint().unwrap();
    assert_eq!(sp.type_mask.bits(), MASK_VELOCITY);
    assert!(sp.vz > 0.1, "land does not descend: {}", sp.vz);
    assert!(sp.vx.hypot(sp.vy) < 1e-3);

    let mut w = World::new();
    w.airborne(copter::GUIDED);
    w.req(Request::Goto {
        n: 20.0,
        e: 0.0,
        d: -5.0,
        yaw: None,
    });
    lidar_stops(&mut w);
    assert_eq!(err(&w.action("pause")), None);
    assert_eq!(w.p.owner(), Owner::Pilot);
    assert_eq!(err(&w.action("rth")), None);
    w.run(100);
    assert_eq!(w.mode, copter::RTL);
    assert_eq!(w.p.owner(), Owner::FcMode);
}

#[test]
fn avoid_off_flies_without_scans() {
    let mut w = World::new();
    w.p.settings.pilot.avoid = AvoidMode::Off;
    w.lidar = false;
    w.airborne(copter::GUIDED);
    w.action("take_control");
    w.stick([0.0; 4], 600);
    w.stick([0.0, 1.0, 0.0, 0.0], 1500);
    assert!(sp_speed(&w) > 1.0);
    assert_eq!(avoid_state(&w), "off");
    assert_eq!(hold_reason(&w), None);
}

/// The pilot's `lidar` readiness item: (ok, blocks).
fn lidar_item(w: &World) -> Option<(bool, bool)> {
    let s = w.p.state(w.now);
    s.readiness
        .iter()
        .find(|r| r.id == "lidar")
        .map(|r| (r.ok, r.severity == ados_protocol::pilot::Severity::Block))
}

/// A world whose vehicle has no lidar fitted: no scan ever arrives.
fn no_lidar() -> World {
    World::without_lidar()
}

#[test]
fn no_lidar_fitted_warns_before_arming() {
    let mut w = no_lidar();
    w.run(200);
    assert_eq!(
        lidar_item(&w),
        Some((false, false)),
        "a warning, never a block"
    );
    assert_eq!(avoid_state(&w), "unavailable");
    assert!(
        !matches!(err(&w.action("arm")).as_deref(), Some(e) if e.starts_with("not_ready")),
        "a missing lidar blocked arming"
    );
}

#[test]
fn a_vehicle_with_no_lidar_flies_its_sticks_with_avoidance_unavailable() {
    let mut w = no_lidar();
    home(&mut w);
    w.airborne(copter::GUIDED);
    w.action("take_control");
    w.stick([0.0; 4], 600);
    w.stick([0.0, 1.0, 0.0, 0.0], 1500);
    assert!(sp_speed(&w) > 1.0, "no-lidar sticks held: {}", sp_speed(&w));
    assert_eq!(avoid_state(&w), "unavailable");
    assert_eq!(hold_reason(&w), None);
    w.stick([0.0; 4], 1500);
    let point = json!({"lat": 12.0009, "lon": 77.0, "alt_m": 6.0});
    assert_eq!(
        err(&w.action_args("goto", point.clone())).as_deref(),
        Some("obstacle_data_unavailable")
    );
    w.req(Request::Goto {
        n: 20.0,
        e: 0.0,
        d: -5.0,
        yaw: None,
    });
    w.run(200);
    assert_eq!(w.p.owner(), Owner::Pilot, "a goto started with no lidar");
    // A lidar that comes up: once its scan rate is measured (a 2 s window),
    // avoidance is on and Fly Here works.
    w.lidar = true;
    w.run(2500);
    assert_eq!(lidar_item(&w), Some((true, true)));
    assert_eq!(avoid_state(&w), "clear");
    assert_eq!(err(&w.action_args("goto", point)), None);
}

#[test]
fn a_lidar_lost_on_the_ground_blocks_the_launch() {
    let mut w = World::new();
    w.run(200);
    assert_eq!(lidar_item(&w), Some((true, true)));
    lidar_stops(&mut w);
    assert_eq!(lidar_item(&w), Some((false, true)));
    let args = json!({"after": "hold", "alt_m": 3.0});
    assert_eq!(
        err(&w.action_args("launch", args)).as_deref(),
        Some("not_ready:lidar")
    );
    assert!(!w.armed);
}

#[test]
fn a_lidar_seen_then_silent_holds_as_stale_not_unavailable() {
    let mut w = World::new();
    w.airborne(copter::GUIDED);
    w.action("take_control");
    w.stick([0.0; 4], 600);
    lidar_stops(&mut w);
    w.stick([0.0, 1.0, 0.0, 0.0], 1500);
    assert!(
        sp_speed(&w) < 0.5,
        "flies on a silent lidar: {}",
        sp_speed(&w)
    );
    assert_eq!(avoid_state(&w), "stale");
    assert_eq!(hold_reason(&w).as_deref(), Some(STALE));
    assert_eq!(lidar_item(&w), Some((false, true)));
}

#[test]
fn a_lidar_the_capture_service_reports_is_expected_before_its_first_scan() {
    let mut w = no_lidar();
    w.p.on_feed(
        "plugin.world-engine.lidar",
        &json!({"v": 1, "device": "/dev/ttyUSB0", "connected": true, "health": "good",
                "health_code": 0, "rate_hz": 0.0, "scans": 0, "dropped_scans": 0, "reconnects": 0}),
        w.now,
    );
    w.run(200);
    assert_eq!(
        avoid_state(&w),
        "stale",
        "a reported lidar read as none fitted"
    );
}
