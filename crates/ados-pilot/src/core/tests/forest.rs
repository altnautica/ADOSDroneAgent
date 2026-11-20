//! Flying through trunks: the obstacle brake judges each trunk along the
//! motion (a trunk beside a clear corridor caps nothing, one on the way
//! still stops the vehicle outside the margin), and the planner holds the
//! route it chose instead of flipping between the straight way and a
//! detour from one replan to the next.

use ados_protocol::pilot::{AvoidState, Candidates, Goal, GoalKind};

use super::safety::G;
use crate::core::control::{brake_limit, keep_out_m, BRAKE_LAG_S, CREEP_SLACK_M};
use crate::geo::{self, V2};
use crate::obstacles::Obstacles;
use crate::planner::{self, plan_committed, Commit, Motion};

const A_H: f32 = 3.0;
const MARGIN: f32 = 1.5;

/// A grid of trunks, each `(centre, radius)`, every cell a trunk's surface
/// touches marked.
fn stand(trunks: &[(V2, f32)]) -> Obstacles {
    let mut o = Obstacles::new();
    o.grid.reset([0.0, 0.0]);
    for &(c, r) in trunks {
        for k in 0..72 {
            let b = (k as f32 * 5.0).to_radians();
            o.grid
                .mark(geo::add(c, geo::scale(geo::bearing_unit(b), r)));
        }
        o.grid.mark(c);
    }
    o.grid.transform();
    o
}

/// Two rows of 0.15 m trunks along the north axis, one every 2.5 m, their
/// near faces `off` metres either side of it.
fn rows(off: f32) -> Vec<(V2, f32)> {
    (0..12)
        .flat_map(|k| {
            let n = -5.0 + 2.5 * k as f32;
            [([n, off + 0.15], 0.15), ([n, -off - 0.15], 0.15)]
        })
        .collect()
}

/// How far the vehicle runs at `v0` for the FC's lag, then braking at
/// [`A_H`] from `s`: the stop the brake plans for.
fn stop_m(v0: f32, s: f32) -> f32 {
    v0 * BRAKE_LAG_S + s * s / (2.0 * A_H)
}

#[test]
fn trunks_beside_a_clear_corridor_leave_the_speed_alone() {
    // Trunk rows 2 m either side, flying 3 m/s down the middle: every trunk
    // is passed outside the margin, so none caps the speed along the way,
    // where a cap toward each trunk held it near 1 m/s.
    let o = stand(&rows(2.0));
    for vel in [[0.0, 0.0], [3.0, 0.0]] {
        let b = brake_limit(&o, [0.0, 0.0], vel, [3.0, 0.0], A_H, G, 0);
        assert!(!b.cut, "cut to {:?} ({:#x}) at {vel:?}", b.v, b.by);
        assert_eq!(b.v, [3.0, 0.0]);
    }
    // Asked across the corridor at a row, the same trunks cap.
    let side = brake_limit(&o, [0.0, 0.0], [0.0, 0.0], [0.0, 3.0], A_H, G, 0);
    assert!(side.cut && side.v[1] < 1.0, "{side:?}");
}

#[test]
fn a_trunk_straight_ahead_still_stops_the_vehicle_outside_the_margin() {
    // Between the same rows, a 0.4 m trunk on the way 5.2 m ahead, flying at
    // it at 3 m/s: the answer stops the vehicle (after running on at 3 m/s
    // for the FC's lag) outside the margin, with no sideways motion added.
    let mut trunks = rows(2.0);
    trunks.push(([5.4, 0.0], 0.2));
    let o = stand(&trunks);
    let b = brake_limit(&o, [0.0, 0.0], [3.0, 0.0], [3.0, 0.0], A_H, G, 0);
    assert!(b.cut, "{b:?}");
    let room = 5.2 - MARGIN - CREEP_SLACK_M;
    assert!(
        stop_m(3.0, b.v[0]) <= room + 0.05,
        "asks {:.2} m/s: stops in {:.2} m, {room:.2} m to the margin",
        b.v[0],
        stop_m(3.0, b.v[0])
    );
    assert!(b.v[1].abs() < 1e-3, "sideways motion added: {:?}", b.v);
    // Nearer, already unable to stop outside the margin at 3 m/s: nothing
    // more toward it.
    let mut trunks = rows(2.0);
    trunks.push(([3.6, 0.0], 0.2));
    let o = stand(&trunks);
    let b = brake_limit(&o, [0.0, 0.0], [3.0, 0.0], [3.0, 0.0], A_H, G, 0);
    assert!(b.v[0] < 1e-3, "still asks {:?}", b.v);
}

