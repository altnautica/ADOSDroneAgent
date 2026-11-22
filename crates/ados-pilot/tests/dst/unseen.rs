//! A goal on a forward depth camera's map whose way lies off the nose: the
//! skill holds its heading north and asks for a point due east, past a
//! trunk the camera has never seen. The vehicle must not creep sideways
//! into that unknown (the brake's stand-in for the space beside the
//! vehicle leaves a little stopping room that moves with it); it turns to
//! look along the way first, then routes round the trunk (avoid `bypass`),
//! keeping out of the margin.

use ados_pilot::config::PlannerMode;
use ados_protocol::pilot::{AvoidMode, Goal, GoalKind, Request};

use crate::metrics::{Meter, REACH_M};
use crate::scenarios::launch_to_hold;
use crate::sensors::SensorTier;
use crate::sim::Sim;

/// The goal, due east of the take-off point, the trunk on the way to it,
/// and the flight height, m.
const GOAL_E_M: f32 = 14.0;
const TRUNK_E_M: f32 = 5.0;
const TRUNK_R_M: f32 = 0.3;
const ALT_M: f32 = 4.0;
const TIMEOUT_MS: u64 = 60_000;
/// How often the skill's goal is sent, ms.
const GOAL_MS: u64 = 200;

#[test]
fn dst_depth_goal_never_creeps_into_space_the_camera_has_not_seen() {
    for planner in [PlannerMode::Reactive, PlannerMode::Trajectory] {
        let mut s = Sim::new("unseen_side", 1, |c| {
            c.pilot.planner = planner;
            c.pilot.avoid = AvoidMode::Bypass;
        });
        s.tier = SensorTier::Depth;
        s.stand = vec![([0.0, TRUNK_E_M], TRUNK_R_M)];
        s.boot();
        launch_to_hold(&mut s, ALT_M);
        let lease_id = crate::forest::lease(&mut s);
        let mut goal = Goal::new(GoalKind::Position, [0.0, GOAL_E_M, -ALT_M], 3.0);
        goal.yaw = Some(0.0);
        let t = s.fc.truth();
        let mut meter = Meter::new(1, [t[0], t[1]], GOAL_E_M);
        let mut turned = false;
        let t0 = s.now;
        while s.now - t0 < TIMEOUT_MS {
            if (s.now - t0).is_multiple_of(GOAL_MS) {
                s.request(Request::Goal {
                    lease_id,
                    goal: goal.clone(),
                });
            }
            s.step();
            let t = s.fc.truth();
            meter.tick([t[0], t[1]], s.stand_clearance_m());
            if crate::bench::trace_on() && (s.now - t0).is_multiple_of(1000) {
                crate::bench::trace_line(&s, t0, [0.0, GOAL_E_M]);
            }
            turned |= s.fc.att[2] > 45f32.to_radians();
            if t[0].hypot(t[1] - GOAL_E_M) <= REACH_M {
                meter.reached((s.now - t0) as f32 / 1000.0);
                break;
            }
        }
        let m = meter.finish();
        eprintln!("dst unseen_side {planner:?}: {m:?}, turned {turned}");
        assert!(!m.collision, "{planner:?}: hit the unseen trunk: {m:?}");
        assert_eq!(m.margin_entries, 0, "{planner:?}: into the margin: {m:?}");
        assert!(turned, "{planner:?}: never looked along the way: {m:?}");
        assert!(m.success, "{planner:?}: never reached the goal: {m:?}");
    }
}
