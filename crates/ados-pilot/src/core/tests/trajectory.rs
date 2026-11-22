//! The trajectory planner: a goto routes round a wall the grid planner
//! finds blocked, in shadow beside the grid planner and flown under
//! `pilot.planner = trajectory`.

use ados_protocol::pilot::{AvoidMode, AvoidState, Goal, GoalKind, Request, SensingTier};
use ados_protocol::voxmap::{VoxSnapshot, VoxTier, VoxmapMeta, ESDF_CAP_DM, VOX_FREE};

use super::harness::World;
use super::safety::wall_scan;
use crate::config::PlannerMode;
use crate::geo;
use crate::modes::copter;
use crate::planner::{self, Outcome};

/// A goto 20 m north with a 20 m wall 4 m ahead, fed for `ms`.
fn goto_at_wall(planner: PlannerMode, avoid: AvoidMode, ms: u64) -> World {
    let mut w = World::new();
    w.p.settings.pilot.planner = planner;
    w.p.settings.pilot.avoid = avoid;
    w.airborne(copter::GUIDED);
    w.req(Request::Goto {
        n: 20.0,
        e: 0.0,
        d: -5.0,
        yaw: None,
    });
    let end = w.now + ms;
    while w.now < end {
        w.p.on_feed(crate::events::TOPIC_SCAN, &wall_scan(4.0), w.now);
        w.run(20);
    }
    w
}

/// The grid planner finds the wall blocks the goto (a goto has no detour
/// spots); under avoid `bypass` the trajectory planner in shadow finds the
/// way round its end, and the goto follows that route instead of holding.
#[test]
fn a_reactive_goto_behind_a_wall_follows_the_trajectory_route() {
    let w = goto_at_wall(PlannerMode::Reactive, AvoidMode::Bypass, 1500);
    let s = w.p.state(w.now);
    assert_eq!(s.avoid_state, AvoidState::Detour, "{s:?}");
    assert_eq!(s.hold_reason, None);
    assert!(
        s.plan_ned.iter().any(|p| p[1].abs() > 3.0),
        "the route leaves the straight way: {:?}",
        s.plan_ned
    );
    let frame = serde_json::to_value(&s).unwrap();
    assert_eq!(frame["shadow"]["source"], "trajectory", "{frame}");
    // Flown toward the route's carrot: the straight line to it cuts no bend.
    let plan = &w.p.plan;
    let margin = w.p.margin();
    let carrot = planner::carrot(&w.p.obstacles.grid, &plan.path, margin, None);
    assert_eq!(plan.target, Some(carrot), "{plan:?}");
}

/// Under avoid `brake` the same goto stops and holds before the wall: no
/// route round, no trajectory planner beside the grid planner.
#[test]
fn a_reactive_goto_behind_a_wall_holds_under_brake() {
    let w = goto_at_wall(PlannerMode::Reactive, AvoidMode::Brake, 1500);
    let s = w.p.state(w.now);
    assert_eq!(s.avoid_state, AvoidState::Blocked, "{s:?}");
    assert_eq!(s.hold_reason.as_deref(), Some("blocked"));
    assert_eq!(w.p.trajectory_outcome(), None);
}

/// A skill goal behind the same wall: under `reactive` the trajectory
/// planner runs beside the grid planner only with `pilot.planner_shadow`.
#[test]
fn a_skill_goal_gets_the_trajectory_shadow_only_when_configured() {
    for shadow in [false, true] {
        let mut w = World::new();
        w.p.settings.pilot.planner_shadow = shadow;
        w.airborne(copter::GUIDED);
        let lease_id = w.lease();
        let end = w.now + 1500;
        while w.now < end {
            w.goal(lease_id, 20.0, 0.0);
            w.p.on_feed(crate::events::TOPIC_SCAN, &wall_scan(4.0), w.now);
            w.run(20);
        }
        let frame = serde_json::to_value(w.p.state(w.now)).unwrap();
        assert_eq!(frame.get("shadow").is_some(), shadow, "{frame}");
        assert_eq!(w.p.trajectory_outcome().is_some(), shadow);
    }
}

