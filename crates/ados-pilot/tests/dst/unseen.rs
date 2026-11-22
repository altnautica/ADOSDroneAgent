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
//! back-away is held there until the nose has turned to look, and the
//! vehicle never backs blind into it or into the trunk's margin.
//!
//! A person walking at a vehicle hovering low over a street it flew up:
//! the way back was seen on the way in, but another walker has since
//! crossed into it out of the camera's view. The back-away must not run
//! blind down the street into them: it sidesteps along ways the camera
//! has looked along, turning to look first.

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
/// The most the vehicle may step backward (south), summed over the ticks
/// its nose faces the person's half (not behind it), m: what the position
/// hold jitters, not a back-away. Measured 0.50 m (reactive) and 0.74 m
/// (trajectory); the bound leaves a quarter metre over the larger.
const BACK_MAX_M: f32 = 1.0;
/// How much nearer than where they stopped the person may come, m.
const PERSON_SLACK_M: f32 = 0.25;

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
        let (mut back, mut blind) = (0.0f32, 0.0f32);
        let mut prev_n = start[0];
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
            meter.person(s.walker_sep_m());
            back = back.max(start[0] - t[0]);
            let looking_back = s.fc.att[2].abs() > std::f32::consts::FRAC_PI_2;
            if !looking_back {
                blind += (prev_n - t[0]).max(0.0);
            }
            prev_n = t[0];
            if crate::bench::trace_on() && (s.now - t0).is_multiple_of(1000) {
                crate::bench::trace_line(&s, t0, [start[0], start[1]]);
            }
        }
        let m = meter.finish();
        eprintln!(
            "dst unseen_back_away {planner:?}: backed {back:.2} m, {blind:.2} m blind, {m:?}"
        );
        assert!(
            !m.collision,
            "{planner:?}: backed into the unseen trunk: {m:?}"
        );
        assert_eq!(m.margin_entries, 0, "{planner:?}: into the margin: {m:?}");
        assert!(
            blind <= BACK_MAX_M,
            "{planner:?}: backed {blind:.2} m blind"
        );
        assert!(
            m.min_person_sep_m >= PERSON_STOP_N_M - PERSON_SLACK_M,
            "{planner:?}: the person came to {:.2} m: {m:?}",
            m.min_person_sep_m
        );
    }
}

/// The hover point up the street, north of the start, the low height the
/// walkers can be met at, the walker coming down the street from its far
/// end, and the one stepping into it from the side this far behind the
/// hover point (out to its middle and back), m.
const STREET_N_M: f32 = 12.0;
const STREET_ALT_M: f32 = 1.5;
const COMING_FROM_N_M: f32 = 30.0;
const CROSSING_BEHIND_M: f32 = 3.5;
const CROSSING_FROM_M: f32 = 14.0;
/// When each walker sets off after the goal's first, ms: the one coming
/// down the street reaches the vehicle's kept distance about 18 s in, and
/// the one behind it reaches the street's middle about 20 s in, where a
/// back-away down the street at walking pace would meet them.
const COMING_AT_MS: u64 = 8_000;
const CROSSING_AT_MS: u64 = 10_500;
const STREET_MS: u64 = 30_000;
/// The least the vehicle keeps off the walker behind it, m: backing down
/// the street blind, it met them (0.4 m).
const MIN_BEHIND_SEP_M: f32 = 2.0;
/// The least the vehicle keeps off every walker, m: the separation the
/// guard keeps.
const MIN_PERSON_SEP_M: f32 = 2.5;

#[test]
fn dst_depth_guard_back_away_never_backs_blind_into_a_walker() {
    for planner in PLANNERS {
        let mut s = depth_sim("unseen_walker_behind", 1, planner);
        s.boot();
        launch_to_hold(&mut s, STREET_ALT_M);
        let lease_id = crate::forest::lease(&mut s);
        let start = s.fc.truth();
        let hover = [start[0] + STREET_N_M, start[1]];
        let mut goal = Goal::new(GoalKind::Position, [hover[0], hover[1], start[2]], 2.0);
        goal.yaw = Some(0.0);
        let t0 = s.now;
        let behind = hover[0] - CROSSING_BEHIND_M;
        s.walkers = vec![
            Walker {
                path: vec![
                    [start[0] + COMING_FROM_N_M, hover[1]],
                    [hover[0] + 1.0, hover[1]],
                ],
                speed: follow::WALK_MPS,
                start_ms: t0 + COMING_AT_MS,
            },
            Walker {
                path: vec![[behind, hover[1] - CROSSING_FROM_M], [behind, hover[1]]],
                speed: follow::WALK_MPS,
                start_ms: t0 + CROSSING_AT_MS,
            },
        ];
        let mut meter = Meter::new(1, [start[0], start[1]], STREET_N_M);
        let mut behind_sep = f32::INFINITY;
        while s.now - t0 < STREET_MS {
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
            meter.tick([t[0], t[1]], s.walker_clearance_m());
            meter.person(s.walker_sep_m());
            let b = s.walkers[1].at(s.now);
            behind_sep = behind_sep.min((b[0] - t[0]).hypot(b[1] - t[1]));
            if crate::bench::trace_on() && (s.now - t0).is_multiple_of(100) {
                crate::bench::trace_line(&s, t0, hover);
            }
        }
        let m = meter.finish();
        eprintln!(
            "dst unseen_walker_behind {planner:?}: {behind_sep:.2} m off the walker behind, {m:?}"
        );
        assert!(!m.collision, "{planner:?}: met a walker: {m:?}");
        assert!(
            behind_sep >= MIN_BEHIND_SEP_M,
            "{planner:?}: backed to {behind_sep:.2} m of the walker it never saw"
        );
        assert!(
            m.min_person_sep_m >= MIN_PERSON_SEP_M,
            "{planner:?}: {:.2} m off a walker: {m:?}",
            m.min_person_sep_m
        );
    }
}