#[test]
fn a_gap_narrower_than_twice_the_margin_is_never_flown_into() {
    // Two trunks across the way 4 m ahead with 2.8 m between their faces
    // (under twice the 1.5 m margin): flying straight at the gap, the brake
    // stops the vehicle short of both keep-outs, as for a trunk on the way.
    let o = stand(&[([4.2, 1.6], 0.2), ([4.2, -1.6], 0.2)]);
    let b = brake_limit(&o, [0.0, 0.0], [0.0, 0.0], [3.0, 0.0], A_H, G, 0);
    let keep = keep_out_m(MARGIN);
    // Where the straight path meets the nearer cell's keep-out: the trunk
    // faces at 1.4 m from the path, inside the keep-out.
    let entry = 4.0 - (keep * keep - 1.4f32.powi(2)).sqrt();
    assert!(
        stop_m(b.v[0], b.v[0]) <= entry + 0.1,
        "asks {:.2} m/s into the gap: stops in {:.2} m, entry {entry:.2} m",
        b.v[0],
        stop_m(b.v[0], b.v[0])
    );
    // The planner never routes through it: the goal 8 m beyond is reached
    // round the row or not at all.
    let wide: Vec<(V2, f32)> = (0..12)
        .flat_map(|k| {
            let e = 1.6 + 0.6 * k as f32;
            [([4.2, e], 0.2), ([4.2, -e], 0.2)]
        })
        .collect();
    let o = stand(&wide);
    let goal = follow_goal([12.0, 0.0], [18.0, 0.0]);
    let p = planner::plan(&o, &goal, [0.0, 0.0], MARGIN);
    let crosses_gap = p.path.windows(2).any(|w| {
        let (a, b) = (w[0], w[1]);
        if (a[0] - 4.2) * (b[0] - 4.2) > 0.0 || (b[0] - a[0]).abs() < 1e-6 {
            return false;
        }
        let t = (4.2 - a[0]) / (b[0] - a[0]);
        (a[1] + t * (b[1] - a[1])).abs() < 1.6
    });
    assert!(!crosses_gap, "routed through the gap: {:?}", p.path);
}

/// A follow goal at `at`, its subject at `subject`, with the ring of
/// detour spots round the subject.
fn follow_goal(at: V2, subject: V2) -> Goal {
    let mut g = Goal::new(GoalKind::Follow, [at[0], at[1], -4.0], 3.0);
    g.subject_ne = Some(subject);
    let back = geo::sub(at, subject);
    g.candidates = Some(Candidates {
        ring_center_ne: subject,
        radius_m: geo::norm(back),
        angle_deg: geo::bearing_of(back).to_degrees(),
        angle_span_deg: 90.0,
    });
    g
}

/// Replan `k` at the 200 ms period, the vehicle moving at 2 m/s.
fn moving(k: u64) -> Motion {
    Motion {
        speed_mps: 2.0,
        now_ms: 200 * k,
    }
}

