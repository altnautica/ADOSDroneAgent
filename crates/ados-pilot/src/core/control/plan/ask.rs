//! What a skill goal or a goto asks before any limit: its speed limit, the
//! velocity toward its target, and the ask published while the pilot holds.

use ados_protocol::pilot::{AvoidState, Goal, GoalKind, VelMode};

use super::super::motion_ref::{approach, APPROACH_DECEL_SHARE};
use super::DETOUR_MIN_MPS;
use crate::core::trace::Ask;
use crate::core::Pilot;
use crate::geo::{self, V2};
use crate::model::{limits, GoalLimits};
use crate::planner::{self, Plan};

impl Pilot {
    /// The goal's speed limit, `max_speed` held to the profile's limit and to
    /// any nav cap, and its own limit without the nav cap.
    pub(super) fn goal_v_cap(&self, max_speed: f32, now_ms: u64) -> (f32, f32) {
        let v_cap = max_speed.min(self.limits_now(now_ms).v_h).max(0.5);
        let own = max_speed
            .min(limits(self.settings.pilot.profile).v_h)
            .max(0.5);
        (v_cap, own)
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
        let to = if self.plan.state == AvoidState::Detour {
            stretch(to, route_beyond_target(&self.plan))
        } else {
            to
        };
        let Some(g) = goal else {
            return approach(to, a_dec, f32::INFINITY, dt);
        };
        let gv = [g.vn, g.ve];
        let command = g.vel_mode == VelMode::Command;
        match (command, detour) {
            (true, false) => gv,
            (true, true) if geo::norm(gv) < 1e-3 => [0.0, 0.0],
            (true, true) => approach(to, a_dec, geo::norm(gv).max(DETOUR_MIN_MPS), dt),
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
