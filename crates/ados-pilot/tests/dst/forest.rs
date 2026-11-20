//! Follow flights through trunks: a skill's follow goal behind a walker,
//! flown by the pilot's planner and obstacle brake against a simulated
//! planar lidar casting each scan on the stand ([`Sim::stand`]). They check
//! what a competent pilot does in a forest: full speed down a clear corridor
//! between trunk rows, never into a gap the vehicle does not fit, a held
//! detour around a trunk in the way, and keeping up with a walker through a
//! seeded random stand, never inside the margin.

use ados_protocol::pilot::{Candidates, Goal, GoalKind, Holder, Request};

use crate::scenarios::launch_to_hold;
use crate::sim::Sim;
use ados_pilot::core::Reply;

/// The pilot's obstacle margin in an open place (the default), m.
const MARGIN_M: f32 = 1.5;
/// Follow height, m.
const ALT_M: f32 = 4.0;
/// The follow distance behind the walker, m.
const BEHIND_M: f32 = 6.0;
/// A walker's pace, m/s.
const WALK_MPS: f32 = 1.4;
/// The skill's goal period, ms (the follow skill's 10 Hz).
const GOAL_MS: u64 = 100;

fn lease(s: &mut Sim) -> u64 {
    let r = s.request(Request::LeaseAcquire {
        holder: Holder {
            plugin_id: "com.example.follow".into(),
            skill_id: "follow-me".into(),
        },
        kind: "motion".into(),
    });
    match r {
        Reply::Lease(l) => l.lease_id.expect("lease granted"),
        other => panic!("{other:?}"),
    }
}

/// A follow goal `BEHIND_M` behind the walker at `walker` heading `head`
/// (unit), with the walker's velocity as its feed-forward and the ring of
/// detour spots around the walker.
fn follow_goal(walker: [f32; 2], head: [f32; 2], speed: f32, max_mps: f32) -> Goal {
    let at = [
        walker[0] - BEHIND_M * head[0],
        walker[1] - BEHIND_M * head[1],
    ];
    let mut g = Goal::new(GoalKind::Follow, [at[0], at[1], -ALT_M], max_mps);
    g.vn = head[0] * speed;
    g.ve = head[1] * speed;
    g.subject_ne = Some(walker);
    g.candidates = Some(Candidates {
        ring_center_ne: walker,
        radius_m: BEHIND_M,
        angle_deg: ados_pilot::geo::atan2(-head[1], -head[0]).to_degrees(),
        angle_span_deg: 90.0,
    });
    g
}

/// What one follow flight measured.
#[derive(Debug, Default)]
pub struct Walk {
    /// Nearest any trunk surface came to the vehicle, m.
    pub min_clearance_m: f32,
    /// Horizontal speed per goal period while the walker walked, m/s.
    pub speeds: Vec<f32>,
    /// The vehicle's distance to its goal per goal period, m.
    pub to_goal_m: Vec<f32>,
    /// Vehicle positions per goal period.
    pub track: Vec<[f32; 2]>,
    /// Route switches over the walk ([`ados_pilot::core::Pilot::route_switches`]).
    pub switches: u32,
}

impl Walk {
    pub fn mean_speed(&self) -> f32 {
        self.speeds.iter().sum::<f32>() / self.speeds.len().max(1) as f32
    }
}

