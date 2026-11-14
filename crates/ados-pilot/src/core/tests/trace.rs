//! The published command of a skill goal or a goto (what was asked, what
//! the setpoint asks of the FC, which limit cut it) and the shadow-mode hook.

use std::f32::consts::TAU;

use ados_protocol::pilot::{Goal, GoalKind, Request};
use serde_json::{json, Value};

use super::harness::World;
use super::safety::wall_scan;
use crate::core::control::brake_limit;
use crate::core::{limit, ShadowCmd, SHADOW_MAX_AGE_MS};
use crate::fc::MASK_POS_VEL_YAW;
use crate::geo;
use crate::modes::copter;
use crate::obstacles::Obstacles;

/// A follow goal toward `ned` at up to 6 m/s with its feed-forward.
fn follow_goal(ned: [f32; 3], vel_ne: [f32; 2]) -> Goal {
    let mut g = Goal::new(GoalKind::Follow, ned, 6.0);
    g.vn = vel_ne[0];
    g.ve = vel_ne[1];
    g
}

/// The pilot frame as the nav stream carries it.
fn frame(w: &World) -> Value {
    serde_json::to_value(w.p.state(w.now)).unwrap()
}

fn pair(v: &Value) -> [f32; 2] {
    [v[0].as_f64().unwrap() as f32, v[1].as_f64().unwrap() as f32]
}

fn limits(cmd: &Value) -> Vec<String> {
    cmd["limits"]
        .as_array()
        .unwrap_or(&Vec::new())
        .iter()
        .map(|l| l.as_str().unwrap().to_owned())
        .collect()
}

/// What the last setpoint asks of the FC at `pos`: its velocity plus the
/// position loop's pull (gain 1) toward its position.
fn fc_ask(w: &World, pos: [f32; 2]) -> [f32; 2] {
    let sp = w.last_setpoint().unwrap();
    assert_eq!(sp.type_mask.bits(), MASK_POS_VEL_YAW);
    geo::add([sp.vx, sp.vy], geo::sub([sp.x, sp.y], pos))
}

/// A follow goal flown in the open: the frame carries the command of the
/// tick, in the FC's local frame, with the skill's goal, the target, the
/// asked and commanded velocity (which is what the setpoint asks of the FC)
/// and the speed limit.
#[test]
fn a_flown_goal_publishes_its_command() {
    let mut w = World::new();
    w.p.epoch_offset_ms = 1_700_000_000_000;
    w.airborne(copter::GUIDED);
    let id = w.lease();
    w.req(Request::Goal {
        lease_id: id,
        goal: follow_goal([3.0, 1.0, -5.0], [1.0, 0.0]),
    });
    w.run(300);
    let v = frame(&w);
    let cmd = &v["command"];
    assert!(cmd.is_object(), "no command on the frame: {v}");
    let t = cmd["t_ms"].as_u64().unwrap() - 1_700_000_000_000;
    assert!(t + 40 >= w.now && t <= w.now, "tick at {t}, now {}", w.now);
    assert_eq!(cmd["goal_ned"], json!([3.0, 1.0, -5.0]));
    assert_eq!(pair(&cmd["target_ne"]), [3.0, 1.0]);
    // Asked: the feed-forward plus the pull toward the target.
    assert_eq!(pair(&cmd["ask_vel_ne"]), [4.0, 1.0]);
    let fc = fc_ask(&w, [0.0, 0.0]);
    let c = pair(&cmd["cmd_vel_ne"]);
    assert!(
        geo::dist(c, fc) < 1e-3,
        "command {c:?}, setpoint asks {fc:?}"
    );
    assert_eq!(cmd["v_cap_mps"].as_f64(), Some(6.0));
    assert!(limits(cmd).is_empty(), "{cmd}");
}

