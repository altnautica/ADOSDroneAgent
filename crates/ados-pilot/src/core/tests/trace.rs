//! The published command of a skill goal or a goto (what was asked, what
//! the setpoint asks of the FC, which limit cut it) and the shadow-mode hook.

use std::f32::consts::TAU;

use ados_protocol::pilot::{AvoidMode, Goal, GoalKind, Request};
use serde_json::{json, Value};

use super::harness::World;
use super::safety::{wall_scan, G};
use crate::core::control::brake_limit;
use crate::core::{limit, ShadowCmd, SHADOW_MAX_AGE_MS};
use crate::fc::{MASK_POS_VEL_ACC_YAW, MASK_POS_VEL_YAW};
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
    let mask = sp.type_mask.bits();
    assert!(
        mask == MASK_POS_VEL_YAW || mask == MASK_POS_VEL_ACC_YAW,
        "mask {mask}"
    );
    geo::add([sp.vx, sp.vy], geo::sub([sp.x, sp.y], pos))
}

/// The pilot's own pull toward a point `to` away: linear at gain 1 near
/// it, slowing into it at 0.7 m/s² (the Normal goal deceleration).
fn pull(to: [f32; 2]) -> [f32; 2] {
    let e = geo::norm(to);
    let r = if e > 0.7 {
        (1.4 * (e - 0.35)).sqrt()
    } else {
        e
    };
    geo::scale(to, r / e)
}

/// Send `goal(seconds)` every 100 ms for `ms`, with `scan` before each
/// 20 ms tick when given.
fn fly_goal(w: &mut World, id: u64, ms: u64, scan: Option<&Value>, goal: impl Fn(f32) -> Goal) {
    let t0 = w.now;
    while w.now < t0 + ms {
        let g = goal((w.now - t0) as f32 / 1000.0);
        w.req(Request::Goal {
            lease_id: id,
            goal: g,
        });
        for _ in 0..5 {
            if let Some(s) = scan {
                w.p.on_feed("plugin.world-engine.scan", s, w.now);
            }
            w.run(20);
        }
    }
}

/// A follow goal flown in the open: the frame carries the command of the
/// tick, in the FC's local frame, with the skill's goal, the target, the
/// asked and commanded velocity (which is what the setpoint asks of the FC)
/// and the speed limit. Once the shaped flight has caught up with the
/// moving goal, nothing limits it.
#[test]
fn a_flown_goal_publishes_its_command() {
    let mut w = World::new();
    w.p.epoch_offset_ms = 1_700_000_000_000;
    w.p.settings.pilot.avoid = AvoidMode::Off;
    w.airborne(copter::GUIDED);
    w.fly = true;
    let id = w.lease();
    fly_goal(&mut w, id, 10_000, None, |t| {
        follow_goal([3.0 + t, 1.0, -5.0], [1.0, 0.0])
    });
    let v = frame(&w);
    let cmd = &v["command"];
    assert!(cmd.is_object(), "no command on the frame: {v}");
    let t = cmd["t_ms"].as_u64().unwrap() - 1_700_000_000_000;
    assert!(t + 40 >= w.now && t <= w.now, "tick at {t}, now {}", w.now);
    let goal = &cmd["goal_ned"];
    assert!(
        (goal[0].as_f64().unwrap() - 12.9).abs() < 1e-3 && goal[1] == 1.0,
        "{goal}"
    );
    assert!((pair(&cmd["target_ne"])[1] - 1.0).abs() < 1e-3, "{cmd}");
    // Asked: the feed-forward plus the pull toward the target, nearly
    // nothing once caught up.
    let ask = pair(&cmd["ask_vel_ne"]);
    assert!(geo::dist(ask, [1.0, 0.0]) < 0.1, "asked {ask:?}");
    // The command is what the setpoint asks of the FC (the vehicle has
    // since flown one 20 ms tick).
    let fc = fc_ask(&w, [w.pos[0], w.pos[1]]);
    let c = pair(&cmd["cmd_vel_ne"]);
    assert!(
        geo::dist(c, fc) < 0.05,
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
    fly_goal(&mut w, id, 6000, Some(&empty), |_| {
        follow_goal([30.0, 0.0, -5.0], [0.0, 0.0])
    });
    let v = frame(&w);
    let cmd = &v["command"];
    let lim = limits(cmd);
    assert!(lim.iter().any(|l| l == "seen_space"), "{cmd}");
    assert!(!lim.iter().any(|l| l == "hold"), "{cmd}");
    let ask = pair(&cmd["ask_vel_ne"]);
    let flown = pair(&cmd["cmd_vel_ne"]);
    assert!(ask[0] > 6.0, "asked {ask:?}");
    assert!(flown[0] > 1.0 && flown[0] < ask[0], "flown {flown:?}");
    assert!(geo::dist(flown, fc_ask(&w, [0.0, 0.0])) < 1e-3);
}

/// A wall across a goto holds the vehicle: the command is a hold that keeps
/// the target and what the goto asked, so asked against commanded shows the
/// freeze, and `hold_reason` says why.
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
    assert_eq!(cmd["hold_reasons"], json!(["blocked"]), "{cmd}");
    assert_eq!(pair(&cmd["target_ne"]), [20.0, 0.0], "{cmd}");
    assert_eq!(cmd["goal_ned"], json!([20.0, 0.0, -5.0]));
    // Asked: the pilot's pull toward the goto's target, slowing into it on
    // the goal deceleration, from where the vehicle is.
    let ask = pair(&cmd["ask_vel_ne"]);
    let want = pull(geo::sub([20.0, 0.0], [w.pos[0], w.pos[1]]));
    assert!(geo::dist(ask, want) < 1e-2, "asked {ask:?}, want {want:?}");
    assert!(cmd["v_cap_mps"].as_f64().unwrap() > 0.0, "{cmd}");
    // Flown: the hold, far below what was asked.
    let flown = pair(&cmd["cmd_vel_ne"]);
    assert!(geo::norm(flown) < 1.0, "flown {flown:?}");
}

