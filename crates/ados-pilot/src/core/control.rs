//! The 50 Hz control loop: failsafes, owner transitions driven by sticks, and
//! the setpoint for whoever owns motion.

use ados_protocol::pilot::{
    AvoidMode, AvoidState, ControlMode, GoalKind, Owner, Sticks, VehicleClass,
};
use serde_json::json;

use super::{Out, Pilot, PLAN_PERIOD_MS, STICKS_PERIOD_MS};
use crate::fc::{heartbeat, MASK_POS_VEL_YAW, MASK_VELOCITY};
use crate::flows::banner;
use crate::geo::{self, V2};
use crate::model::{limits, Command, StickIn};
use crate::planner;

mod brake;
mod plan;
pub use brake::*;

/// Takeover: any axis above this for `TAKEOVER_MS`.
const TAKEOVER_AXIS: f32 = 0.9;
const TAKEOVER_MS: u64 = 1000;
/// Mission pre-emption: deflection above this for `PREEMPT_MS`.
const PREEMPT_AXIS: f32 = 0.3;
const PREEMPT_MS: u64 = 300;
/// Landing: switch to LAND below this height.
pub(super) const LAND_SWITCH_AGL_M: f32 = 0.6;
/// Floor for stick descent.
const FLOOR_AGL_M: f32 = 0.5;
/// Swarm goals older than this are dropped.
const SWARM_STALE_MS: u64 = 500;
/// Goto arrival radius.
const GOTO_ARRIVE_M: f32 = 1.0;
/// Low-battery RTH countdown.
const LOW_BATT_COUNTDOWN_MS: u64 = 10_000;
/// The FC's response time the stick brake plans for. In the SITL x500 traces
/// the measured velocity trails a cut setpoint by about 0.5 s at cruise and
/// keeps rising for 0.6 s when the cut comes mid-acceleration (the FC first
/// unwinds its acceleration at its jerk limit), so 0.8 s.
pub const BRAKE_LAG_S: f32 = 0.8;
/// Obstacles further than this never limit stick flight.
const BRAKE_RANGE_M: f32 = 20.0;
/// Hard stop: an obstacle closer than this fraction of the margin forces a
/// hold (only motion away from it is left).
pub const HARD_STOP_FRAC: f32 = 0.6;
/// `hold_reason` while the hard stop holds.
pub const HOLD_OBSTACLE_CLOSE: &str = "obstacle-close";
/// A braked velocity under this counts as stopped (`avoid_state` blocked).
const STOPPED_MPS: f32 = 0.1;
/// Bearing sectors (5°) the stick brake keeps one constraint per.
const BRAKE_SECTORS: usize = 72;
/// The FC's horizontal position-loop gain (ArduPilot `PSC_POSXY_P` default):
/// with a position setpoint it asks for the feed-forward plus this times
/// the distance to the target.
const FC_POS_GAIN: f32 = 1.0;

fn max_abs(a: [f32; 4]) -> f32 {
    a.iter().fold(0.0f32, |m, v| m.max(v.abs()))
}

impl Pilot {
    /// One control tick at `now_ms`.
    pub fn tick(&mut self, now_ms: u64) {
        let dt = self.last_tick_ms.map_or(0.02, |t| {
            (now_ms.saturating_sub(t) as f32 / 1000.0).clamp(0.001, 0.1)
        });
        self.last_tick_ms = Some(now_ms);
        if self.failsafe.heartbeat_due(now_ms) {
            self.mav(heartbeat());
        }
        if self.banner.as_ref().is_some_and(|b| b.until_ms <= now_ms) {
            self.banner = None;
        }
        if !self.fc.linked(now_ms) {
            return;
        }
        if let Some(r) = self.auth.expire(now_ms) {
            self.out.push(Out::Revoked(r));
            self.brake_hold(now_ms, Some("lease-revoked"));
        }
        self.drive_launch(now_ms);
        self.track_touchdown(now_ms);
        let axes = self.stick_axes();
        if self.owner() == Owner::Tuning {
            self.tuning_axes = axes;
            return;
        }
        let raw = self.raw_axes();
        self.stick_transitions(axes, raw, now_ms);
        self.failsafes(now_ms);
        self.low_battery(now_ms);
        if now_ms.saturating_sub(self.plan_ms) >= PLAN_PERIOD_MS {
            self.plan_ms = now_ms;
            if let Some(p) = self.pos_ne(now_ms) {
                self.obstacles.rebuild(p, now_ms);
            }
            self.replan(now_ms);
        }
        self.fly(axes, dt, now_ms);
    }