/// A goal 30 m out over a scan with no return: the brake holds the speed to
/// what the vehicle can stop from inside the space it has seen, and the
/// command names that limit and keeps what was asked.
#[test]
fn a_braked_goal_names_the_limit_that_cut_it() {
    let mut w = World::new();
    w.airborne(copter::GUIDED);
    let id = w.lease();
    let empty = json!({"frame": "body_frd", "angle_start_rad": 0.0,
                       "angle_increment_rad": TAU / 360.0, "ranges_m": vec![0.0f32; 360]});
    w.p.on_feed("plugin.world-engine.scan", &empty, w.now);
    w.req(Request::Goal {
        lease_id: id,
        goal: follow_goal([30.0, 0.0, -5.0], [0.0, 0.0]),
    });
    w.run(300);
    let v = frame(&w);
    let cmd = &v["command"];
    assert_eq!(limits(cmd), ["seen_space"], "{cmd}");
    let ask = pair(&cmd["ask_vel_ne"]);
    let flown = pair(&cmd["cmd_vel_ne"]);
    assert!(ask[0] > 29.0, "asked {ask:?}");
    assert!(flown[0] > 1.0 && flown[0] < ask[0], "flown {flown:?}");
    assert!(geo::dist(flown, fc_ask(&w, [0.0, 0.0])) < 1e-3);
}

/// A wall across a goto holds the vehicle: the command is a hold with
/// nothing asked and no target, and `hold_reason` says why.
#[test]
fn a_blocked_goto_publishes_a_hold() {
    let mut w = World::new();
    w.airborne(copter::GUIDED);
    w.req(Request::Goto {
        n: 20.0,
        e: 0.0,
        d: -5.0,
        yaw: None,
    });
    let end = w.now + 1500;
    while w.now < end {
        w.p.on_feed("plugin.world-engine.scan", &wall_scan(4.0), w.now);
        w.run(20);
    }
    let v = frame(&w);
    let cmd = &v["command"];
    assert_eq!(v["hold_reason"], "blocked");
    assert_eq!(limits(cmd), ["hold"], "{cmd}");
    assert!(cmd["target_ne"].is_null(), "{cmd}");
    assert_eq!(pair(&cmd["ask_vel_ne"]), [0.0, 0.0]);
    assert_eq!(cmd["goal_ned"], json!([20.0, 0.0, -5.0]));
}

/// Under external navigation the speed cap binds a far goal: the command
/// names it.
#[test]
fn the_nav_cap_is_named_when_it_binds() {
    let mut w = World::new();
    w.p.fc.params.insert("EK3_SRC1_POSXY".into(), 3.0);
    w.p.fc.params.insert("EK3_SRC2_POSXY".into(), 6.0);
    w.airborne(copter::GUIDED);
    // vision-nav reports source set 2 (external nav) active.
    w.p.on_feed("nav.state", &json!({"active_set": 2, "gps_fix": 0}), w.now);
    let id = w.lease();
    w.req(Request::Goal {
        lease_id: id,
        goal: follow_goal([8.0, 0.0, -5.0], [3.0, 0.0]),
    });
    w.run(300);
    let cmd = &frame(&w)["command"];
    assert!(limits(cmd).contains(&"nav_cap".to_owned()), "{cmd}");
    assert_eq!(cmd["v_cap_mps"].as_f64(), Some(2.0));
    let flown = pair(&cmd["cmd_vel_ne"]);
    assert!(geo::norm(flown) <= 2.0 + 1e-3, "{flown:?}");
}

/// A feed-forward faster than the goal's own limit is held to it.
#[test]
fn the_goal_speed_limit_is_named_when_it_binds() {
    let mut w = World::new();
    w.airborne(copter::GUIDED);
    let id = w.lease();
    w.req(Request::Goal {
        lease_id: id,
        goal: follow_goal([2.0, 0.0, -5.0], [8.0, 0.0]),
    });
    w.run(300);
    let cmd = &frame(&w)["command"];
    assert_eq!(
        limits(cmd).first().map(String::as_str),
        Some("speed"),
        "{cmd}"
    );
    let sp = pair(&cmd["sp_vel_ned"]);
    assert!(geo::norm(sp) <= 6.0 + 1e-3, "{sp:?}");
}

/// Stick flight flies no goal, so the frame carries no command.
#[test]
fn stick_flight_publishes_no_command() {
    let mut w = World::new();
    w.airborne(copter::GUIDED);
    w.stick([0.0, 0.5, 0.0, 0.0], 300);
    assert!(frame(&w).get("command").is_none());
}

