//! Flight on a forward depth camera's map beside space the camera has not
//! seen.
//!
//! A goal whose way lies off the nose: the skill holds its heading north
//! and asks for a point due east, past a trunk the camera has never seen.
//! The vehicle must not creep sideways into that unknown (the brake's
//! stand-in for the space beside the vehicle leaves a little stopping room
//! that moves with it); it turns to look along the way first, then routes
//! round the trunk (avoid `bypass`), keeping out of the margin.
//!
//! A follow skill orbiting a standing subject, or holding the side of a
//! walking one: the motion beside the subject is held to what the camera
//! has seen, and the camera only glances toward it, so the subject stays
//! in view nearly all the time, clear of every trunk.
//!
//! A person walking up to a hovering vehicle and standing inside the
//! separation the guard keeps: the guard's back-away runs into space
//! behind the vehicle the camera has never seen, with a trunk in it; the
//! back-away is held there, and the vehicle does not back blind into it.

use ados_pilot::config::PlannerMode;
use ados_protocol::pilot::{AvoidMode, Goal, GoalKind, Request};

use crate::follow::{self, Setup, Trail};
use crate::metrics::{Meter, MARGIN_M, REACH_M};
use crate::scenarios::launch_to_hold;
use crate::sensors::{SensorTier, Walker};
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
/// The share of following periods the subject must stay in the camera's
/// view.
const VIEW_MIN: f32 = 0.85;
/// The trunk behind the hovering vehicle (south of it) and where the
/// person stops in front of it (north), m.
const BEHIND_S_M: f32 = 2.6;
const PERSON_STOP_N_M: f32 = 2.0;
/// How long the person stands there, ms.
const STAND_MS: u64 = 20_000;
/// The most the vehicle may back from where it hovered, m: what the
/// position hold drifts, not a back-away.
const BACK_MAX_M: f32 = 0.5;

const PLANNERS: [PlannerMode; 2] = [PlannerMode::Reactive, PlannerMode::Trajectory];

fn depth_sim(name: &'static str, seed: u64, planner: PlannerMode) -> Sim {
    let mut s = Sim::new(name, seed, |c| {
        c.pilot.planner = planner;
        c.pilot.avoid = AvoidMode::Bypass;
    });
    s.tier = SensorTier::Depth;
    s
}

