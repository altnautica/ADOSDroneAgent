//! A survey drift in a mine, flown GNSS-denied under avoid `brake`: a goto
//! 42 m down a 4.9 m drift whose walls begin 10 m on, the EKF's velocity
//! reading 0.3 m/s short across the drift for the goto's first 5 s (a
//! flow-fused estimate lagging the acceleration). Under the GNSS-denied
//! speed cap the pilot keeps the FC on its own reference wherever the cap
//! leaves room, so the copter stays near the middle of the drift, the
//! straight way to the goal stays open and the goto reaches the end without
//! holding blocked, never inside the obstacle margin. Any hold keeps its
//! point.

use ados_pilot::config::PlannerMode;
use ados_protocol::pilot::{AvoidMode, Owner, Request};

use crate::metrics::{Meter, RunMetrics, REACH_M};
use crate::scenarios::launch_to_hold;
use crate::sim::Sim;

/// The drift: its walls this far either side of its middle (`n = 0`), from
/// this far east to this far, m.
const HALF_WIDTH_M: f32 = 2.45;
const WALL_FROM_E_M: f32 = 10.0;
const WALL_TO_E_M: f32 = 50.0;
/// Wall posts: radius and spacing, m (overlapping: no gap through them).
const POST_R_M: f32 = 0.25;
const POST_STEP_M: f32 = 0.4;
/// The goal, down the middle of the drift, and the flight height, m.
const GOAL_E_M: f32 = 42.0;
const ALT_M: f32 = 2.0;
/// The EKF velocity's shortfall across the drift (north), m/s, and for how
/// long from the goto's start, ms.
const BIAS_MPS: f32 = 0.3;
const BIAS_MS: u64 = 5_000;
const TIMEOUT_MS: u64 = 90_000;
/// How often the published state is read, ms.
const STATE_MS: u64 = 200;
/// A hold's point is where the copter first slows under this, m/s.
const HELD_MPS: f32 = 0.2;
/// The most a hold may drift from its point, m.
pub const HOLD_DRIFT_M: f32 = 0.3;

fn drift() -> Vec<([f32; 2], f32)> {
    let n = ((WALL_TO_E_M - WALL_FROM_E_M) / POST_STEP_M).round() as i32;
    (0..=n)
        .flat_map(|k| {
            let e = WALL_FROM_E_M + POST_STEP_M * k as f32;
            [-1.0f32, 1.0].map(|side| ([side * (HALF_WIDTH_M + POST_R_M), e], POST_R_M))
        })
        .collect()
}

/// One run down the drift.
#[derive(Debug)]
pub struct Run {
    pub m: RunMetrics,
    /// The pilot published the goto held `blocked`.
    pub blocked: bool,
    /// The most any hold drifted from its point, m.
    pub hold_drift_m: f32,
}

/// Fly the drift on `seed` with `planner`.
pub fn mine_drift(seed: u64, planner: PlannerMode) -> Run {
    let mut s = Sim::new("mine_drift", seed, |c| {
        c.pilot.planner = planner;
        c.pilot.avoid = AvoidMode::Brake;
    });
    // External navigation is the horizontal position source: no GNSS, so
    // the low-battery failsafe lands (the ADOS profile's row without GNSS).
    s.fc.params.insert("EK3_SRC1_POSXY".into(), 6.0);
    s.fc.params.insert("BATT_FS_LOW_ACT".into(), 1.0);
    s.stand = drift();
    s.boot();
    launch_to_hold(&mut s, ALT_M);
    assert!(
        s.p.nav_cap(s.now).is_some(),
        "flown under the GNSS-denied cap"
    );
    let r = s.request(Request::Goto {
        n: 0.0,
        e: GOAL_E_M,
        d: -ALT_M,
        yaw: None,
    });
    assert_eq!(s.p.owner(), Owner::Goto, "goto refused: {r:?}");
    let t = s.fc.truth();
    let mut meter = Meter::new(seed, [t[0], t[1]], GOAL_E_M);
    let (mut blocked, mut hold_drift_m) = (false, 0.0f32);
    let mut held_at: Option<[f32; 2]> = None;
    let t0 = s.now;
    while s.now - t0 < TIMEOUT_MS {
        s.fc.vel_bias = if s.now - t0 < BIAS_MS {
            [BIAS_MPS, 0.0]
        } else {
            [0.0; 2]
        };
        s.step();
        let t = s.fc.truth();
        meter.tick([t[0], t[1]], s.stand_clearance_m());
        if (s.now - t0).is_multiple_of(STATE_MS) {
            let hold = s.p.state(s.now).hold_reason;
            blocked |= hold.as_deref() == Some("blocked");
            let speed = (s.fc.vel[0] + s.fc.vel_bias[0]).hypot(s.fc.vel[1] + s.fc.vel_bias[1]);
            held_at = match (hold, held_at) {
                (None, _) => None,
                (Some(_), None) if speed < HELD_MPS => Some([t[0], t[1]]),
                (Some(_), at) => at,
            };
            if let Some(a) = held_at {
                hold_drift_m = hold_drift_m.max((t[0] - a[0]).hypot(t[1] - a[1]));
            }
        }
        if crate::bench::trace_on() && (s.now - t0).is_multiple_of(1000) {
            crate::bench::trace_line(&s, t0, [0.0, GOAL_E_M]);
        }
        if t[0].hypot(t[1] - GOAL_E_M) <= REACH_M {
            meter.reached((s.now - t0) as f32 / 1000.0);
            break;
        }
    }
    Run {
        m: meter.finish(),
        blocked,
        hold_drift_m,
    }
}

#[test]
fn dst_mine_drift_goto_reaches_the_end_on_a_lagging_estimate() {
    for planner in [PlannerMode::Reactive, PlannerMode::Trajectory] {
        for seed in crate::SEEDS {
            let r = mine_drift(seed, planner);
            eprintln!("dst mine_drift {planner:?} s{seed}: {r:?}");
            let m = &r.m;
            assert!(
                m.success,
                "{planner:?} s{seed}: never reached the end: {r:?}"
            );
            assert!(!r.blocked, "{planner:?} s{seed}: held blocked: {r:?}");
            assert!(!m.collision, "{planner:?} s{seed}: hit a wall: {r:?}");
            assert_eq!(
                m.margin_entries, 0,
                "{planner:?} s{seed}: into the margin: {r:?}"
            );
            assert!(
                r.hold_drift_m <= HOLD_DRIFT_M,
                "{planner:?} s{seed}: a hold drifted {:.2} m",
                r.hold_drift_m
            );
        }
    }
}
