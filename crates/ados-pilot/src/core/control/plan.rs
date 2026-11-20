//! Planner-driven flight: a skill goal or a goto flown on the one shaped
//! reference ([`super::MotionRef`]) at the profile's goal limits, braked as
//! stick flight is, and held inside the geofence and the altitude limits.

use ados_protocol::pilot::{AvoidMode, AvoidState, GoalKind, Owner};

mod ask;
mod bounds;

use super::motion_ref::{leash_within, MotionRef, ASK_SMOOTH_S, REF_LEASH_M, SHAPER_REPORT_MPS};
use super::subject_guard::Subject;
use super::{
    geofence, FC_POS_GAIN, FLOOR_AGL_M, GOTO_ARRIVE_M, HARD_STOP_FRAC, HOLD_NO_VANTAGE,
    HOLD_OBSTACLE_CLOSE, HOLD_OBSTACLE_STALE,
};
use crate::core::trace::{Ask, Tick};
use crate::core::{limit, Pilot};
use crate::fc::{MASK_POS_VEL_ACC_YAW, MASK_POS_VEL_YAW};
use crate::geo::{self, V2};
use crate::model::{limits, GoalLimits, HOLD_CAPTURE_MPS};
use bounds::{clamp_d, inside_fence, live_goal};

/// Least speed a commanded-velocity goal flies its detour at, m/s: its
/// velocity may point anywhere while the pilot flies around an obstacle.
const DETOUR_MIN_MPS: f32 = 1.0;

/// What one shaped tick is flown within.
struct Shape {
    pos: V2,
    dt: f32,
    gl: GoalLimits,
    v_cap: f32,
    speed_by: u16,
    capped: bool,
    home: V2,
    max_dist: f32,
    /// The subject a skill's goal names, for the subject brake.
    subject: Option<Subject>,
}

