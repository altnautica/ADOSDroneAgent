//! A goto behind a wall, seen by a forward depth camera into the voxel map:
//! the grid planner finds the goto blocked (a goto has no detour spots) and
//! flies the trajectory planner's route round the wall's end instead of
//! holding, never inside the obstacle margin.

use ados_pilot::config::PlannerMode;
use ados_protocol::pilot::{AvoidState, Owner, Request};

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
/// The goto, straight north behind the wall, and the flight height, m.
const GOAL_N_M: f32 = 18.0;
const ALT_M: f32 = 4.0;
const TIMEOUT_MS: u64 = 60_000;
/// How often the published avoid state is read, ms.
const STATE_MS: u64 = 200;

fn wall() -> Vec<([f32; 2], f32)> {
    let n = (2.0 * WALL_HALF_M / POST_STEP_M).round() as i32;
    (0..=n)
        .map(|k| ([WALL_N_M, -WALL_HALF_M + POST_STEP_M * k as f32], POST_R_M))
        .collect()
}

fn reactive(s: &mut ados_pilot::config::Settings) {
    s.pilot.planner = PlannerMode::Reactive;
}

/// Fly the goto on `seed`; returns the run's metrics and whether the pilot
/// published a detour.
fn goto_round_the_wall(seed: u64) -> (crate::metrics::RunMetrics, bool) {
    let mut s = Sim::new("goto_wall", seed, reactive);
    s.tier = SensorTier::Depth;
    s.stand = wall();
    s.boot();
    launch_to_hold(&mut s, ALT_M);
    let r = s.request(Request::Goto {
        n: GOAL_N_M,
        e: 0.0,
        d: -ALT_M,
        yaw: None,
    });
    assert_eq!(s.p.owner(), Owner::Goto, "goto refused: {r:?}");
    let t = s.fc.truth();
    let mut meter = Meter::new(seed, [t[0], t[1]], GOAL_N_M);
    let mut detour = false;
    let t0 = s.now;
    while s.now - t0 < TIMEOUT_MS {
        s.step();
        let t = s.fc.truth();
        meter.tick([t[0], t[1]], s.stand_clearance_m());
        if (s.now - t0).is_multiple_of(STATE_MS) {
            detour |= s.p.state(s.now).avoid_state == AvoidState::Detour;
        }
        if crate::bench::trace_on() && (s.now - t0).is_multiple_of(1000) {
            crate::bench::trace_line(&s, t0, [GOAL_N_M, 0.0]);
        }
        if (t[0] - GOAL_N_M).hypot(t[1]) <= REACH_M {
            meter.reached((s.now - t0) as f32 / 1000.0);
            break;
        }
    }
    (meter.finish(), detour)
}

#[test]
fn dst_reactive_goto_routes_round_a_wall_on_a_depth_map() {
    for seed in crate::SEEDS {
        let (m, detour) = goto_round_the_wall(seed);
        eprintln!("dst goto_wall s{seed}: {m:?}, detour {detour}");
        assert!(detour, "s{seed}: the goto never routed round: {m:?}");
        assert!(m.success, "s{seed}: never reached the goal: {m:?}");
        assert!(!m.collision, "s{seed}: hit the wall: {m:?}");
        assert_eq!(m.margin_entries, 0, "s{seed}: into the margin: {m:?}");
        assert!(m.min_clearance_m >= MARGIN_M, "s{seed}: {m:?}");
    }
}