#[test]
fn the_straight_way_at_the_margins_edge_does_not_flip_the_route() {
    // A trunk beside the straight way to the goal, its keep-out just reaching
    // the line: the vehicle's position jitters a few centimetres between
    // replans, so the straight way reads clear and blocked in turn. Planned
    // from scratch every replan the route flips between the two; held, it
    // changes once.
    let o = stand(&[([5.0, 1.62], 0.2)]);
    let goal = follow_goal([10.0, 0.0], [16.0, 0.0]);
    let mut held = Commit::default();
    let (mut fresh_flips, mut last_fresh) = (0, None);
    for k in 0..30 {
        let from = [0.0, if k % 2 == 0 { 0.06 } else { -0.06 }];
        let fresh = planner::plan(&o, &goal, from, MARGIN).state;
        if last_fresh.is_some_and(|s| s != fresh) {
            fresh_flips += 1;
        }
        last_fresh = Some(fresh);
        plan_committed(&o, &goal, from, MARGIN, moving(k), &mut held);
    }
    assert!(fresh_flips >= 10, "the jitter never flipped a fresh plan");
    assert!(held.switches <= 1, "{} route switches", held.switches);
}

#[test]
fn a_goal_behind_a_trunk_gets_one_held_detour() {
    // A 0.8 m trunk straight between the vehicle and its goal. Flown along
    // its own plan, replanned every 0.4 m: one detour round one side of the
    // trunk, held, then the straight way once past it.
    let o = stand(&[([6.0, 0.15], 0.4)]);
    let goal = follow_goal([13.0, 0.0], [19.0, 0.0]);
    let mut held = Commit::default();
    let mut pos = [0.0, 0.0];
    let mut states = Vec::new();
    let mut sides = Vec::new();
    for k in 0..60 {
        let p = plan_committed(&o, &goal, pos, MARGIN, moving(k), &mut held);
        states.push(p.state);
        let Some(t) = p.target else { break };
        if p.state == AvoidState::Detour && (5.0..7.0).contains(&pos[0]) {
            sides.push(pos[1].signum());
        }
        let step = geo::sub(t, pos);
        let d = geo::norm(step);
        if d < 0.05 {
            break;
        }
        pos = geo::add(pos, geo::scale(step, 0.4f32.min(d) / d));
        assert!(
            geo::dist(pos, [6.0, 0.15]) - 0.4 >= MARGIN - 0.2,
            "flew to {pos:?}, inside the margin"
        );
    }
    let changes = states.windows(2).filter(|w| w[0] != w[1]).count();
    assert!(states.contains(&AvoidState::Detour), "{states:?}");
    assert!(changes <= 2, "{changes} changes: {states:?}");
    assert!(held.switches <= 2, "{} route switches", held.switches);
    assert!(
        sides.windows(2).all(|w| w[0] == w[1]),
        "passed the trunk on both sides: {sides:?}"
    );
    assert!(geo::dist(pos, [13.0, 0.0]) < 0.5, "ended at {pos:?}");
}

/// True when the segments `a → b` and `c → d` cross.
fn crosses(a: V2, b: V2, c: V2, d: V2) -> bool {
    let side = |p: V2, q: V2, r: V2| {
        let (u, v) = (geo::sub(q, p), geo::sub(r, p));
        u[0] * v[1] - u[1] * v[0]
    };
    side(a, b, c) * side(a, b, d) < 0.0 && side(c, d, a) * side(c, d, b) < 0.0
}

/// Replan `k` at the 200 ms period, the vehicle held to a crawl.
fn crawling(k: u64) -> Motion {
    Motion {
        speed_mps: 0.1,
        now_ms: 200 * k,
    }
}

/// The clearance the brake flies freely at the margin: a route inside it is
/// flown at a crawl.
fn room() -> f32 {
    let room = MARGIN + planner::ROUTE_ROOM_M;
    assert!(room > keep_out_m(MARGIN));
    room
}