impl Pilot {
    /// `stale_hold` is last tick's hold for stale obstacle data, if any.
    pub(super) fn fly_plan(
        &mut self,
        pos: [f32; 3],
        yaw: f32,
        dt: f32,
        stale_hold: Option<[f32; 3]>,
        now_ms: u64,
    ) {
        let pos_ne = [pos[0], pos[1]];
        let (goal, max_speed, goal_d, goal_yaw, subject) = match self.owner() {
            Owner::Skill => match self.auth.lease().and_then(|l| l.goal.clone()) {
                // A vantage goal faces the predicted subject unless it sets a yaw.
                Some(g) => {
                    let subject = g
                        .subject_ne
                        .or_else(|| g.vantage.as_ref().map(|v| v.predicted_ne));
                    (Some(g.clone()), g.max_speed_mps, g.d, g.yaw, subject)
                }
                None => (None, 0.0, pos[2], None, None),
            },
            // A goto's own limit is the profile's; the nav cap holds it below.
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
        // The subject brake's track is kept on every tick, held or not; the
        // climb clear gives the highest setpoint down position allowed.
        let guard = self.track_subject(pos, now_ms);
        let floor_d = guard.and_then(|g| g.1);
        if self.obstacle_data_stale(now_ms) {
            // Unknown obstacles: hold where the vehicle was when the data
            // went stale, with no push (nothing is seen to push from). The
            // goal is flown again once the scans are fresh. A safety hold:
            // it is not shaped.
            let hold = stale_hold.unwrap_or(pos);
            self.stale_hold = Some(hold);
            self.hold_reason = Some(HOLD_OBSTACLE_STALE.into());
            self.drop_goal_ref();
            if self.speed_capped(now_ms) {
                self.cap_flown([0.0, 0.0], now_ms);
            }
            self.setpoint = Some(hold);
            let pull = geo::scale(geo::sub([hold[0], hold[1]], pos_ne), FC_POS_GAIN);
            let ask = self.held_ask(goal.as_ref(), max_speed, pos_ne, dt, now_ms);
            self.command = Some(Tick::hold(now_ms, ask, pull, [0.0; 3], [0.0; 2], 0));
            let m = self
                .tx()
                .setpoint(MASK_POS_VEL_YAW, hold, [0.0; 3], yaw, 0.0);
            self.mav(m);
            return;
        }
        let margin = self.margin();
        let close = self
            .obstacles
            .min_raw_m(now_ms)
            .is_some_and(|d| d < HARD_STOP_FRAC * margin)
            && self.settings.pilot.avoid != AvoidMode::Off;
        let home = self.fc.home_ne();
        let max_dist = self.settings.pilot.max_dist();
        let max_alt = self.settings.pilot.max_alt();
        let agl = self.agl(now_ms);
        if self.owner() == Owner::Goto {
            if let Some(t) = self.goto {
                // Arrival at the point the limits let it fly to: a target past
                // the fence or the ceiling is reached at the limit, once the
                // shaped approach has slowed (no stop from speed).
                let t_ne = inside_fence([t[0], t[1]], home, max_dist);
                let (t_d, _) = clamp_d(t[2], 0.0, pos[2], agl, FLOOR_AGL_M, max_alt);
                let slow = self
                    .flown_goal_ref()
                    .is_none_or(|r| geo::norm(r.sh.v) < HOLD_CAPTURE_MPS);
                if geo::dist(pos_ne, t_ne) < GOTO_ARRIVE_M
                    && (pos[2] - t_d).abs() < GOTO_ARRIVE_M
                    && slow
                {
                    self.drop_goal_ref();
                    self.brake_hold(None);
                    return;
                }
            }
        }
        let (v_cap, own_cap) = self.goal_v_cap(max_speed, now_ms);
        // The speed limit that binds: the nav cap when it is below the
        // goal's own limit.
        let speed_by = if v_cap < own_cap - 1e-3 {
            limit::NAV_CAP
        } else {
            limit::SPEED
        };
        let shape = Shape {
            pos: pos_ne,
            dt,
            gl: self.goal_limits_now(now_ms),
            v_cap,
            speed_by,
            capped: self.speed_capped(now_ms),
            home,
            max_dist,
            subject: guard.map(|g| g.0),
        };
        let target = if close { None } else { self.plan.target };
        let Some(target) = target else {
            let vantage = goal.as_ref().is_some_and(|g| g.kind == GoalKind::Vantage);
            self.hold_reason = Some(
                if close {
                    HOLD_OBSTACLE_CLOSE
                } else if vantage {
                    HOLD_NO_VANTAGE
                } else {
                    "blocked"
                }
                .into(),
            );
            let ask = self.held_ask(goal.as_ref(), max_speed, pos_ne, dt, now_ms);
            if close {
                self.hard_stop_hold(pos, yaw, ask, dt, now_ms);
                return;
            }
            // Blocked or no vantage: slow to a stop on the goal profile and
            // hold where it ends. The brake's push walks the hold back out
            // of the margin.
            let hold_d = self.plan_hold.map_or(pos[2], |h| h[2]);
            let hold_d = match floor_d {
                Some(f) if f < hold_d => clamp_d(f, 0.0, pos[2], agl, FLOOR_AGL_M, max_alt).0,
                _ => hold_d,
            };
            let mut r = self.goal_ref(pos, yaw, now_ms);
            let mut limits = 0;
            let v = self.fly_shaped(&mut r, [0.0, 0.0], &shape, &mut limits, now_ms);
            let yaw_cmd = r.slew_yaw(None, &shape.gl, dt);
            r.at_ms = now_ms;
            self.motion_ref = Some(r);
            let hold = [r.p[0], r.p[1], hold_d];
            self.plan_hold = Some(hold);
            self.setpoint = Some(hold);
            let cmd = geo::add(v, geo::scale(geo::sub(r.p, pos_ne), FC_POS_GAIN));
            self.command = Some(Tick::hold(
                now_ms,
                ask,
                cmd,
                [v[0], v[1], 0.0],
                r.sh.a,
                limits,
            ));
            let m = self.tx().setpoint_acc(
                MASK_POS_VEL_ACC_YAW,
                hold,
                [v[0], v[1], 0.0],
                [r.sh.a[0], r.sh.a[1], 0.0],
                yaw_cmd,
                0.0,
            );
            self.mav(m);
            return;
        };
        self.hold_reason = None;
        self.plan_hold = None;
        let detour = self.plan.state == AvoidState::Detour;
        // A straight plan to a skill's goal: fly the goal as it is now, not
        // as it was at the last replan. Replanned every PLAN_PERIOD_MS, a
        // moving goal would step every 200 ms. A vantage goal's target is
        // the vantage point the planner chose, not the goal's own point.
        let target = match (&goal, self.plan.goal) {
            (Some(g), Some(planned))
                if !detour && target == planned && g.kind != GoalKind::Vantage =>
            {
                let since_ms = self
                    .auth
                    .lease()
                    .map_or(0, |l| now_ms.saturating_sub(l.last_goal_ms));
                live_goal(g, since_ms)
            }
            _ => target,
        };
        let mut r = self.goal_ref(pos, yaw, now_ms);
        if shape.capped {
            // Capped, the velocity is flown from the vehicle's own position:
            // a reference ahead of it would let the FC's position loop pull
            // past the cap.
            r.p = pos_ne;
        }
        let asked = self.goal_ask(goal.as_ref(), geo::sub(target, r.p), &shape.gl, dt);
        // Every limit that changes what was asked, for the published command:
        // the speed limit that binds when the ask is over it, and the goal's
        // own limit when its own velocity is.
        let mut limits = if geo::norm(asked) > v_cap + 1e-3 {
            speed_by
        } else {
            0
        };
        let own = goal.as_ref().map_or([0.0; 2], |g| [g.vn, g.ve]);
        if geo::norm(own) > own_cap + 1e-3 {
            limits |= limit::SPEED;
        }
        let want = geo::clamp_len(asked, v_cap);
        let v = self.fly_shaped(&mut r, want, &shape, &mut limits, now_ms);
        let want_yaw = goal_yaw.or_else(|| match subject {
            Some(s) => Some(geo::bearing_of(geo::sub(s, pos_ne))),
            None if geo::dist(target, pos_ne) > 1.0 => {
                Some(geo::bearing_of(geo::sub(target, pos_ne)))
            }
            None => None,
        });
        let yaw_cmd = r.slew_yaw(want_yaw, &shape.gl, dt);
        r.at_ms = now_ms;
        self.motion_ref = Some(r);
        // Climbing clear of the subject: the setpoint is raised to the climb
        // floor, never lowered to it, and a descending feed-forward dropped.
        let (goal_d, vd) = match (floor_d, goal.as_ref().map_or(0.0, |g| g.vd)) {
            (Some(f), vd) if f < goal_d => (f, vd.min(0.0)),
            (_, vd) => (goal_d, vd),
        };
        let (goal_d, vd) = clamp_d(goal_d, vd, pos[2], agl, FLOOR_AGL_M, max_alt);
        let sp = [r.p[0], r.p[1], goal_d];
        self.setpoint = Some(sp);
        // The FC's pull toward the reference is held so the commanded speed
        // stays within the goal's limit.
        let cmd = geo::clamp_len(
            geo::add(v, geo::scale(geo::sub(r.p, pos_ne), FC_POS_GAIN)),
            v_cap.max(geo::norm(v)),
        );
        self.command = Some(Tick {
            at_ms: now_ms,
            ask: Ask {
                target_ne: Some(target),
                vel: asked,
                v_cap,
            },
            cmd,
            sp_vel: [v[0], v[1], vd],
            sp_acc: r.sh.a,
            limits,
            bound_by: limits & speed_by,
        });
        let m = self.tx().setpoint_acc(
            MASK_POS_VEL_ACC_YAW,
            sp,
            [v[0], v[1], vd],
            [r.sh.a[0], r.sh.a[1], 0.0],
            yaw_cmd,
            0.0,
        );
        self.mav(m);
    }

    /// One shaped tick toward `want`: the brakes' caps at the goal
    /// acceleration slow it first (the subject brake's
    /// [`Pilot::subject_soft`], then the obstacle brake's [`Pilot::soft_brake`]),
    /// the shaper holds its acceleration and jerk, then the safety limits
    /// cut what it flies at once: the speed cap, the subject brake on the
    /// profile's stick acceleration ([`Pilot::subject_cut`]), the obstacle
    /// brake on the same acceleration ([`Pilot::goal_brake`]), last so a
    /// back-away from the subject never flies into an obstacle, and the
    /// geofence. Moves the reference point on the velocity flown and returns
    /// that velocity.
    fn fly_shaped(
        &mut self,
        r: &mut MotionRef,
        want: V2,
        s: &Shape,
        limits: &mut u16,
        now_ms: u64,
    ) -> V2 {
        // Smoothed first, so a goal reversing at its update rate does not
        // make the shaper wander; the brake then cuts the smoothed ask.
        r.ask = geo::add(
            r.ask,
            geo::scale(geo::sub(want, r.ask), (s.dt / ASK_SMOOTH_S).min(1.0)),
        );
        let mut want = r.ask;
        if let Some(subject) = &s.subject {
            if let Some(held) = self.subject_soft(subject, s.pos, want, s.gl.a_h, s.v_cap, now_ms) {
                *limits |= limit::SUBJECT;
                want = held;
            }
        }
        let (want, by) = self.soft_brake(s.pos, want, s.gl.a_h, now_ms);
        *limits |= by;
        let mut v = r.sh.track(want, &s.gl.shape(), s.dt);
        if geo::dist(v, want) > SHAPER_REPORT_MPS {
            *limits |= limit::SHAPER;
        }
        if geo::norm(v) > s.v_cap + 1e-3 {
            // A speed cap that came on mid-flight holds at once.
            v = geo::clamp_len(v, s.v_cap);
            r.sh.reset(v);
            *limits |= s.speed_by;
        }
        let mut p = if s.capped {
            s.pos
        } else {
            leash_within(geo::add(r.p, geo::scale(v, s.dt)), s.pos, REF_LEASH_M)
        };
        if let Some(subject) = &s.subject {
            let fc_ask = geo::add(v, geo::scale(geo::sub(p, s.pos), FC_POS_GAIN));
            if let Some((cut, slide_capped)) =
                self.subject_cut(subject, s.pos, fc_ask, v, s.v_cap, now_ms)
            {
                // A safety cut, flown from the vehicle's own position as
                // the obstacle brake's is.
                *limits |= limit::SUBJECT;
                if slide_capped {
                    *limits |= s.speed_by;
                }
                v = cut;
                p = s.pos;
                r.cut(p, v);
            }
        }
        let fc_ask = geo::add(v, geo::scale(geo::sub(p, s.pos), FC_POS_GAIN));
        if let Some((braked, by)) = self.goal_brake(s.pos, fc_ask, s.v_cap, s.speed_by, now_ms) {
            // At the vehicle's own position the FC's position loop adds
            // nothing: it flies the braked velocity.
            *limits |= by;
            v = braked;
            p = s.pos;
            r.cut(p, v);
        }
        let fenced = geofence(s.pos, s.home, s.max_dist, v);
        if geo::dist(fenced, v) > 1e-3 {
            *limits |= limit::GEOFENCE;
            v = fenced;
            r.sh.reset(v);
        }
        let inside = inside_fence(p, s.home, s.max_dist);
        if geo::dist(inside, p) > 1e-3 {
            *limits |= limit::GEOFENCE;
            p = inside;
        }
        r.p = p;
        if s.capped {
            self.cap_flown(v, now_ms);
        } else {
            self.cap_vel = None;
        }
        v
    }

    /// Inside the hard-stop distance: hold where the vehicle stopped, a
    /// safety hold that is not shaped. The brake's push moves the hold back
    /// out: a fast push flies from the vehicle's own position, a slow one
    /// walks the hold point.
    fn hard_stop_hold(&mut self, pos: [f32; 3], yaw: f32, ask: Ask, dt: f32, now_ms: u64) {
        let pos_ne = [pos[0], pos[1]];
        // A shaped stop's hold point is its reference, which may lead the
        // vehicle by up to the leash: the hard stop holds where the vehicle
        // is, at the hold's altitude.
        if self.drop_goal_ref() {
            if let Some(h) = self.plan_hold.as_mut() {
                h[0] = pos[0];
                h[1] = pos[1];
            }
        }
        let push = geofence(
            pos_ne,
            self.fc.home_ne(),
            self.settings.pilot.max_dist_m,
            self.brake_limit(pos, [0.0, 0.0], now_ms),
        );
        let mut hold = *self.plan_hold.get_or_insert(pos);
        let mut vel = [0.0; 3];
        if geo::norm(push) >= HOLD_CAPTURE_MPS {
            // Horizontal only: the hold keeps its altitude.
            hold = [pos[0], pos[1], hold[2]];
            vel = [push[0], push[1], 0.0];
            self.plan_hold = Some(hold);
        } else if geo::norm(push) > 1e-3 {
            hold[0] += push[0] * dt;
            hold[1] += push[1] * dt;
            self.plan_hold = Some(hold);
        }
        if self.speed_capped(now_ms) {
            self.cap_flown(push, now_ms);
        }
        self.setpoint = Some(hold);
        let pull = geo::scale(geo::sub([hold[0], hold[1]], pos_ne), FC_POS_GAIN);
        let mut extra = limit::HARD_STOP;
        if geo::norm(push) > 1e-3 {
            extra |= limit::PUSH;
        }
        self.command = Some(Tick::hold(
            now_ms,
            ask,
            geo::add([vel[0], vel[1]], pull),
            vel,
            [0.0; 2],
            extra,
        ));
        let m = self.tx().setpoint(MASK_POS_VEL_YAW, hold, vel, yaw, 0.0);
        self.mav(m);
    }
}
