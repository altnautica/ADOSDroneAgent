//! The pilot's caps with no RC transmitter: goals and gotos are held to 50 m
//! from home and 40 m up whatever the configured limits, a launch into (or a
//! resume of) an FC mission past either cap is refused, and the pilot state
//! carries the declaration and the link-loss window. RC frames with no
//! channel (the FC repeating its last values once input stops) are no
//! receiver.

use ados_protocol::mavlink::ardupilotmega::{MavMessage, RC_CHANNELS_DATA};
use ados_protocol::pilot::{Goal, GoalKind, Owner, RcReceiver, Request};
use serde_json::{json, Value};

use super::confined::err;
use super::confined_route::{feed, item};
use super::harness::{World, FC_COMP, FC_SYS};
use crate::config::{NO_RC_MAX_ALT_M, NO_RC_MAX_DIST_M};
use crate::modes::copter;

/// Declared with no RC, wide configured limits, the FC's no-RC posture.
fn no_rc() -> World {
    let mut w = World::new();
    let p = &mut w.p.settings.pilot;
    p.rc_receiver = RcReceiver::Absent;
    p.max_dist_m = 200.0;
    p.max_alt_agl_m = 120.0;
    for (n, v) in [
        ("FS_THR_ENABLE", 0.0),
        ("FENCE_ENABLE", 1.0),
        ("FENCE_TYPE", 3.0),
        ("FENCE_RADIUS", 70.0),
        ("FENCE_ALT_MAX", 50.0),
        ("FENCE_ACTION", 5.0),
    ] {
        w.p.fc.params.insert(n.into(), v);
    }
    w
}

fn rc_frame(w: &mut World, chancount: u8) {
    let rc = MavMessage::RC_CHANNELS(RC_CHANNELS_DATA {
        chancount,
        chan1_raw: 1500,
        chan2_raw: 1500,
        chan3_raw: 1000,
        chan4_raw: 1500,
        ..Default::default()
    });
    w.p.on_fc(FC_SYS, FC_COMP, &rc, w.now);
    w.run(100);
}

fn rc_seen_ok(w: &World) -> bool {
    w.p.readiness(w.now)
        .into_iter()
        .find(|r| r.id == "rc_seen")
        .is_none_or(|r| r.ok)
}

#[test]
fn rc_frames_with_no_channel_are_no_receiver() {
    let mut w = no_rc();
    rc_frame(&mut w, 0);
    assert!(rc_seen_ok(&w), "stale values with no channel");
    rc_frame(&mut w, 8);
    assert!(!rc_seen_ok(&w), "a receiver feeding 8 channels");
}

#[test]
fn a_skill_goal_stops_at_the_no_rc_distance_cap() {
    let mut w = no_rc();
    w.airborne(copter::GUIDED);
    let id = w.lease();
    w.pos = [NO_RC_MAX_DIST_M - 0.1, 0.0, -4.0];
    let mut goal = Goal::new(GoalKind::Position, [90.0, 0.0, -4.0], 5.0);
    goal.vn = 3.0;
    for _ in 0..4 {
        w.req(Request::Goal {
            lease_id: id,
            goal: goal.clone(),
        });
        w.run(100);
        w.pos[0] = w.pos[0].max(NO_RC_MAX_DIST_M);
    }
    let sp = w.last_setpoint().unwrap();
    assert!(sp.vx <= 1e-3, "outward velocity {}", sp.vx);
    assert!(
        sp.x <= NO_RC_MAX_DIST_M + 1e-3,
        "setpoint n {} past the cap",
        sp.x
    );
}

#[test]
fn a_goto_above_the_no_rc_ceiling_arrives_at_the_ceiling() {
    let mut w = no_rc();
    w.airborne(copter::GUIDED);
    // At the 40 m ceiling. The rangefinder reads nothing up there (out of
    // its range), so the height is the EKF's once the 5 m reading from
    // `airborne` has aged out.
    w.pos = [0.0, 0.0, -NO_RC_MAX_ALT_M];
    w.agl = None;
    w.run(600);
    assert_eq!(w.p.agl(w.now), Some(NO_RC_MAX_ALT_M));
    w.action("take_control");
    w.req(Request::Goto {
        n: 0.0,
        e: 0.0,
        d: -80.0,
        yaw: None,
    });
    w.run(400);
    assert_eq!(
        w.p.owner(),
        Owner::Pilot,
        "arrived at the 40 m ceiling and holds"
    );
}