/// A wall in front of a follow goal holds the vehicle; the command keeps the
/// goal's feed-forward and speed limit in what was asked.
#[test]
fn a_blocked_follow_keeps_what_it_asked() {
    let mut w = World::new();
    w.airborne(copter::GUIDED);
    let id = w.lease();
    let end = w.now + 1500;
    while w.now < end {
        // Each goal renews the lease.
        w.req(Request::Goal {
            lease_id: id,
            goal: follow_goal([20.0, 0.0, -5.0], [3.0, 0.0]),
        });
        for _ in 0..10 {
            w.p.on_feed("plugin.world-engine.scan", &wall_scan(4.0), w.now);
            w.run(20);
        }
    }
    let v = frame(&w);
    let cmd = &v["command"];
    assert_eq!(v["hold_reason"], "blocked", "{v}");
    assert_eq!(
        limits(cmd).last().map(String::as_str),
        Some("hold"),
        "{cmd}"
    );
    let target = pair(&cmd["target_ne"]);
    let to = geo::sub(target, [w.pos[0], w.pos[1]]);
    let ask = pair(&cmd["ask_vel_ne"]);
    assert!(
        geo::dist(ask, geo::add([3.0, 0.0], pull(to))) < 1e-2,
        "asked {ask:?}, target {target:?}"
    );
    assert_eq!(cmd["v_cap_mps"].as_f64(), Some(6.0));
    let flown = pair(&cmd["cmd_vel_ne"]);
    assert!(geo::norm(flown) < 1.0, "flown {flown:?}");
}

/// Under external navigation the speed cap binds a far goal: the command
/// names it first, and names the goal's own limit only when that cut too.
/// At the cap the shaper no longer limits it.
#[test]
fn the_nav_cap_is_named_when_it_binds() {
    for (ff, want) in [(3.0, vec!["nav_cap"]), (8.0, vec!["nav_cap", "speed"])] {
        let mut w = World::new();
        w.p.fc.params.insert("EK3_SRC1_POSXY".into(), 3.0);
        w.p.fc.params.insert("EK3_SRC2_POSXY".into(), 6.0);
        w.airborne(copter::GUIDED);
        let id = w.lease();
        let t0 = w.now;
        while w.now < t0 + 5000 {
            // vision-nav reports source set 2 (external nav) active.
            w.p.on_feed("nav.state", &json!({"active_set": 2, "gps_fix": 0}), w.now);
            fly_goal(&mut w, id, 100, None, |_| {
                follow_goal([8.0, 0.0, -5.0], [ff, 0.0])
            });
        }
        let cmd = &frame(&w)["command"];
        assert_eq!(limits(cmd), want, "feed-forward {ff}: {cmd}");
        assert_eq!(cmd["v_cap_mps"].as_f64(), Some(2.0));
        let flown = pair(&cmd["cmd_vel_ne"]);
        assert!(geo::norm(flown) <= 2.0 + 1e-3, "{flown:?}");
    }
}

/// A goal far away with nothing to brake for: the setpoint's pull toward it
/// is held, so the commanded velocity stays within the speed limit, and the
/// shaped flight reaches it.
#[test]
fn the_commanded_velocity_stays_within_the_limit() {
    let mut w = World::new();
    w.p.settings.pilot.avoid = AvoidMode::Off;
    w.airborne(copter::GUIDED);
    let id = w.lease();
    let t0 = w.now;
    while w.now < t0 + 10_000 {
        fly_goal(&mut w, id, 100, None, |_| {
            follow_goal([60.0, 0.0, -5.0], [0.0, 0.0])
        });
        let cmd = &frame(&w)["command"];
        let flown = pair(&cmd["cmd_vel_ne"]);
        assert!(geo::norm(flown) <= 6.0 + 1e-3, "flown {flown:?}");
    }
    let cmd = &frame(&w)["command"];
    assert!(pair(&cmd["ask_vel_ne"])[0] > 6.0, "{cmd}");
    let flown = pair(&cmd["cmd_vel_ne"]);
    assert!(flown[0] > 5.0, "flown {flown:?}");
}

