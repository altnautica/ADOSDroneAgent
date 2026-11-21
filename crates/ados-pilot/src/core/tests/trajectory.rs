//! The trajectory planner: a goto routes round a wall the grid planner
//! finds blocked, in shadow beside the grid planner and flown under
//! `pilot.planner = trajectory`.

use ados_protocol::pilot::{AvoidState, Request};

use super::harness::World;
use super::safety::wall_scan;
use crate::config::PlannerMode;
use crate::modes::copter;
use crate::planner::Outcome;

/// A goto 20 m north with a 20 m wall 4 m ahead, fed for `ms`.
fn goto_at_wall(planner: PlannerMode, ms: u64) -> World {
    let mut w = World::new();
    w.p.settings.pilot.planner = planner;
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
/// spots); the trajectory planner in shadow finds the way round its end,
/// and the goto follows that route instead of holding.
#[test]
fn a_reactive_goto_behind_a_wall_follows_the_trajectory_route() {
    let w = goto_at_wall(PlannerMode::Reactive, 1500);
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
}

/// Flown by the trajectory planner: the same goto replans at 10 Hz, flies
/// a committed plan round the wall, and publishes no shadow.
#[test]
fn a_trajectory_goto_behind_a_wall_flies_round_it() {
    let w = goto_at_wall(PlannerMode::Trajectory, 1500);
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
