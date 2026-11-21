//! What a skill goal or a goto asks before any limit: its speed limit, the
//! velocity toward its target, and the ask published while the pilot holds.

use ados_protocol::pilot::{AvoidState, Goal, GoalKind, VelMode};

use super::super::motion_ref::{approach, APPROACH_DECEL_SHARE};
use super::DETOUR_MIN_MPS;
use crate::core::trace::Ask;
use crate::core::{limit, Pilot};
use crate::geo::{self, V2};
use crate::model::{limits, GoalLimits};
use crate::planner::{self, Plan};

/// The most a commanded speed is raised along a detour longer than the
/// straight way to its end.
pub const DETOUR_STRETCH_MAX: f32 = 1.5;

impl Pilot {
    /// The goal's speed limit, its own limit without the caps, and the
    /// [`limit`] bit of the one that binds: `max_speed` held to the
    /// profile's limit (`speed`), to any nav cap (`nav_cap`) and to the
    /// sensing rule's speed (`sensing`), never under 0.5 m/s (the brake's
    /// seen-space limit stops the rest).
    pub(super) fn goal_v_cap(&self, max_speed: f32, now_ms: u64) -> (f32, f32, u16) {
        let mut v_cap = max_speed.min(self.limits_now(now_ms).v_h).max(0.5);
        let own = max_speed
            .min(limits(self.settings.pilot.profile).v_h)
            .max(0.5);
        let mut by = if v_cap < own - 1e-3 {
            limit::NAV_CAP
        } else {
            limit::SPEED
        };
        if let Some(s) = self.sensing_cap().map(|s| s.max(0.5)) {
            if s < v_cap - 1e-3 {
                v_cap = s;
                by = limit::SENSING;
            }
        }
        (v_cap, own, by)
    }

    /// The velocity a goal asks with its target `to` away, before any
    /// limit. A commanded velocity is asked as it is ([`VelMode::Command`]);
    /// a feed-forward gets the pilot's pull toward the target, slowing into
    /// it on the goal limits ([`approach`]). On a detour, and for a vantage
    /// goal, the pilot flies its path toward the target with no feed-forward:
    /// a commanded velocity then only sets the speed (at least
    /// [`DETOUR_MIN_MPS`]), and a commanded stop stays a stop. A detour's
    /// target is a point [`planner::LOOKAHEAD_M`] along its route, moved on
    /// every replan: the approach slows into the route's end, not into that
    /// point, so a detour is not flown at the speed of a stop 2 m ahead.
    pub(super) fn goal_ask(&self, goal: Option<&Goal>, to: V2, gl: &GoalLimits, dt: f32) -> V2 {
        let a_dec = APPROACH_DECEL_SHARE * gl.a_h;
        let detour = self.plan.state == AvoidState::Detour
            || goal.is_some_and(|g| g.kind == GoalKind::Vantage);
        // A commanded speed is the skill's speed toward its goal: along a
        // detour longer than the straight way to its end it is flown that
        // much faster (at most [`DETOUR_STRETCH_MAX`]).
        let mut longer = 1.0;
        let to = match (self.plan.state, self.plan.target, self.plan.goal) {
            (AvoidState::Detour, Some(t), Some(end)) => {
                let route = stretch(to, route_beyond_target(&self.plan));
                let straight = geo::dist(geo::sub(t, to), end);
                longer = (geo::norm(route) / straight.max(1e-3)).clamp(1.0, DETOUR_STRETCH_MAX);
                route
            }
            (AvoidState::Detour, ..) => stretch(to, route_beyond_target(&self.plan)),
            _ => to,
        };
        let Some(g) = goal else {
            return approach(to, a_dec, f32::INFINITY, dt);
        };
        let gv = [g.vn, g.ve];
        let command = g.vel_mode == VelMode::Command;
        match (command, detour) {
            (true, false) => gv,
            (true, true) if geo::norm(gv) < 1e-3 => [0.0, 0.0],
            (true, true) => approach(to, a_dec, geo::norm(gv).max(DETOUR_MIN_MPS) * longer, dt),
            (false, false) => geo::add(gv, approach(to, a_dec, f32::INFINITY, dt)),
            (false, true) => approach(to, a_dec, f32::INFINITY, dt),
        }
    }

    /// What the goal asks while the pilot holds: as [`Pilot::goal_ask`],
    /// toward the plan's target, else toward the goal's own point.
    pub(super) fn held_ask(
        &self,
        goal: Option<&Goal>,
        max_speed: f32,
        pos_ne: V2,
        dt: f32,
        now_ms: u64,
    ) -> Ask {
        let Some(aim) = self.commanded_goal() else {
            return Ask::NONE;
        };
        let target = self.plan.target.unwrap_or([aim[0], aim[1]]);
        let gl = self.goal_limits_now(now_ms);
        Ask {
            target_ne: Some(target),
            vel: self.goal_ask(goal, geo::sub(target, pos_ne), &gl, dt),
            v_cap: self.goal_v_cap(max_speed, now_ms).0,
        }
    }
}

/// How much of a detour's route lies beyond its target, metres.
fn route_beyond_target(plan: &Plan) -> f32 {
    let len: f32 = plan.path.windows(2).map(|w| geo::dist(w[0], w[1])).sum();
    (len - planner::LOOKAHEAD_M).max(0.0)
}

/// `to` lengthened by `more` metres along itself.
fn stretch(to: V2, more: f32) -> V2 {
    let n = geo::norm(to);
    if n < 1e-4 {
        return to;
    }
    geo::scale(to, (n + more) / n)
}

#[cfg(test)]
mod tests {
    use ados_protocol::pilot::{AvoidState, Goal, GoalKind, VelMode};

    use crate::config::Settings;
    use crate::core::Pilot;
    use crate::geo;
    use crate::model::goal_limits;
    use crate::planner::Plan;

    #[test]
    fn a_commanded_speed_is_raised_by_how_much_longer_the_detour_is() {
        // A detour from the origin round (4, 3) to (8, 0): 10 m of route
        // for 8 m of straight way. The skill commands 1.4 m/s: the pilot
        // flies the route at 1.4 x 1.25 = 1.75 m/s, toward its target.
        let mut p = Pilot::new(Settings::default());
        p.plan = Plan {
            state: AvoidState::Detour,
            target: Some([1.6, 1.2]),
            goal: Some([8.0, 0.0]),
            path: vec![[0.0, 0.0], [4.0, 3.0], [8.0, 0.0]],
            min_clearance: None,
        };
        let mut g = Goal::new(GoalKind::Follow, [8.0, 0.0, -4.0], 3.0);
        g.vel_mode = VelMode::Command;
        g.vn = 1.4;
        let gl = goal_limits(p.settings.pilot.profile);
        let ask = p.goal_ask(Some(&g), [1.6, 1.2], &gl, 0.02);
        assert!((geo::norm(ask) - 1.75).abs() < 1e-3, "{ask:?}");
        assert!(geo::dot(ask, [0.8, 0.6]) > geo::norm(ask) - 1e-3, "{ask:?}");
        // A detour no longer than the straight way keeps the commanded speed.
        p.plan.goal = Some([16.0, 0.0]);
        let ask = p.goal_ask(Some(&g), [1.6, 1.2], &gl, 0.02);
        assert!((geo::norm(ask) - 1.4).abs() < 1e-3, "{ask:?}");
    }
}
