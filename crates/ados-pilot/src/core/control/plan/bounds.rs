//! Where a planner-driven setpoint may be: a skill's goal carried forward
//! between its updates, and the geofence and altitude bounds.

use ados_protocol::pilot::Goal;

use crate::geo::{self, V2};

/// A goal is carried forward on its own velocity for at most this long
/// after it arrived: past it the skill has stopped sending, and its last
/// position is the best guess.
const GOAL_CARRY_MAX_MS: u64 = 500;

/// Where a goal that arrived `since_ms` ago is now, carried forward on its
/// own velocity for at most [`GOAL_CARRY_MAX_MS`].
pub(super) fn live_goal(g: &Goal, since_ms: u64) -> V2 {
    let dt = since_ms.min(GOAL_CARRY_MAX_MS) as f32 / 1000.0;
    [g.n + g.vn * dt, g.e + g.ve * dt]
}

/// `p`, or the point on the geofence circle toward it when it lies outside.
pub(super) fn inside_fence(p: V2, home: V2, max_dist: f32) -> V2 {
    let rel = geo::sub(p, home);
    let d = geo::norm(rel);
    if d <= max_dist {
        return p;
    }
    geo::add(home, geo::scale(rel, max_dist / d))
}

/// The setpoint down position and down velocity held to an AGL within
/// `[floor, ceil]`, from the vehicle's `d_now` and its height above ground.
/// A bound is held, never pushed: a vehicle already past one keeps its
/// height (it may still move back inside), so a rangefinder stuck low or
/// high never drives a climb or a descent. A down velocity the clamp cut
/// is dropped. Without a height above ground both pass through.
pub(super) fn clamp_d(
    goal_d: f32,
    vd: f32,
    d_now: f32,
    agl: Option<f32>,
    floor: f32,
    ceil: f32,
) -> (f32, f32) {
    let Some(a) = agl else {
        return (goal_d, vd);
    };
    let ground_d = d_now + a; // NED: the ground is below
    let (top, bottom) = (ground_d - ceil, ground_d - floor);
    let d = goal_d.min(bottom.max(d_now)).max(top.min(d_now));
    let vd = if (d > goal_d && vd < 0.0) || (d < goal_d && vd > 0.0) {
        0.0
    } else {
        vd
    };
    (d, vd)
}