/// Mark a square block of cells centred on `c` with half-width `half`.
fn block(o: &mut Obstacles, c: [f32; 2], half: [f32; 2]) {
    o.grid.reset([0.0, 0.0]);
    let mut n = c[0] - half[0];
    while n <= c[0] + half[0] {
        let mut e = c[1] - half[1];
        while e <= c[1] + half[1] {
            o.grid.mark([n, e]);
            e += 0.1;
        }
        n += 0.1;
    }
    o.grid.transform();
}

/// The brake says which of its terms cut a velocity.
#[test]
fn the_brake_attributes_its_cut() {
    let mut o = Obstacles::new();
    // A block 4 m ahead: the stopping distance to it cuts 5 m/s.
    block(&mut o, [4.5, 0.0], [0.5, 2.0]);
    let b = brake_limit(&o, [0.0, 0.0], [0.0, 0.0], [5.0, 0.0], 3.0, 1.5, 0);
    assert!(b.cut, "{b:?}");
    assert_eq!(b.by & limit::OBSTACLE, limit::OBSTACLE, "{b:?}");
    assert_eq!(b.by & limit::OBSTACLE_CLOSING, 0, "no scan closing: {b:?}");
    // Nothing in reach: no cut, nothing named.
    let open = brake_limit(&o, [-20.0, 0.0], [0.0, 0.0], [-1.0, 0.0], 3.0, 1.5, 0);
    assert!(!open.cut && open.by == 0, "{open:?}");
    // Inside the hard-stop distance.
    let close = brake_limit(&o, [3.3, 0.0], [0.0, 0.0], [1.0, 0.0], 3.0, 1.5, 0);
    assert!(close.close);
    assert_eq!(close.by & limit::HARD_STOP, limit::HARD_STOP, "{close:?}");
    // Inside the margin moving along the block: the push back out.
    let along = brake_limit(&o, [2.8, 2.5], [0.0, 0.0], [0.0, 0.3], 3.0, 1.5, 0);
    assert_eq!(along.by & limit::PUSH, limit::PUSH, "{along:?}");
}

/// A planner run in shadow mode: its command is published with its source
/// and marked live while the pilot flies a goal, never sent to the FC, and
/// dropped once stale.
#[test]
fn a_shadow_command_is_published_and_never_flown() {
    let mut w = World::new();
    w.p.epoch_offset_ms = 1_700_000_000_000;
    w.airborne(copter::GUIDED);
    // Nothing flies a goal: commanded, not flown.
    let shadow = |at_ms| ShadowCmd {
        at_ms,
        pos_ne: [0.5, 0.0],
        vel_ne: [3.5, 0.25],
        acc_ne: [1.0, 0.0],
        path_ne: (0..20).map(|k| [k as f32, 0.0]).collect(),
        committed: false,
        reason: Some("exploratory"),
    };
    w.p.set_shadow("trajectory", Some(shadow(w.now)));
    let v = frame(&w);
    let s = &v["shadow"];
    assert_eq!(s["source"], "trajectory");
    assert_eq!(s["t_ms"].as_u64(), Some(w.now + 1_700_000_000_000));
    assert_eq!(pair(&s["vel_ne"]), [3.5, 0.25]);
    assert_eq!(s["path_ne"].as_array().unwrap().len(), 16);
    assert_eq!(s["reason"], "exploratory");
    assert_eq!(s["live"], false);
    // A goal flies: live, and the setpoint is the pilot's own.
    let id = w.lease();
    w.req(Request::Goal {
        lease_id: id,
        goal: follow_goal([3.0, 0.0, -5.0], [0.0, 0.0]),
    });
    w.run(100);
    w.p.set_shadow("trajectory", Some(shadow(w.now)));
    assert_eq!(frame(&w)["shadow"]["live"], true);
    w.run(100);
    let sp = w.last_setpoint().unwrap();
    assert!(
        sp.vy.abs() < 1e-3,
        "the shadow's velocity was flown: {sp:?}"
    );
    // Stale: dropped.
    w.run(SHADOW_MAX_AGE_MS + 100);
    assert!(frame(&w).get("shadow").is_none());
}