    /// Operator axes as the pilot may use them: gated, and only in `ados` mode.
    fn stick_axes(&self) -> [f32; 4] {
        if self.control_mode() == ControlMode::Ados {
            self.sessions.effective_axes()
        } else {
            [0.0; 4]
        }
    }

    /// Operator axes before the handover gate (for detecting a takeover).
    fn raw_axes(&self) -> [f32; 4] {
        if self.control_mode() == ControlMode::Ados && self.sessions.owner.is_some() {
            self.sessions.axes
        } else {
            [0.0; 4]
        }
    }

    /// `axes` are the gated sticks (what may fly, and what a skill's nudges
    /// see); `raw` the operator's actual deflection, which decides takeovers.
    /// Every stick takeover re-arms the gate, so the vehicle holds until the
    /// sticks are centred.
    fn stick_transitions(&mut self, axes: [f32; 4], raw: [f32; 4], now_ms: u64) {
        let deflect = max_abs(raw);
        match self.owner() {
            Owner::Skill => {
                if now_ms.saturating_sub(self.sticks_ms) >= STICKS_PERIOD_MS {
                    self.sticks_ms = now_ms;
                    if let Some(l) = self.auth.lease() {
                        self.out.push(Out::Sticks(Sticks {
                            lease_id: l.id,
                            axes,
                        }));
                    }
                }
                if self
                    .takeover
                    .update(deflect > TAKEOVER_AXIS, now_ms, TAKEOVER_MS)
                {
                    self.revoke("takeover");
                    self.brake_hold(now_ms, None);
                    self.sessions.arm_gate();
                    self.banner = Some(banner("pilot.banner.pilot_took_control", &[], now_ms));
                }
            }
            Owner::MissionFc => {
                if self
                    .mission_preempt
                    .update(deflect > PREEMPT_AXIS, now_ms, PREEMPT_MS)
                {
                    let g = self.family().guided();
                    self.set_mode(g, now_ms);
                    self.set_owner(Owner::Pilot, "stick");
                    self.engage_pilot(now_ms);
                    self.sessions.arm_gate();
                    self.mission_paused = true;
                    self.banner = Some(banner("pilot.banner.pilot_took_control", &[], now_ms));
                }
            }
            Owner::Idle | Owner::FcMode
                if self.fc.armed && self.in_guided() && deflect > PREEMPT_AXIS =>
            {
                self.set_owner(Owner::Pilot, "stick");
                self.engage_pilot(now_ms);
                self.sessions.arm_gate();
            }
            Owner::Swarm | Owner::Goto if deflect > PREEMPT_AXIS => {
                self.set_owner(Owner::Pilot, "stick");
                self.goto = None;
                self.swarm = None;
                self.engage_pilot(now_ms);
                self.sessions.arm_gate();
            }
            _ => {
                self.takeover.update(false, now_ms, TAKEOVER_MS);
                self.mission_preempt.update(false, now_ms, PREEMPT_MS);
            }
        }
    }

