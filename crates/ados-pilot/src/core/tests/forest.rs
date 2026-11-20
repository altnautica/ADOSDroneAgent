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
use crate::planner::{self, plan_committed, Commit};

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
        plan_committed(&o, &goal, from, MARGIN, &mut held);
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
    for _ in 0..60 {
        let p = plan_committed(&o, &goal, pos, MARGIN, &mut held);
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
