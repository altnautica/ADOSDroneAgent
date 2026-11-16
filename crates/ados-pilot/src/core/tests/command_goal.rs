//! Goals that command their whole velocity (`vel_mode: command`), and the
//! safety brake's priority over the shaper.

use ados_protocol::pilot::{AvoidMode, Goal, GoalKind, VelMode};

use super::harness::wall;
use super::shaping::{fly_for, flying, peak_acc_jerk, A_MAX, JERK};

/// A commanded-velocity follow goal toward `n` north asking `vn`.
fn commanded(n: f32, vn: f32) -> Goal {
    let mut g = Goal::new(GoalKind::Follow, [n, 0.0, -5.0], 6.0);
    g.vn = vn;
    g.vel_mode = VelMode::Command;
    g
}

/// The goal's point is 20 m ahead but it asks no velocity: the pilot adds
/// no pull toward the point and the vehicle stays where it is. Asked
/// 1.5 m/s it reaches it within the goal limits; asked zero again it stops
/// on the profile and holds where the stop ends.
#[test]
fn a_commanded_velocity_is_flown_with_no_pull_toward_the_point() {
    let (mut w, id) = flying();
    let start = w.pos;
    fly_for(&mut w, id, 3_000, |_, _| commanded(20.0, 0.0));
    let moved = (w.pos[0] - start[0]).hypot(w.pos[1] - start[1]);
    assert!(moved < 0.01, "drifted {moved:.3} m toward the point");
    let mut v = fly_for(&mut w, id, 5_000, |_, _| commanded(20.0, 1.5));
    assert!((w.vel[0] - 1.5).abs() < 0.02, "flying {:.2} m/s", w.vel[0]);
    v.extend(fly_for(&mut w, id, 5_000, |_, _| commanded(20.0, 0.0)));
    let (a, j) = peak_acc_jerk(&v);
    assert!(a <= A_MAX + 0.05, "peak acceleration {a:.2} m/s²");
    assert!(j <= JERK + 0.1, "peak jerk {j:.2} m/s³");
    assert!(w.vel[0].abs() < 0.01, "still flying {:.2} m/s", w.vel[0]);
    let stopped = w.pos;
    fly_for(&mut w, id, 3_000, |_, _| commanded(20.0, 0.0));
    let drift = (w.pos[0] - stopped[0]).hypot(w.pos[1] - stopped[1]);
    assert!(drift < 0.01, "drifted {drift:.3} m while held");
}

/// Flying a commanded 3 m/s north, a wall shows up 6 m ahead. Slowing on
/// the goal limits alone (1 m/s², 2 m/s³) would take about 6 m, into the
/// margin; the obstacle brake, on the profile's stick acceleration, cuts the
/// shaped command at once and the vehicle stops outside the 1.5 m margin.
#[test]
fn a_shaped_goal_still_stops_for_an_obstacle_within_the_stop_budget() {
    let (mut w, id) = flying();
    w.p.settings.pilot.avoid = AvoidMode::Brake;
    fly_for(&mut w, id, 6_000, |_, _| commanded(100.0, 3.0));
    assert!(
        (w.vel[0] - 3.0).abs() < 0.05,
        "cruising {:.2} m/s",
        w.vel[0]
    );
    let wall_n = w.pos[0] + 6.0;
    let mut nearest = f32::INFINITY;
    let t0 = w.now;
    while w.now < t0 + 6_000 {
        let range = wall_n - w.pos[0];
        nearest = nearest.min(range);
        w.scan_ranges = wall(0, range);
        if (w.now - t0).is_multiple_of(100) {
            let g = commanded(100.0, 3.0);
            w.req(ados_protocol::pilot::Request::Goal {
                lease_id: id,
                goal: g,
            });
        }
        w.run(20);
    }
    assert!(nearest >= 1.5, "came within {nearest:.2} m of the wall");
    assert!(w.vel[0].abs() < 0.1, "still flying {:.2} m/s", w.vel[0]);
}