    fn failsafes(&mut self, now_ms: u64) {
        // The FC's own landed state: a touchdown never holds a failsafe back.
        let airborne = self.fc.armed && !self.fc.landed();
        // Controller lost: the owning session stopped streaming.
        if self.owner() == Owner::Pilot
            && self.sessions.owner.is_some()
            && self.failsafe.input_stale(now_ms)
            && !self.controller_lost_shown
        {
            self.controller_lost_shown = true;
            self.sessions.axes = [0.0; 4];
            self.brake_hold(now_ms, Some("controller-lost"));
            self.banner = Some(banner("pilot.banner.controller_lost", &[], now_ms));
        }
        let flying = matches!(
            self.owner(),
            Owner::Pilot | Owner::Skill | Owner::Goto | Owner::Swarm
        );
        let window = u64::from(self.settings.pilot.link_loss_s) * 1000;
        if airborne && flying && self.failsafe.link_lost(now_ms, window) {
            let action = self.settings.pilot.link_loss_action;
            self.banner = Some(banner(
                "pilot.banner.link_loss",
                &[("action", json!(action.as_str())), ("in_s", json!(0))],
                now_ms,
            ));
            let _ = match action {
                crate::config::LinkLossAction::Rth => self.do_rth(now_ms),
                crate::config::LinkLossAction::Land => self.do_land(now_ms),
                crate::config::LinkLossAction::Hold => {
                    self.revoke("link-loss");
                    self.brake_hold(now_ms, Some("link-loss"));
                    Ok(())
                }
            };
        }
    }

    fn low_battery(&mut self, now_ms: u64) {
        let Some(pct) = self.fc.battery_pct else {
            return;
        };
        let p = &self.fc.params;
        let low_pct = match (p.get("BATT_LOW_MAH"), p.get("BATT_CAPACITY")) {
            (Some(&l), Some(&c)) if l > 0.0 && c > 0.0 => l / c * 100.0,
            _ => 20.0,
        };
        let airborne = self.fc.armed && !self.fc.landed();
        if !airborne || pct > low_pct + 5.0 {
            self.rth_deadline_ms = None;
            if pct > low_pct + 5.0 {
                self.low_batt_cancelled = false;
            }
            return;
        }
        if pct > low_pct || self.low_batt_cancelled || matches!(self.owner(), Owner::FcMode) {
            return;
        }
        let deadline = *self
            .rth_deadline_ms
            .get_or_insert(now_ms + LOW_BATT_COUNTDOWN_MS);
        if now_ms >= deadline {
            self.rth_deadline_ms = None;
            self.low_batt_cancelled = true;
            let _ = self.do_rth(now_ms);
        }
    }

    fn replan(&mut self, now_ms: u64) {
        let Some(pos) = self.pos_ne(now_ms) else {
            return;
        };
        let margin = self.margin();
        let goal = match self.owner() {
            Owner::Skill => self.auth.lease().and_then(|l| l.goal.clone()),
            Owner::Goto => self.goto.map(|g| ados_protocol::pilot::Goal {
                kind: GoalKind::Position,
                n: g[0],
                e: g[1],
                d: g[2],
                vn: 0.0,
                ve: 0.0,
                vd: 0.0,
                yaw: None,
                yaw_rate: None,
                subject_ne: None,
                max_speed_mps: limits(self.settings.pilot.profile).v_h,
                candidates: None,
                margin_m: None,
            }),
            _ => None,
        };
        let Some(goal) = goal else {
            self.plan = planner::Plan::hold(AvoidState::Clear);
            return;
        };
        if self.settings.pilot.avoid == AvoidMode::Off {
            let t = [goal.n, goal.e];
            self.plan = planner::Plan {
                state: AvoidState::Off,
                target: Some(t),
                goal: Some(t),
                path: vec![pos, t],
                min_clearance: None,
            };
            return;
        }
        self.plan = planner::plan(&self.obstacles, &goal, pos, margin);
    }

