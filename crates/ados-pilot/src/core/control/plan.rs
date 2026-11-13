//! Planner-driven flight: the setpoint for a skill goal or a goto, braked
//! the same way stick flight is, and held inside the geofence and the
//! altitude limits.

use ados_protocol::pilot::{AvoidMode, AvoidState, Owner};

use super::{
    geofence, FC_POS_GAIN, FLOOR_AGL_M, GOTO_ARRIVE_M, HARD_STOP_FRAC, HOLD_OBSTACLE_CLOSE,
};
use crate::core::Pilot;
use crate::fc::MASK_POS_VEL_YAW;
use crate::geo::{self, V2};

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
                Some(t) => (None, self.limits_now(now_ms).v_h, t[2], None, None),
                None => (None, 0.0, pos[2], None, None),
            },
        };
        let home = self.fc.home_ne();
        let max_dist = self.settings.pilot.max_dist_m;
        let max_alt = self.settings.pilot.max_alt_agl_m;
        let agl = self.agl(now_ms);
        if self.owner() == Owner::Goto {
            if let Some(t) = self.goto {
                // Arrival at the point the limits let it fly to: a target past
                // the fence or the ceiling is reached at the limit.
                let t_ne = inside_fence([t[0], t[1]], home, max_dist);
                let (t_d, _) = clamp_d(t[2], 0.0, pos[2], agl, FLOOR_AGL_M, max_alt);
                if geo::dist(pos_ne, t_ne) < GOTO_ARRIVE_M && (pos[2] - t_d).abs() < GOTO_ARRIVE_M {
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
        let v_cap = max_speed.min(self.limits_now(now_ms).v_h).max(0.5);
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
        let fence = |v: V2| geofence(pos_ne, home, max_dist, v);
        let (sp_ne, vel) = if self.nav_capped(now_ms) {
            // Capped, a distant position setpoint would let the FC's position
            // loop pull past the cap, so the velocity is flown from the
            // vehicle's own position. The ramp runs first and the brake and
            // the fence after it, so their cut flies at once.
            let want = self.cap_ramp(geo::clamp_len(ask, v_cap), now_ms);
            let v = fence(self.goal_brake(pos_ne, want, v_cap, now_ms).unwrap_or(want));
            self.cap_flown(v, now_ms);
            (pos_ne, v)
        } else {
            self.cap_vel = None;
            match self.goal_brake(pos_ne, ask, v_cap, now_ms) {
                // At the vehicle's own position the FC's position loop adds
                // nothing: it flies the braked velocity.
                Some(v) => (pos_ne, fence(v)),
                None => (inside_fence(target, home, max_dist), fence(vel)),
            }
        };
        let (goal_d, vd) = clamp_d(
            goal_d,
            goal.as_ref().map_or(0.0, |g| g.vd),
            pos[2],
            agl,
            FLOOR_AGL_M,
            max_alt,
        );
        let sp = [sp_ne[0], sp_ne[1], goal_d];
        self.setpoint = Some(sp);
        let m = self
            .tx()
            .setpoint(MASK_POS_VEL_YAW, sp, [vel[0], vel[1], vd], yaw_cmd, 0.0);
        self.mav(m);
    }
}

/// `p`, or the point on the geofence circle toward it when it lies outside.
fn inside_fence(p: V2, home: V2, max_dist: f32) -> V2 {
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
fn clamp_d(
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
