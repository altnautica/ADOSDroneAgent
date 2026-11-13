use super::*;
use ados_protocol::pilot::Candidates;

fn follow_goal(n: f32, e: f32, subject: V2, radius: f32, angle_deg: f32) -> Goal {
    let mut g = Goal::new(GoalKind::Follow, [n, e, -4.0], 5.0);
    g.subject_ne = Some(subject);
    g.candidates = Some(Candidates {
        ring_center_ne: subject,
        radius_m: radius,
        angle_deg,
        angle_span_deg: 90.0,
    });
    g
}

fn obs_with(points: &[V2], center: V2) -> Obstacles {
    let mut o = Obstacles::new();
    o.grid.reset(center);
    for p in points {
        o.grid.mark(*p);
    }
    o.grid.transform();
    o
}

#[test]
fn clear_goal_is_kept() {
    let o = obs_with(&[], [0.0, 0.0]);
    let p = plan(
        &o,
        &follow_goal(5.0, 0.0, [10.0, 0.0], 5.0, 180.0),
        [0.0, 0.0],
        1.5,
    );
    assert_eq!(p.state, AvoidState::Clear);
    assert_eq!(p.target, Some([5.0, 0.0]));
}

#[test]
fn trunk_on_the_goal_gives_a_detour() {
    // A 0.5 m trunk sits on the follow goal 5 m behind the subject.
    let mut trunk = Vec::new();
    for di in -2..=2 {
        for dj in -2..=2 {
            trunk.push([5.0 + di as f32 * 0.1, dj as f32 * 0.1]);
        }
    }
    let o = obs_with(&trunk, [0.0, 0.0]);
    let p = plan(
        &o,
        &follow_goal(5.0, 0.0, [10.0, 0.0], 5.0, 180.0),
        [0.0, 0.0],
        1.5,
    );
    assert_eq!(p.state, AvoidState::Detour);
    let g = p.goal.unwrap();
    assert!(clearance(&o.grid, g) >= 1.5);
    assert!(geo::dist(p.target.unwrap(), [0.0, 0.0]) <= LOOKAHEAD_M + 0.3);
    assert!(p.path.len() <= 16);
}

#[test]
fn wall_around_every_candidate_blocks() {
    // A full ring wall at 2 m around the vehicle: nothing reachable.
    let mut wall = Vec::new();
    for k in 0..360 {
        let b = (k as f32).to_radians();
        wall.push([2.0 * b.cos(), 2.0 * b.sin()]);
    }
    let o = obs_with(&wall, [0.0, 0.0]);
    let p = plan(
        &o,
        &follow_goal(6.0, 0.0, [11.0, 0.0], 5.0, 180.0),
        [0.0, 0.0],
        0.8,
    );
    assert_eq!(p.state, AvoidState::Blocked);
    assert_eq!(p.target, None);
}

#[test]
fn candidate_without_line_of_sight_is_rejected() {
    // A long east-west wall between the vehicle side and the subject, with the
    // goal on the wall: every candidate behind the wall sees the subject, but
    // the ones south of it (vehicle side) do not.
    let mut wall = Vec::new();
    for k in -60..=60 {
        wall.push([7.5, k as f32 * 0.1]);
    }
    let o = obs_with(&wall, [0.0, 0.0]);
    let goal = follow_goal(7.5, 0.0, [10.0, 0.0], 2.5, 180.0);
    let p = plan(&o, &goal, [0.0, 0.0], 1.0);
    if let Some(g) = p.goal {
        assert!(
            o.line_of_sight(g, [10.0, 0.0]),
            "chosen candidate {g:?} cannot see the subject"
        );
    }
}

#[test]
fn along_walks_the_polyline() {
    let p = along(&[[0.0, 0.0], [1.0, 0.0], [1.0, 3.0]], 2.0);
    assert!((p[0] - 1.0).abs() < 1e-5 && (p[1] - 1.0).abs() < 1e-5);
}