    /// Produce this tick's command for the owner.
    fn fly(&mut self, axes: [f32; 4], dt: f32, now_ms: u64) {
        self.setpoint = None;
        self.brake_avoid = AvoidState::Clear;
        if !self.fc.armed || !self.in_guided() {
            return;
        }
        let Some(pos) = self.pos(now_ms) else { return };
        let yaw = self.yaw(now_ms);
        let forward_flight =
            self.fc.vehicle_class() == VehicleClass::Fixedwing || self.fc.vtol_forward();
        match self.owner() {
            Owner::Pilot if forward_flight => {
                let lim =
                    crate::fixedwing::PlaneLimits::from_params(|n| self.fc.params.get(n).copied());
                let tx = self.tx();
                let msgs = self.fw.step(
                    axes,
                    self.settings.pilot.profile,
                    &lim,
                    self.settings.pilot.min_agl_fw_m,
                    dt,
                    now_ms,
                    &tx,
                );
                for m in msgs {
                    self.mav(m);
                }
            }
            Owner::Pilot => self.fly_sticks(axes, pos, yaw, dt, now_ms),
            Owner::Skill | Owner::Goto => self.fly_plan(pos, yaw, now_ms),
            Owner::Swarm => match self.swarm {
                Some(g) if now_ms.saturating_sub(g.at_ms) <= SWARM_STALE_MS => {
                    let v = self.brake_limit(pos, [g.vel[0], g.vel[1]], now_ms);
                    let m = self.tx().setpoint(
                        MASK_VELOCITY,
                        [0.0; 3],
                        [v[0], v[1], g.vel[2]],
                        0.0,
                        0.0,
                    );
                    self.mav(m);
                }
                _ => self.brake_hold(now_ms, None),
            },
            _ => {}
        }
    }

    fn fly_sticks(&mut self, axes: [f32; 4], pos: [f32; 3], yaw: f32, dt: f32, now_ms: u64) {
        let lim = limits(self.settings.pilot.profile);
        let agl = self.agl(now_ms);
        let mut axes = axes;
        if self.landing {
            axes = [axes[0], axes[1], -1.0, axes[3]];
            if agl.is_some_and(|a| a < LAND_SWITCH_AGL_M) {
                let m = self
                    .family()
                    .land(self.fc.vehicle_class() == VehicleClass::Vtol);
                self.set_mode(m, now_ms);
                self.set_owner(Owner::FcMode, "land");
                self.landing = false;
                self.land_handed_over();
                return;
            }
        }
        // Landing protection: full down held at the floor lands.
        let at_floor = agl.is_some_and(|a| a < LAND_SWITCH_AGL_M);
        if self
            .land_protect
            .update(axes[2] < -0.9 && at_floor, now_ms, 1500)
        {
            self.landing = true;
        }
        let mut vz = (-lim.v_up, lim.v_down);
        if let Some(a) = agl {
            if a < FLOOR_AGL_M && !self.landing {
                vz.1 = 0.0;
            }
            if self.landing {
                let vd = self.fc.fresh_local(now_ms).map_or(0.0, |l| l.vd);
                vz.1 = vz.1.min(self.land_descent_cap(a, vd));
            }
            if a >= self.settings.pilot.max_alt_agl_m {
                vz.0 = 0.0;
            }
        }
        let vel = self
            .fc
            .fresh_local(now_ms)
            .map_or([0.0; 3], |l| [l.vn, l.ve, l.vd]);
        let inp = StickIn {
            axes,
            pos,
            vel,
            yaw,
            dt,
        };
        let pos_ne = [pos[0], pos[1]];
        let brake = self.settings.pilot.avoid != AvoidMode::Off;
        let home = self.fc.home_ne();
        let max_dist = self.settings.pilot.max_dist_m;
        let obstacles = &self.obstacles;
        let a_h = lim.a_h;
        let margin = self.settings.pilot.obstacle_margin_m;
        let mut braked: Option<Braked> = None;
        let limit = |v: V2| {
            let v = if brake {
                let b = brake_limit(obstacles, pos_ne, [vel[0], vel[1]], v, a_h, margin, now_ms);
                braked = Some(b);
                b.v
            } else {
                v
            };
            geofence(pos_ne, home, max_dist, v)
        };
        let cmd = self.mr.step(&inp, &lim, limit, vz);
        self.brake_avoid = match braked {
            Some(b) if b.close || (b.cut && geo::norm(b.v) < STOPPED_MPS) => AvoidState::Blocked,
            Some(b) if b.cut => AvoidState::Detour,
            _ => AvoidState::Clear,
        };
        let close = braked.is_some_and(|b| b.close);
        if close {
            self.hold_reason = Some(HOLD_OBSTACLE_CLOSE.into());
        } else if self.hold_reason.as_deref() == Some(HOLD_OBSTACLE_CLOSE) {
            self.hold_reason = None;
        }
        let tx = self.tx();
        let m = match cmd {
            Command::Hold { pos, yaw } => {
                self.setpoint = Some(pos);
                // Outside `ados` no operator steers this hold (the axes are
                // zeroed): the FC's GUIDED keeps the target once given, so it
                // is sent once, and a ground station's reposition is not
                // overwritten on the next tick.
                if self.control_mode() != ControlMode::Ados {
                    let same = self
                        .fc_hold
                        .is_some_and(|h| h.iter().zip(pos).all(|(a, b)| (a - b).abs() < 0.05));
                    if same {
                        return;
                    }
                    self.fc_hold = Some(pos);
                }
                tx.setpoint(MASK_POS_VEL_YAW, pos, [0.0; 3], yaw, 0.0)
            }
            Command::Velocity { vel, yaw_rate } => {
                self.fc_hold = None;
                tx.setpoint(MASK_VELOCITY, [0.0; 3], vel, 0.0, yaw_rate)
            }
        };
        self.mav(m);
    }

