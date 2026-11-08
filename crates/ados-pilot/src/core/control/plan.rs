//! Planner-driven flight: the setpoint for a skill goal or a goto, braked
//! the same way stick flight is.

use ados_protocol::pilot::{AvoidMode, AvoidState, Owner};

use super::{FC_POS_GAIN, GOTO_ARRIVE_M, HARD_STOP_FRAC, HOLD_OBSTACLE_CLOSE};
use crate::core::Pilot;
use crate::fc::MASK_POS_VEL_YAW;
use crate::geo;
use crate::model::limits;

impl Pilot {
    pub(super) fn fly_plan(&mut self, pos: [f32; 3], yaw: f32, now_ms: u64) {
        let pos_ne = [pos[0], pos[1]];
        let margin = self.margin();
        let close = self
            .obstacles
            .min_raw_m(now_ms)
            .is_some_and(|d| d < HARD_STOP_FRAC * margin)
            && self.settings.pilot.avoid != AvoidMode::Off;
        let (goal, max_speed, goal_d, goal_yaw, subject) = match self.owner() {
            Owner::Skill => match self.auth.lease().and_then(|l| l.goal.clone()) {
                Some(g) => (Some(g.clone()), g.max_speed_mps, g.d, g.yaw, g.subject_ne),
                None => (None, 0.0, pos[2], None, None),
            },
            _ => match self.goto {
                Some(t) => (
                    None,
                    limits(self.settings.pilot.profile).v_h,
                    t[2],
                    None,
                    None,
                ),
                None => (None, 0.0, pos[2], None, None),
            },
        };
        if self.owner() == Owner::Goto {
            if let Some(t) = self.goto {
                if geo::dist(pos_ne, [t[0], t[1]]) < GOTO_ARRIVE_M
                    && (pos[2] - t[2]).abs() < GOTO_ARRIVE_M
                {
                    self.brake_hold(now_ms, None);
                    return;
                }
            }
        }
        let target = if close { None } else { self.plan.target };
        let Some(target) = target else {
            self.hold_reason = Some(
                if close {
                    HOLD_OBSTACLE_CLOSE
                } else {
                    "blocked"
                }
                .into(),
            );
            let hold = *self.plan_hold.get_or_insert(pos);
            self.setpoint = Some(hold);
            let m = self
                .tx()
                .setpoint(MASK_POS_VEL_YAW, hold, [0.0; 3], yaw, 0.0);
            self.mav(m);
            return;
        };
        self.hold_reason = None;
        self.plan_hold = None;
        let v_cap = max_speed
            .min(limits(self.settings.pilot.profile).v_h)
            .max(0.5);
        let to = geo::sub(target, pos_ne);
        let detour = self.plan.state == AvoidState::Detour;
        let vel = match (&goal, detour) {
            (Some(g), false) => geo::clamp_len([g.vn, g.ve], v_cap),
            _ => geo::clamp_len(geo::scale(to, 1.0), v_cap),
        };
        let yaw_cmd = goal_yaw.unwrap_or_else(|| match subject {
            Some(s) => geo::bearing_of(geo::sub(s, pos_ne)),
            None if geo::norm(to) > 1.0 => geo::bearing_of(to),
            None => yaw,
        });
        let ask = geo::add(vel, geo::scale(to, FC_POS_GAIN));
        let (sp_ne, vel) = match self.goal_brake(pos_ne, ask, v_cap, now_ms) {
            // At the vehicle's own position the FC's position loop adds
            // nothing: it flies the braked velocity.
            Some(v) => (pos_ne, v),
            None => (target, vel),
        };
        let sp = [sp_ne[0], sp_ne[1], goal_d];
        self.setpoint = Some(sp);
        let vd = goal.as_ref().map_or(0.0, |g| g.vd);
        let m = self
            .tx()
            .setpoint(MASK_POS_VEL_YAW, sp, [vel[0], vel[1], vd], yaw_cmd, 0.0);
        self.mav(m);
    }
}