/// Flown by the trajectory planner: the same goto replans at 10 Hz, flies
/// a committed plan round the wall, and publishes no shadow.
#[test]
fn a_trajectory_goto_behind_a_wall_flies_round_it() {
    let w = goto_at_wall(PlannerMode::Trajectory, AvoidMode::Brake, 1500);
    let s = w.p.state(w.now);
    assert_eq!(s.avoid_state, AvoidState::Detour, "{s:?}");
    assert_eq!(s.hold_reason, None);
    assert!(
        s.plan_ned.iter().any(|p| p[1].abs() > 3.0),
        "the plan leaves the straight way: {:?}",
        s.plan_ned
    );
    assert_eq!(w.p.trajectory_outcome(), Some(Outcome::Committed));
    let sp = w.last_setpoint().expect("a setpoint");
    assert!(sp.vy.abs() > 0.05, "it turns off the straight way: {sp:?}");
    let frame = serde_json::to_value(w.p.state(w.now)).unwrap();
    assert!(frame.get("shadow").is_none(), "{frame}");
}

/// A depth-tier map seen free 60 × 60 m round the start (−12 m to −2 m
/// down), published as `seq` with the body at `body` (the map's frame is
/// the local one).
fn free_depth_map(seq: u64, body: [f32; 3]) -> VoxSnapshot {
    let dims = [120, 120, 20];
    let n = dims.iter().map(|&d| usize::from(d)).product();
    VoxSnapshot {
        valid: true,
        seq,
        meta: VoxmapMeta {
            stamp_ns: 0,
            res_m: 0.5,
            origin_cell: [-60, -60, -24],
            dims,
            map_from_body: [body[0], body[1], body[2], 1.0, 0.0, 0.0, 0.0],
            tier: VoxTier::Depth,
        },
        occupancy: vec![VOX_FREE; n],
        esdf_dm: vec![ESDF_CAP_DM; n],
    }
}

/// The subject the goal names, off to the east-north-east and far enough
/// that no subject brake binds.
const SUBJECT_NE: [f32; 2] = [30.0, 82.0];

/// A skill goal 20 m north naming [`SUBJECT_NE`], with its own `yaw` or
/// none, flown by the trajectory planner on a forward-only (depth) map for
/// `ms`.
fn fly_past_subject(yaw: Option<f32>, ms: u64) -> World {
    let mut w = World::new();
    w.p.settings.pilot.planner = PlannerMode::Trajectory;
    w.airborne(copter::GUIDED);
    let lease_id = w.lease();
    let end = w.now + ms;
    let mut seq = 0;
    while w.now < end {
        let mut goal = Goal::new(GoalKind::Position, [20.0, 0.0, -5.0], 3.0);
        goal.subject_ne = Some(SUBJECT_NE);
        goal.yaw = yaw;
        w.req(Request::Goal { lease_id, goal });
        seq += 2;
        w.p.on_voxmap(&mut free_depth_map(seq, w.pos), w.now);
        w.run(100);
    }
    w
}

/// On a forward-only tier the trajectory planner's motion glance turns
/// the nose from the goal's subject toward where it flies, unless the goal
/// sets its own yaw (a search sweep's vantage goal): that yaw is kept
/// while the trajectory flies.
#[test]
fn a_goal_with_its_own_yaw_gets_no_motion_glance() {
    let deg = f32::to_radians;
    let off = |a: f32, b: f32| geo::wrap_pi(a - b).abs();
    let own = deg(-10.0);
    let w = fly_past_subject(Some(own), 3000);
    let tier = w.p.state(w.now).sensing.map(|s| s.tier);
    assert_eq!(tier, Some(SensingTier::Depth));
    let sp = w.last_setpoint().expect("a setpoint");
    assert!(sp.vx.hypot(sp.vy) >= 0.5, "the trajectory flies: {sp:?}");
    assert!(off(sp.yaw, own) < deg(2.0), "the goal's yaw: {sp:?}");
    // The same goal without a yaw: the nose glances 30° from the subject
    // toward the motion (north), not the goal's yaw nor the subject.
    let w = fly_past_subject(None, 3000);
    let sp = w.last_setpoint().expect("a setpoint");
    assert!(sp.vx.hypot(sp.vy) >= 0.5, "the trajectory flies: {sp:?}");
    let face = geo::bearing_of(geo::sub(SUBJECT_NE, [w.pos[0], w.pos[1]]));
    let glance = face - deg(30.0);
    assert!(
        off(sp.yaw, glance) < deg(3.0),
        "a glance from {face}: {sp:?}"
    );
}