    fn brake_limit(&self, pos: [f32; 3], v: V2, now_ms: u64) -> V2 {
        if self.settings.pilot.avoid == AvoidMode::Off {
            return v;
        }
        let lim = limits(self.settings.pilot.profile);
        let vel = self
            .fc
            .fresh_local(now_ms)
            .map_or([0.0; 2], |l| [l.vn, l.ve]);
        brake_limit(
            &self.obstacles,
            [pos[0], pos[1]],
            vel,
            v,
            lim.a_h,
            self.margin(),
            now_ms,
        )
        .v
    }

    /// The brake for a goal setpoint (skill or goto). With a position
    /// setpoint the FC flies the feed-forward plus its position loop's pull
    /// toward the target (`ask`), which the planner's `clear` never bounds by
    /// the stopping distance. So `ask`, and the velocity the vehicle flies
    /// now, are held to the stick brake's caps ([`brake_limit`]: seen space,
    /// obstacles, FC lag). When either breaks one, the braked `ask` within
    /// `v_cap` is returned, to fly from where the vehicle is; `None` leaves
    /// the planned setpoint.
    fn goal_brake(&mut self, pos: V2, ask: V2, v_cap: f32, now_ms: u64) -> Option<V2> {
        if self.settings.pilot.avoid == AvoidMode::Off {
            return None;
        }
        let a_h = limits(self.settings.pilot.profile).a_h;
        let margin = self.margin();
        let vel = self
            .fc
            .fresh_local(now_ms)
            .map_or([0.0; 2], |l| [l.vn, l.ve]);
        let asked = brake_limit(&self.obstacles, pos, vel, ask, a_h, margin, now_ms);
        let flying = brake_limit(&self.obstacles, pos, vel, vel, a_h, margin, now_ms);
        if !asked.cut && !flying.cut {
            return None;
        }
        let v = geo::clamp_len(asked.v, v_cap);
        self.brake_avoid = if geo::norm(v) < STOPPED_MPS {
            AvoidState::Blocked
        } else {
            AvoidState::Detour
        };
        Some(v)
    }
}