/// Up at the hold, a lease, then the walker walks `trail` (a polyline) at
/// `pace` with the goal following it; the run goes on `tail_ms` after the
/// walker stops.
fn walk(s: &mut Sim, trail: &[[f32; 2]], pace: f32, max_mps: f32, tail_ms: u64) -> Walk {
    s.boot();
    launch_to_hold(s, ALT_M);
    let lease_id = lease(s);
    let mut w = Walk {
        min_clearance_m: s.stand_clearance_m(),
        ..Walk::default()
    };
    let switches0 = s.p.route_switches();
    let mut leg = 0;
    let mut at = trail[0];
    let mut stopped_ms = None;
    loop {
        // The walker steps along the trail.
        let mut step = pace * GOAL_MS as f32 / 1000.0;
        while step > 0.0 && leg + 1 < trail.len() {
            let to = trail[leg + 1];
            let d = (to[0] - at[0]).hypot(to[1] - at[1]);
            if d <= step {
                at = to;
                step -= d;
                leg += 1;
            } else {
                at = [
                    at[0] + (to[0] - at[0]) * step / d,
                    at[1] + (to[1] - at[1]) * step / d,
                ];
                step = 0.0;
            }
        }
        let walking = leg + 1 < trail.len();
        let (a, b) = (
            trail[leg.min(trail.len() - 2)],
            trail[(leg + 1).min(trail.len() - 1)],
        );
        let len = (b[0] - a[0]).hypot(b[1] - a[1]).max(1e-3);
        let head = [(b[0] - a[0]) / len, (b[1] - a[1]) / len];
        let speed = if walking { pace } else { 0.0 };
        let goal = follow_goal(at, head, speed, max_mps);
        let g = [goal.n, goal.e];
        s.request(Request::Goal { lease_id, goal });
        for _ in 0..GOAL_MS / ados_pilot::core::TICK_MS {
            s.step();
            w.min_clearance_m = w.min_clearance_m.min(s.stand_clearance_m());
        }
        let p = [s.fc.pos[0], s.fc.pos[1]];
        w.track.push(p);
        w.to_goal_m.push((g[0] - p[0]).hypot(g[1] - p[1]));
        if walking {
            w.speeds.push(s.fc.vel[0].hypot(s.fc.vel[1]));
        } else {
            let since = *stopped_ms.get_or_insert(s.now);
            if s.now - since >= tail_ms {
                break;
            }
        }
    }
    w.switches = s.p.route_switches() - switches0;
    w
}

/// Two rows of trunks either side of a straight trail, their near faces 2 m
/// off it (0.12-0.18 m radius, one every 2.5 m), the walker 14 m ahead of
/// the vehicle: the vehicle catches up at its goal's 3 m/s, never held to
/// the walker's pace by the trunks beside it, and never inside the margin.
pub fn forest_corridor(seed: u64) -> Sim {
    let mut s = Sim::new("forest_corridor", seed, |_| {});
    let phase = s.rng.range(0.0, 2.5);
    for k in 0..40 {
        let n = -5.0 + phase + 2.5 * k as f32;
        for side in [-1.0f32, 1.0] {
            let r = s.rng.range(0.12, 0.18);
            s.stand.push(([n, side * (2.0 + r)], r));
        }
    }
    let w = walk(&mut s, &[[14.0, 0.0], [70.0, 0.0]], WALK_MPS, 3.0, 2_000);
    // Catching up: once up to 1.6 m/s from the hover (the goal limits'
    // ramp), until within 3 m of the goal, the vehicle never flies slower.
    let caught = w
        .to_goal_m
        .iter()
        .position(|d| *d < 3.0)
        .expect("caught up with the goal");
    let up = w
        .speeds
        .iter()
        .position(|v| *v >= 1.6)
        .expect("up to speed");
    let window = &w.speeds[up..caught.min(w.speeds.len())];
    assert!(
        window.len() >= 20,
        "a catch-up of at least 2 s: {}",
        window.len()
    );
    let slowest = window.iter().copied().fold(f32::INFINITY, f32::min);
    eprintln!(
        "forest_corridor s{seed}: slowest {slowest:.2} m/s over {:.1} s of catch-up, mean {:.2} m/s, clearance {:.2} m, {} switches",
        window.len() as f32 / 10.0,
        w.mean_speed(),
        w.min_clearance_m,
        w.switches
    );
    assert!(slowest >= 1.6, "held to {slowest:.2} m/s between the rows");
    assert!(
        w.min_clearance_m >= MARGIN_M,
        "{:.2} m from a trunk",
        w.min_clearance_m
    );
    assert_eq!(
        w.switches, 0,
        "the straight way down the corridor is never left"
    );
    s
}

/// A row of trunks across the trail with one gap narrower than twice the
/// margin (2.3-2.9 m between the surfaces) straight on the way: the vehicle
/// never enters the gap; it goes round the row's end or waits short of it,
/// never inside the margin.
pub fn forest_gap(seed: u64) -> Sim {
    let mut s = Sim::new("forest_gap", seed, |_| {});
    let gap = s.rng.range(2.3, 2.9);
    let r = 0.2;
    let half = gap / 2.0 + r;
    let mut e = half;
    while e <= 8.0 {
        s.stand.push(([12.0, e], r));
        s.stand.push(([12.0, -e], r));
        e += 0.9;
    }
    let w = walk(&mut s, &[[20.0, 0.0], [26.0, 0.0]], WALK_MPS, 3.0, 20_000);
    let through = w
        .track
        .iter()
        .any(|p| (p[0] - 12.0).abs() < 0.5 && p[1].abs() < half);
    eprintln!(
        "forest_gap s{seed}: gap {gap:.2} m, clearance {:.2} m, end {:?}, {} switches",
        w.min_clearance_m,
        w.track.last(),
        w.switches
    );
    assert!(!through, "flew into a {gap:.2} m gap");
    assert!(
        w.min_clearance_m >= MARGIN_M,
        "{:.2} m from a trunk",
        w.min_clearance_m
    );
    s
}