fn wp(s: u32, n: f32, up: f32) -> Value {
    json!({"seq": s, "cmd": 16, "frame": 6, "lat_e7": 1, "lon_e7": 1, "alt_m": up,
           "ned": [n, 0.0, -up]})
}

fn launch_mission(w: &mut World, items: Vec<Value>) -> Option<String> {
    feed(w, "loaded", 0, items, None, json!({}));
    w.run(200);
    err(&w.action_args("launch", json!({"after": "mission"})))
}

#[test]
fn a_mission_past_the_no_rc_caps_is_refused() {
    let mut w = no_rc();
    let far = vec![item(1, 10.0, 0.0), item(2, 80.0, 0.0)];
    assert_eq!(
        launch_mission(&mut w, far).as_deref(),
        Some("beyond_no_rc_caps")
    );
    let high = vec![wp(1, 10.0, 5.0), wp(2, 20.0, 45.0)];
    assert_eq!(
        launch_mission(&mut w, high).as_deref(),
        Some("beyond_no_rc_caps")
    );
    let unplaced = vec![
        item(1, 10.0, 0.0),
        json!({"seq": 2, "cmd": 16, "frame": 10, "lat_e7": 1, "lon_e7": 1, "alt_m": 5.0, "ned": null}),
    ];
    assert_eq!(
        launch_mission(&mut w, unplaced).as_deref(),
        Some("route_unchecked")
    );
    assert!(!w.armed);
    let inside = vec![wp(1, 10.0, 5.0), wp(2, 45.0, 35.0)];
    assert_eq!(launch_mission(&mut w, inside), None);
}

/// A take-off flown "here" (latitude and longitude 0) has no position but
/// climbs to its own height: past the cap it is refused, in a frame with no
/// height above home it is unchecked, inside it launches.
#[test]
fn a_take_off_here_is_held_to_the_no_rc_ceiling() {
    let takeoff = |frame: u8, up: f32| {
        json!({"seq": 1, "cmd": 22, "frame": frame, "lat_e7": 0, "lon_e7": 0, "alt_m": up,
               "ned": null})
    };
    let mut w = no_rc();
    assert_eq!(
        launch_mission(&mut w, vec![takeoff(3, 60.0), wp(2, 10.0, 5.0)]).as_deref(),
        Some("beyond_no_rc_caps")
    );
    assert_eq!(
        launch_mission(&mut w, vec![takeoff(10, 30.0), wp(2, 10.0, 5.0)]).as_deref(),
        Some("route_unchecked")
    );
    assert_eq!(
        launch_mission(&mut w, vec![takeoff(3, 30.0), wp(2, 10.0, 5.0)]),
        None
    );
}

/// A launch asked to climb past the height cap stops at it.
#[test]
fn a_launch_climbs_no_higher_than_the_no_rc_ceiling() {
    let mut w = no_rc();
    assert_eq!(err(&w.action_args("launch", json!({"alt_m": 60.0}))), None);
    w.run(3_000);
    assert_eq!(w.agl, Some(NO_RC_MAX_ALT_M));
}

#[test]
fn with_rc_the_same_mission_launches() {
    let mut w = no_rc();
    w.p.settings.pilot.rc_receiver = RcReceiver::Present;
    w.p.fc.params.insert("FS_THR_ENABLE".into(), 5.0);
    let far = vec![item(1, 10.0, 0.0), item(2, 80.0, 0.0)];
    assert_eq!(launch_mission(&mut w, far), None);
}

#[test]
fn the_pilot_state_carries_the_declaration_and_the_window() {
    let mut w = World::new();
    let s = w.p.state(w.now);
    assert_eq!((s.rc_receiver, s.link_loss_s), (RcReceiver::Present, 10));
    let v = serde_json::to_value(&s).unwrap();
    assert!(v.get("rc_receiver").is_none(), "omitted while present");
    assert_eq!(v["link_loss_s"], json!(10));
    w.p.settings.pilot.rc_receiver = RcReceiver::Absent;
    w.p.settings.pilot.link_loss_s = 12;
    let v = serde_json::to_value(w.p.state(w.now)).unwrap();
    assert_eq!(
        (v["rc_receiver"].clone(), v["link_loss_s"].clone()),
        (json!("absent"), json!(12))
    );
}