#[test]
fn dst_depth_goal_never_creeps_into_space_the_camera_has_not_seen() {
    for planner in PLANNERS {
        let mut s = depth_sim("unseen_side", 1, planner);
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

/// An orbit (10°/s over the ground) round a subject standing 8 m north,
/// trunks 11-12 m out on seeded bearings, on the depth tier.
fn orbit(seed: u64, planner: PlannerMode) -> follow::Follow {
    let mut s = depth_sim("unseen_orbit", seed, planner);
    let c = [8.0f32, 0.0f32];
    for k in 0..6 {
        let a = (60.0 * k as f32 + s.rng.range(0.0, 60.0)).to_radians();
        let d = s.rng.range(11.0, 12.0);
        s.stand
            .push(([c[0] + d * a.cos(), c[1] + d * a.sin()], 0.25));
    }
    let trail = Trail {
        pts: vec![c],
        pace: follow::WALK_MPS,
        start_ms: 0,
    };
    let setup = Setup {
        bearing_pref_deg: None,
        blind_ms: None,
        tail_ms: 2_000,
        ring: false,
        orbit_dps: Some(10.0),
    };
    follow::fly(&mut s, &trail, &setup)
}

/// A walker 30 m north at a walk, the skill asking for its right side
/// (90° from its heading), trunks 4 m off the walk's east side, on the
/// depth tier.
fn side(seed: u64, planner: PlannerMode) -> follow::Follow {
    let mut s = depth_sim("unseen_follow_side", seed, planner);
    for k in 0..5 {
        let n = 12.0 + 6.0 * k as f32 + s.rng.range(-1.0, 1.0);
        s.stand.push(([n, 10.0 + s.rng.range(0.0, 2.0)], 0.25));
    }
    let trail = Trail {
        pts: vec![[8.0, 0.0], [38.0, 0.0]],
        pace: follow::WALK_MPS,
        start_ms: 0,
    };
    let setup = Setup {
        bearing_pref_deg: Some(90.0),
        blind_ms: None,
        tail_ms: 3_000,
        ring: false,
        orbit_dps: None,
    };
    follow::fly(&mut s, &trail, &setup)
}

#[test]
fn dst_depth_follow_keeps_the_subject_in_view_beside_unseen_space() {
    type Case = (&'static str, fn(u64, PlannerMode) -> follow::Follow);
    let cases: [Case; 2] = [("orbit", orbit), ("side", side)];
    for (name, case) in cases {
        for planner in PLANNERS {
            for seed in 1..=2 {
                let f = case(seed, planner);
                eprintln!(
                    "dst unseen {name} {planner:?} s{seed}: in view {:.2} of {} periods, \
                     min clearance {:.2} m, min separation {:.2} m, orbited {:.0}°",
                    f.view_share(),
                    f.periods,
                    f.min_clearance_m,
                    f.min_separation_m,
                    f.orbit_deg,
                );
                let at = format!("{name} {planner:?} s{seed}");
                assert!(f.view_share() >= VIEW_MIN, "{at}: subject out of view");
                assert!(f.min_clearance_m >= MARGIN_M, "{at}: into a trunk's margin");
                assert!(
                    f.min_separation_m >= follow::SEPARATION_M,
                    "{at}: inside the separation"
                );
            }
        }
    }
}

#[test]
fn dst_depth_guard_back_away_is_held_short_of_unseen_space() {
    for planner in PLANNERS {
        let mut s = depth_sim("unseen_back_away", 1, planner);
        s.stand = vec![([-BEHIND_S_M, 0.0], TRUNK_R_M)];
        s.boot();
        launch_to_hold(&mut s, ALT_M);
        let lease_id = crate::forest::lease(&mut s);
        let start = s.fc.truth();
        let mut goal = Goal::new(GoalKind::Position, start, 3.0);
        goal.yaw = Some(0.0);
        let t0 = s.now;
        s.walkers = vec![Walker {
            path: vec![
                [start[0] + 12.0, start[1]],
                [start[0] + PERSON_STOP_N_M, start[1]],
            ],
            speed: follow::WALK_MPS,
            start_ms: t0,
        }];
        let walk_ms = ((12.0 - PERSON_STOP_N_M) / follow::WALK_MPS * 1000.0) as u64;
        let mut meter = Meter::new(1, [start[0], start[1]], 1.0);
        let mut back = 0.0f32;
        while s.now - t0 < walk_ms + STAND_MS {
            if (s.now - t0).is_multiple_of(GOAL_MS) {
                s.request(Request::Goal {
                    lease_id,
                    goal: goal.clone(),
                });
                let batch = s.tracker_batch();
                let now = s.now;
                s.p.on_feed("vision.detection", &batch, now);
            }
            s.step();
            let t = s.fc.truth();
            meter.tick([t[0], t[1]], s.stand_clearance_m());
            back = back.max(start[0] - t[0]);
            if crate::bench::trace_on() && (s.now - t0).is_multiple_of(1000) {
                crate::bench::trace_line(&s, t0, [start[0], start[1]]);
            }
        }
        let m = meter.finish();
        eprintln!("dst unseen_back_away {planner:?}: backed {back:.2} m, {m:?}");
        assert!(
            !m.collision,
            "{planner:?}: backed into the unseen trunk: {m:?}"
        );
        assert_eq!(m.margin_entries, 0, "{planner:?}: into the margin: {m:?}");
        assert!(back <= BACK_MAX_M, "{planner:?}: backed {back:.2} m blind");
    }
}