/// A hold shorter than the frame period still reaches the next frame's
/// command, and is gone from the frame after it.
#[test]
fn a_short_hold_reaches_the_next_frame() {
    let mut w = World::new();
    w.airborne(copter::GUIDED);
    let id = w.lease();
    w.req(Request::Goal {
        lease_id: id,
        goal: follow_goal([20.0, 0.0, -5.0], [0.0, 0.0]),
    });
    w.run(500);
    while !w.now.is_multiple_of(100) {
        w.run(20);
    }
    // One tick with a wall inside the hard-stop distance, then open space.
    w.p.on_feed("plugin.world-engine.scan", &wall_scan(0.8), w.now);
    w.run(20);
    assert!(
        frame(&w)["hold_reason"].is_string(),
        "the wall did not hold"
    );
    w.p.on_feed("plugin.world-engine.scan", &open_scan(), w.now);
    w.run(60);
    let v = frame(&w);
    assert!(v["hold_reason"].is_null(), "still holding: {v}");
    let cmd = &v["command"];
    assert!(limits(cmd).contains(&"hold".to_owned()), "{cmd}");
    assert_eq!(
        cmd["hold_reasons"].as_array().map(Vec::len),
        Some(1),
        "{cmd}"
    );
    // Published: the next frame starts clean.
    w.run(40);
    let cmd = &frame(&w)["command"];
    assert!(!limits(cmd).contains(&"hold".to_owned()), "{cmd}");
    assert!(cmd.get("hold_reasons").is_none(), "{cmd}");
}

/// A scan with no returns.
fn open_scan() -> Value {
    json!({"frame": "body_frd", "angle_start_rad": 0.0,
           "angle_increment_rad": TAU / 360.0, "ranges_m": vec![0.0f32; 360]})
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

/// A stick tick publishes its command: what the sticks asked, the shaped
/// reference sent, the limits it flew at and the input age and `seq` of the
/// frame flown; the frame also carries `stick` and `profile_limits`.
#[test]
fn stick_flight_publishes_the_stick_command() {
    let mut w = World::new();
    w.airborne(copter::GUIDED);
    w.action("take_control");
    w.stick([0.0; 4], 600);
    w.stick([0.0, 0.5, 0.0, 0.0], 300);
    let f = frame(&w);
    let cmd = &f["command"];
    let stick = &cmd["stick"];
    assert_eq!(stick, &f["stick"], "{f}");
    assert!(stick["ask_ne_mps"][0].as_f64().unwrap() > 2.0, "{stick}");
    let v = stick["v_ne_mps"][0].as_f64().unwrap();
    assert!(v > 0.0 && v < 2.0, "shaped below the ask: {stick}");
    assert_eq!(stick["braking"], false);
    assert_eq!(stick["input_seq"], w.seq - 1);
    assert!(stick["input_age_ms"].as_u64().unwrap() <= 20, "{stick}");
    assert_eq!(cmd["profile_limits"]["profile"], "normal");
    assert_eq!(f["profile_limits"]["v_h_mps"], 7.0);
    assert!(limits(cmd).iter().any(|l| l == "shaper"), "{cmd}");
    // Released, it brakes; once at rest the frame keeps the stick command
    // with nothing asked.
    w.stick([0.0; 4], 100);
    assert_eq!(frame(&w)["stick"]["braking"], true);
    w.stick([0.0; 4], 3000);
    let f = frame(&w);
    assert_eq!(f["stick"]["braking"], false);
    assert_eq!(f["stick"]["v_ne_mps"], serde_json::json!([0.0, 0.0]));
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
    let b = brake_limit(&o, [0.0, 0.0], [0.0, 0.0], [5.0, 0.0], 3.0, G, 0);
    assert!(b.cut, "{b:?}");
    assert_eq!(b.by & limit::OBSTACLE, limit::OBSTACLE, "{b:?}");
    assert_eq!(b.by & limit::OBSTACLE_CLOSING, 0, "no scan closing: {b:?}");
    // Nothing in reach: no cut, nothing named.
    let open = brake_limit(&o, [-20.0, 0.0], [0.0, 0.0], [-1.0, 0.0], 3.0, G, 0);
    assert!(!open.cut && open.by == 0, "{open:?}");
    // Inside the hard-stop distance.
    let close = brake_limit(&o, [3.3, 0.0], [0.0, 0.0], [1.0, 0.0], 3.0, G, 0);
    assert!(close.close);
    assert_eq!(close.by & limit::HARD_STOP, limit::HARD_STOP, "{close:?}");
    // Inside the margin moving along the block: the push back out.
    let along = brake_limit(&o, [2.8, 2.5], [0.0, 0.0], [0.0, 0.3], 3.0, G, 0);
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
