//! A goal behind a wall, seen by a forward depth camera into the voxel map:
//! the grid planner finds it blocked (a goto has no detour spots, a
//! position goal none either). Under avoid `bypass` the goto or the skill
//! goal flies the trajectory planner's route round the wall's end instead of
//! holding, never inside the obstacle margin; under `brake` it stops and
//! holds before the wall.

use ados_pilot::config::PlannerMode;
use ados_protocol::pilot::{AvoidMode, AvoidState, Goal, GoalKind, Owner, Request};

use crate::metrics::{Meter, MARGIN_M, REACH_M};
use crate::scenarios::launch_to_hold;
use crate::sensors::SensorTier;
use crate::sim::Sim;

/// The wall: posts this far north, out to this either side, m. Short
/// enough that the way round its end starts inside the camera's view from
/// the take-off heading (north); the copter turns toward the route as it
/// flies it.
const WALL_N_M: f32 = 10.0;
const WALL_HALF_M: f32 = 2.0;
/// Its posts: radius and spacing, m (overlapping: no gap through it).
const POST_R_M: f32 = 0.25;
const POST_STEP_M: f32 = 0.4;
/// The goal, straight north behind the wall, and the flight height, m.
const GOAL_N_M: f32 = 18.0;
const ALT_M: f32 = 4.0;
const TIMEOUT_MS: u64 = 60_000;
/// How often the published avoid state is read, and a skill's goal sent,
/// ms.
const STATE_MS: u64 = 200;

fn wall() -> Vec<([f32; 2], f32)> {
    let n = (2.0 * WALL_HALF_M / POST_STEP_M).round() as i32;
    (0..=n)
        .map(|k| ([WALL_N_M, -WALL_HALF_M + POST_STEP_M * k as f32], POST_R_M))
        .collect()
}

/// What flew the goal.
#[derive(Debug, Clone, Copy, PartialEq)]
enum By {
    Goto,
    Skill,
}

/// Fly the goal on `seed` under `avoid` (the reactive planner); returns
/// the run's metrics, whether the pilot published a detour, and whether it
/// published the goal blocked.
fn round_the_wall(seed: u64, avoid: AvoidMode, by: By) -> (crate::metrics::RunMetrics, bool, bool) {
    let mut s = Sim::new("goto_wall", seed, |c| {
        c.pilot.planner = PlannerMode::Reactive;
        c.pilot.avoid = avoid;
    });
    s.tier = SensorTier::Depth;
    s.stand = wall();
    s.boot();
    launch_to_hold(&mut s, ALT_M);
    let target = [GOAL_N_M, 0.0, -ALT_M];
    let lease = match by {
        By::Goto => {
            let r = s.request(Request::Goto {
                n: target[0],
                e: target[1],
                d: target[2],
                yaw: None,
            });
            assert_eq!(s.p.owner(), Owner::Goto, "goto refused: {r:?}");
            None
        }
        By::Skill => Some(crate::forest::lease(&mut s)),
    };
    let goal = Goal::new(GoalKind::Position, target, 3.0);
    let t = s.fc.truth();
    let mut meter = Meter::new(seed, [t[0], t[1]], GOAL_N_M);
    let (mut detour, mut blocked) = (false, false);
    let t0 = s.now;
    while s.now - t0 < TIMEOUT_MS {
        if let (Some(lease_id), true) = (lease, (s.now - t0).is_multiple_of(STATE_MS)) {
            s.request(Request::Goal {
                lease_id,
                goal: goal.clone(),
            });
        }
        s.step();
        let t = s.fc.truth();
        meter.tick([t[0], t[1]], s.stand_clearance_m());
        if (s.now - t0).is_multiple_of(STATE_MS) {
            let st = s.p.state(s.now).avoid_state;
            detour |= st == AvoidState::Detour;
            blocked |= st == AvoidState::Blocked;
        }
        if crate::bench::trace_on() && (s.now - t0).is_multiple_of(1000) {
            crate::bench::trace_line(&s, t0, [GOAL_N_M, 0.0]);
        }
        if (t[0] - GOAL_N_M).hypot(t[1]) <= REACH_M {
            meter.reached((s.now - t0) as f32 / 1000.0);
            break;
        }
    }
    (meter.finish(), detour, blocked)
}

#[test]
fn dst_bypass_goto_and_skill_route_round_a_wall_on_a_depth_map() {
    for by in [By::Goto, By::Skill] {
        for seed in crate::SEEDS {
            let (m, detour, _) = round_the_wall(seed, AvoidMode::Bypass, by);
            eprintln!("dst goto_wall {by:?} s{seed}: {m:?}, detour {detour}");
            assert!(detour, "{by:?} s{seed}: never routed round: {m:?}");
            assert!(m.success, "{by:?} s{seed}: never reached the goal: {m:?}");
            assert!(!m.collision, "{by:?} s{seed}: hit the wall: {m:?}");
            assert_eq!(
                m.margin_entries, 0,
                "{by:?} s{seed}: into the margin: {m:?}"
            );
            assert!(m.min_clearance_m >= MARGIN_M, "{by:?} s{seed}: {m:?}");
        }
    }
}

#[test]
fn dst_brake_goto_and_skill_stop_and_hold_before_a_wall() {
    for by in [By::Goto, By::Skill] {
        let (m, detour, blocked) = round_the_wall(1, AvoidMode::Brake, by);
        eprintln!("dst goto_wall brake {by:?}: {m:?}");
        assert!(blocked && !detour, "{by:?}: held blocked, never routed");
        assert!(!m.success, "{by:?}: stayed before the wall: {m:?}");
        assert!(!m.collision, "{by:?}: {m:?}");
        assert_eq!(m.margin_entries, 0, "{by:?}: {m:?}");
    }
}