/// A 0.8 m trunk straight between the vehicle and its goal: the planner
/// chooses a detour round it and holds that one route (into the detour and
/// back to the straight way once past, nothing else), never inside the
/// margin.
pub fn forest_detour(seed: u64) -> Sim {
    let mut s = Sim::new("forest_detour", seed, |_| {});
    let e = s.rng.range(-0.3, 0.3);
    s.stand.push(([9.0, e], 0.4));
    let w = walk(&mut s, &[[22.0, 0.0], [23.0, 0.0]], WALK_MPS, 3.0, 20_000);
    let end = *w.to_goal_m.last().expect("flew");
    eprintln!(
        "forest_detour s{seed}: {} switches, {end:.2} m from the goal, clearance {:.2} m",
        w.switches, w.min_clearance_m
    );
    assert!(
        w.switches <= 2,
        "{} route switches round one trunk",
        w.switches
    );
    assert!(end < 1.5, "never reached the goal: {end:.2} m off");
    assert!(
        w.min_clearance_m >= MARGIN_M,
        "{:.2} m from a trunk",
        w.min_clearance_m
    );
    s
}

/// A seeded stand: trunks (0.12-0.3 m) on a jittered 4 m lattice, none
/// within 1.8 m of a walker's trail that turns every 12 m, and the walker
/// walking it at 1.4 m/s. The vehicle keeps up through the turns, weaving
/// where the follow point cuts a corner into the trees, never inside the
/// margin and without flipping between routes.
pub fn forest_stand(seed: u64) -> Sim {
    let mut s = Sim::new("forest_stand", seed, |_| {});
    let mut trail = vec![[8.0f32, 0.0f32]];
    let mut b = 0.0f32;
    for _ in 0..6 {
        b = (b + s.rng.range(-35.0, 35.0)).clamp(-40.0, 40.0);
        let last = *trail.last().expect("a start");
        let r = b.to_radians();
        trail.push([last[0] + 12.0 * r.cos(), last[1] + 12.0 * r.sin()]);
    }
    let from_trail = |p: [f32; 2]| {
        trail
            .windows(2)
            .map(|w| {
                let ab = [w[1][0] - w[0][0], w[1][1] - w[0][1]];
                let ap = [p[0] - w[0][0], p[1] - w[0][1]];
                let t = ((ap[0] * ab[0] + ap[1] * ab[1]) / (ab[0] * ab[0] + ab[1] * ab[1]))
                    .clamp(0.0, 1.0);
                (ap[0] - t * ab[0]).hypot(ap[1] - t * ab[1])
            })
            .fold(f32::INFINITY, f32::min)
    };
    for i in -1..22 {
        for j in -6..=6 {
            let c = [
                4.0 * i as f32 + s.rng.range(-1.2, 1.2),
                4.0 * j as f32 + s.rng.range(-1.2, 1.2),
            ];
            let r = s.rng.range(0.12, 0.3);
            if from_trail(c) - r >= 1.8 && c[0].hypot(c[1]) - r >= 3.0 {
                s.stand.push((c, r));
            }
        }
    }
    let w = walk(&mut s, &trail, WALK_MPS, 3.0, 5_000);
    let walk_s = w.speeds.len() as f32 / 10.0;
    let end = *w.to_goal_m.last().expect("flew");
    eprintln!(
        "forest_stand s{seed}: {} trunks, mean {:.2} m/s over {walk_s:.0} s, clearance {:.2} m, {} switches, {end:.1} m from the goal at the end",
        s.stand.len(),
        w.mean_speed(),
        w.min_clearance_m,
        w.switches
    );
    assert!(
        w.min_clearance_m >= MARGIN_M,
        "{:.2} m from a trunk",
        w.min_clearance_m
    );
    assert!(
        w.mean_speed() >= 0.9 * WALK_MPS,
        "kept to {:.2} m/s behind a {WALK_MPS} m/s walker",
        w.mean_speed()
    );
    assert!(end < 3.0, "left {end:.1} m behind its goal");
    assert!(
        w.switches as f32 <= walk_s / 3.0,
        "{} route switches in {walk_s:.0} s",
        w.switches
    );
    s
}