/// The least clearance on the grid along `a → b`, sampled every half cell.
fn least_clearance(o: &Obstacles, a: V2, b: V2) -> f32 {
    let n = (geo::dist(a, b) / 0.125).ceil() as usize;
    (0..=n)
        .filter_map(|k| {
            let p = geo::add(a, geo::scale(geo::sub(b, a), k as f32 / n as f32));
            o.grid.clearance(p)
        })
        .fold(f32::INFINITY, f32::min)
}

#[test]
fn a_straight_way_flown_at_a_crawl_is_chosen_again() {
    // From a seeded stand where a follow crawled for 7 s: the straight way
    // to the goal passes a trunk at more than the margin but inside the
    // brake's keep-out, so it is flown, at a crawl. Crawling for the stall's
    // replans, the planner chooses again: a detour that keeps the room the
    // brake flies freely near where the vehicle stalled.
    let trunks = [
        ([7.017, -3.617], 0.169),
        ([4.973, 1.037], 0.263),
        ([3.801, -4.646], 0.214),
    ];
    let o = stand(&trunks);
    let from = [6.66, -1.74];
    let goal = follow_goal([15.005, -3.160], [19.602, -7.017]);
    let target = [goal.n, goal.e];
    let least = least_clearance(&o, from, target);
    assert!((MARGIN..room()).contains(&least), "{least} m on the way");
    let mut held = Commit::default();
    let first = plan_committed(&o, &goal, from, MARGIN, moving(0), &mut held);
    assert_eq!(first.state, AvoidState::Clear, "{first:?}");
    let plans: Vec<_> = (1..=u64::from(planner::STALL_PLANS))
        .map(|k| plan_committed(&o, &goal, from, MARGIN, crawling(k), &mut held))
        .collect();
    let (p, before) = plans.split_last().expect("plans");
    assert!(before.iter().all(|p| p.state == AvoidState::Clear));
    assert_eq!(p.state, AvoidState::Detour, "{p:?}");
    let here = o.grid.clearance(from).expect("on the grid");
    let near = |p: &V2| geo::dist(*p, from) < planner::STALL_RADIUS_M;
    for w in p.path.windows(2).filter(|w| near(&w[1])) {
        let c = least_clearance(&o, w[0], w[1]);
        assert!(c >= room().min(here), "{c} m near the stall: {:?}", p.path);
    }
    // Held while the vehicle flies it: one route change in all.
    let held_on = plan_committed(&o, &goal, from, MARGIN, moving(11), &mut held);
    assert_eq!(held_on.state, AvoidState::Detour, "{held_on:?}");
    assert_eq!(held.switches, 1, "one route change");
}

#[test]
fn narrows_that_are_the_only_way_out_stay_routed() {
    // A passage wider than twice the margin, narrower than the room the brake
    // flies freely, in a fence of trunks across the whole grid: crawling
    // through it never leaves the vehicle without a route through it.
    let (a, b) = ([5.0, -1.75], [5.0, 1.75]);
    let mut trunks = vec![(a, 0.1), (b, 0.1)];
    for k in 0..40 {
        let along = 0.4 * k as f32;
        trunks.push(([5.0, 2.15 + along], 0.1));
        trunks.push(([5.0, -2.15 - along], 0.1));
    }
    let o = stand(&trunks);
    let from = [3.2, 0.0];
    // The subject close enough that every detour spot is on the grid.
    let goal = follow_goal([9.0, 0.0], [12.0, 0.0]);
    let least = least_clearance(&o, from, [goal.n, goal.e]);
    assert!(
        (MARGIN..room()).contains(&least),
        "{least} m in the passage"
    );
    let mut held = Commit::default();
    for k in 0..3 * u64::from(planner::STALL_PLANS) {
        let p = plan_committed(&o, &goal, from, MARGIN, crawling(k), &mut held);
        assert_ne!(p.state, AvoidState::Blocked, "replan {k}: {p:?}");
        let through = p.path.windows(2).any(|w| crosses(w[0], w[1], a, b));
        assert!(through, "replan {k} left the only way: {:?}", p.path);
    }
}
