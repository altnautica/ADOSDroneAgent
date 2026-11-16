//! The 50 Hz control loop: failsafes, owner transitions driven by sticks, and
//! the setpoint for whoever owns motion.

use ados_protocol::pilot::{
    AvoidMode, AvoidState, ControlMode, GoalKind, Owner, Sticks, VehicleClass,
};

use super::position_check::HOLD_POSITION_UNVERIFIED;
use super::{Out, Pilot, PLAN_PERIOD_MS, STICKS_PERIOD_MS};
use crate::fc::{heartbeat, MASK_POS_VEL_ACC_YAW, MASK_VELOCITY};
use crate::flows::banner;
use crate::geo::{self, V2};
use crate::model::StickAsk;
use crate::planner;

mod brake;
mod failsafes;
mod goal_brake;
mod motion_ref;
mod obstacle_data;
mod plan;
mod route;
mod stall;
mod subject_guard;
pub(super) mod supervisor;
mod trail;
mod vantage_goal;
pub use brake::*;
pub(super) use motion_ref::MotionRef;
use motion_ref::{leash_within, REF_LEASH_M, SHAPER_REPORT_MPS};
#[cfg(test)]
pub use route::{check_legs, legs_of, Origin};
pub use route::{route_of, Route, RouteItem, CONFINED_CAP_MPS};
pub(super) use subject_guard::SubjectTrack;
pub use supervisor::{Resume, Supervisor};
pub use vantage_goal::HOLD_NO_VANTAGE;

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
/// `hold_reason` while avoid is on and the obstacle data is stale.
pub const HOLD_OBSTACLE_STALE: &str = "obstacle-data-stale";
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
        self.control_tick(now_ms);
        self.latch_tick();
    }

    fn control_tick(&mut self, now_ms: u64) {
        let dt = self.last_tick_ms.map_or(0.02, |t| {
            (now_ms.saturating_sub(t) as f32 / 1000.0).clamp(0.001, 0.1)
        });
        self.last_tick_ms = Some(now_ms);
        // Set again below only while this tick flies a goal or the sticks.
        self.command = None;
        self.stick = None;
        if self.failsafe.heartbeat_due(now_ms) {
            self.mav(heartbeat());
        }
        if self.banner.as_ref().is_some_and(|b| b.until_ms <= now_ms) {
            self.banner = None;
        }
        self.check_take(now_ms, false);
        if !self.fc.linked(now_ms) {
            return;
        }
        if let Some(r) = self.auth.expire(now_ms) {
            self.out.push(Out::Revoked(r));
            self.brake_hold(Some("lease-revoked"));
        }
        self.drive_launch(now_ms);
        self.track_touchdown(now_ms);
        let axes = self.stick_axes();
        if self.owner() == Owner::Tuning {
            self.tuning_axes = axes;
            // With no RC transmitter nothing else catches a lost operator
            // during a tuning flight.
            if !self.settings.pilot.rc_receiver.is_present() {
                self.failsafes(now_ms);
            }
            return;
        }
        let raw = self.raw_axes();
        self.stick_transitions(axes, raw, now_ms);
        self.failsafes(now_ms);
        self.enforce_position_check(now_ms);
        self.low_battery(now_ms);
        // Fresh scans after a stale spell (or the first scans of a lidar not
        // seen before) replan at once: a goal waits for no planning period.
        let recovered = matches!(self.plan.state, AvoidState::Stale | AvoidState::Unavailable)
            && self.obstacle_data_state(now_ms).is_none();
        if recovered || now_ms.saturating_sub(self.plan_ms) >= PLAN_PERIOD_MS {
            self.plan_ms = now_ms;
            if let Some(p) = self.pos_ne(now_ms) {
                self.obstacles.rebuild(p, now_ms);
            }
            self.replan(now_ms);
        }
        self.confined_tick(now_ms);
        self.supervise(now_ms);
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
                    self.brake_hold(None);
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

    fn replan(&mut self, now_ms: u64) {
        let Some(pos) = self.pos_ne(now_ms) else {
            return;
        };
        let margin = self.margin();
        let goal = match self.owner() {
            Owner::Skill => self.auth.lease().and_then(|l| l.goal.clone()),
            Owner::Goto => self.goto.map(|g| {
                ados_protocol::pilot::Goal::new(GoalKind::Position, g, self.limits_now(now_ms).v_h)
            }),
            _ => None,
        };
        let Some(goal) = goal else {
            self.plan = planner::Plan::hold(AvoidState::Clear);
            return;
        };
        if goal.kind == GoalKind::Vantage {
            self.replan_vantage(&goal, pos, margin, now_ms);
            return;
        }
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
        match self.obstacle_data_state(now_ms) {
            Some(AvoidState::Stale) => self.plan = planner::Plan::hold(AvoidState::Stale),
            // No obstacle sensor: the goal flies straight, reported as such.
            Some(s) => {
                let t = [goal.n, goal.e];
                self.plan = planner::Plan {
                    state: s,
                    target: Some(t),
                    goal: Some(t),
                    path: vec![pos, t],
                    min_clearance: None,
                };
            }
            None => self.plan = planner::plan(&self.obstacles, &goal, pos, margin),
        }
    }

    /// Produce this tick's command for the owner.
    fn fly(&mut self, axes: [f32; 4], dt: f32, now_ms: u64) {
        self.setpoint = None;
        self.brake_avoid = AvoidState::Clear;
        // A goal's stale hold lasts only while every tick keeps it.
        let stale_hold = self.stale_hold.take();
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
            Owner::Skill | Owner::Goto => self.fly_plan(pos, yaw, dt, stale_hold, now_ms),
            // Unknown obstacles: the swarm goal is not flown; the pilot holds.
            Owner::Swarm if self.obstacle_data_stale(now_ms) => {
                self.brake_hold(Some(HOLD_OBSTACLE_STALE));
            }
            Owner::Swarm => match self.swarm {
                Some(g) if now_ms.saturating_sub(g.at_ms) <= SWARM_STALE_MS => {
                    // The ramp first, then the brake: the brake's cut flies
                    // at once.
                    let want = self.cap_ramp([g.vel[0], g.vel[1]], now_ms);
                    let v = self.brake_limit(pos, want, now_ms);
                    self.cap_flown(v, now_ms);
                    let m = self.tx().setpoint(
                        MASK_VELOCITY,
                        [0.0; 3],
                        [v[0], v[1], g.vel[2]],
                        0.0,
                        0.0,
                    );
                    self.mav(m);
                }
                _ => self.brake_hold(None),
            },
            _ => {}
        }
    }

    /// The operator's sticks fly the pilot's one reference
    /// ([`MotionRef`]) on the stick limits: what they ask after the centre
    /// threshold and the expo is reached at the accelerate limit, and a
    /// released stick brakes to a hold at the brake limit, holding where the
    /// reference stops. The vertical speed and the yaw rate ramp and brake
    /// the same way on their own accelerations. Only a safety cut steps it:
    /// the obstacle brake, the geofence, a speed cap coming on, a height
    /// bound.
    fn fly_sticks(&mut self, axes: [f32; 4], pos: [f32; 3], yaw: f32, dt: f32, now_ms: u64) {
        let lim = self.stick_limits(now_ms);
        // An unverified position: the sticks fly slowly, and a hold is a
        // velocity brake, never a position hold on the estimate.
        let unverified = self.position_unverified(now_ms).is_some();
        let capped = unverified || self.speed_capped(now_ms);
        let agl = self.agl(now_ms);
        // Unknown obstacles: the sticks ask nothing (a landing still
        // descends), and with no scans there is nothing to push away from.
        // Scans that arrive empty where a surface should be leave the
        // vertical stick live: climbing or descending out of the scan plane
        // is how that resolves, and the planar lidar never braked it.
        let stale = self.obstacle_data_stale(now_ms);
        let mut axes = if !stale {
            axes
        } else if self.stale_on_empty_scans(now_ms) {
            [0.0, 0.0, axes[2], 0.0]
        } else {
            [0.0; 4]
        };
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
            if a >= self.settings.pilot.max_alt() {
                vz.0 = 0.0;
            }
        }
        let vel = self
            .fc
            .fresh_local(now_ms)
            .map_or([0.0; 3], |l| [l.vn, l.ve, l.vd]);
        let pos_ne = [pos[0], pos[1]];
        let unavailable = self.obstacle_data_unavailable(now_ms);
        let brake = self.settings.pilot.avoid != AvoidMode::Off && !stale && !unavailable;
        let ask = StickAsk::of(axes, yaw, &lim);
        let mut r = self.motion_ref(pos, yaw, now_ms);
        let mut limits = 0;
        // Horizontal. A speed cap that comes on mid-flight holds at once.
        if capped && geo::norm(r.sh.v) > lim.v_h + 1e-3 {
            r.sh.reset(geo::clamp_len(r.sh.v, lim.v_h));
            limits |= super::limit::NAV_CAP;
        }
        let shaped = r.sh.track_ab(ask.ne, lim.a_acc, lim.a_brk, lim.jerk, dt);
        if geo::dist(shaped, ask.ne) > SHAPER_REPORT_MPS {
            limits |= super::limit::SHAPER;
        }
        let braked = brake.then(|| {
            let geom = self.brake_geom(self.settings.pilot.effective_margin_m());
            let v2 = [vel[0], vel[1]];
            brake_limit(&self.obstacles, pos_ne, v2, shaped, lim.a_brk, geom, now_ms)
        });
        let mut v = braked.map_or(shaped, |b| b.v);
        if let Some(b) = braked {
            limits |= b.by;
        }
        let fenced = geofence(pos_ne, self.fc.home_ne(), self.settings.pilot.max_dist_m, v);
        if geo::dist(fenced, v) > 1e-3 {
            limits |= super::limit::GEOFENCE;
            v = fenced;
        }
        if unverified || geo::norm(v) < geo::norm(shaped) - 1e-3 {
            // A cut that slows flies at once from the vehicle's own
            // position, and hands its velocity back to the reference:
            // leaving the obstacle is shaped again from what flew.
            r.cut(pos_ne, v);
        } else {
            if geo::dist(v, shaped) > 1e-3 {
                // The push back out of the margin (or a fence's slide)
                // moves the reference with it, so a hold walks out.
                r.sh.reset(v);
            }
            // The reference point moves on what flies, held near the vehicle
            // while it moves; standing still it is the hold point and stays,
            // wherever the vehicle drifts (or a ground station repositions
            // it outside `ados`). Capped, the FC's pull toward the reference
            // may add only what the cap leaves.
            if v != [0.0, 0.0] {
                let reach = if capped {
                    (lim.v_h - geo::norm(v)).clamp(0.0, REF_LEASH_M)
                } else {
                    REF_LEASH_M
                };
                r.p = leash_within(geo::add(r.p, geo::scale(v, dt)), pos_ne, reach);
            }
        }
        // Vertical (NED: down positive). A floor, a ceiling or the landing's
        // descent cap the reference is past cuts it at once, and its height
        // never lies beyond the vehicle's toward that bound. Faster than the
        // profile's own speed (a profile change, a skill's descent handed
        // over), it brakes on the vertical acceleration instead.
        if r.vert.v > vz.1 && vz.1 < lim.v_down {
            r.vert.reset(vz.1);
        } else if r.vert.v < vz.0 && vz.0 > -lim.v_up {
            r.vert.reset(vz.0);
        }
        let vd = r
            .vert
            .track(ask.vd.clamp(vz.0, vz.1), lim.a_v, lim.jerk_v, dt);
        let mut d = r.d + vd * dt;
        if vz.1 <= 0.0 {
            d = d.min(pos[2]);
        }
        if vz.0 >= 0.0 {
            d = d.max(pos[2]);
        }
        if vd != 0.0 {
            r.d = d.clamp(pos[2] - REF_LEASH_M, pos[2] + REF_LEASH_M);
        } else {
            r.d = d;
        }
        // Heading: the yaw rate ramps and brakes on the yaw acceleration,
        // and the heading holds where the turn stops.
        let yaw_cmd = r.slew_rate(ask.yaw_rate, lim.yaw_dps2.to_radians(), dt);
        if unverified {
            r.d = pos[2];
        }
        // A goal taking over goes on from the velocity flown.
        r.ask = r.sh.v;
        r.at_ms = now_ms;
        self.motion_ref = Some(r);

        // Judged against what the sticks ask, not the shaped velocity: the
        // reference follows what flew, push included, so it may point back.
        self.brake_avoid = match braked {
            _ if stale => AvoidState::Stale,
            _ if unavailable => AvoidState::Unavailable,
            Some(b) if b.close || (b.cut && progress(ask.ne, b.v) < STOPPED_MPS) => {
                AvoidState::Blocked
            }
            Some(b) if b.cut => AvoidState::Detour,
            _ => AvoidState::Clear,
        };
        let reason = if stale && !self.landing {
            Some(HOLD_OBSTACLE_STALE)
        } else if braked.is_some_and(|b| b.close) {
            Some(HOLD_OBSTACLE_CLOSE)
        } else if unverified {
            Some(HOLD_POSITION_UNVERIFIED)
        } else {
            None
        };
        let passing = |r: &str| {
            matches!(
                r,
                HOLD_OBSTACLE_CLOSE | HOLD_OBSTACLE_STALE | HOLD_POSITION_UNVERIFIED
            )
        };
        match reason {
            Some(r) => {
                self.hold_reason = Some(r.into());
                limits |= super::limit::HOLD;
            }
            // Cleared: back to what a supervisor hold is about (a stall
            // waiting for the operator still holds), else no reason.
            None if self.hold_reason.as_deref().is_some_and(passing) => {
                self.hold_reason = self
                    .sup
                    .hold
                    .as_ref()
                    .filter(|_| self.supervised().is_some())
                    .map(|h| h.reason)
                    .filter(|r| !passing(r))
                    .map(Into::into);
            }
            None => {}
        }
        self.stick = Some(super::trace::StickTick {
            at_ms: now_ms,
            ask,
            v,
            a: r.sh.a,
            vd,
            yaw_rate: r.yaw_rate,
            cmd: geo::add(v, geo::scale(geo::sub(r.p, pos_ne), FC_POS_GAIN)),
            braking: ask.is_release() && !r.at_rest(),
            input_age_ms: self
                .failsafe
                .input_age_ms(now_ms)
                .map_or(0, |a| u32::try_from(a).unwrap_or(u32::MAX)),
            input_seq: self.sessions.last_seq().unwrap_or(0),
            limits,
            lim,
        });

        let tx = self.tx();
        if unverified {
            self.fc_hold = None;
            let m = tx.setpoint(MASK_VELOCITY, [0.0; 3], [v[0], v[1], vd], 0.0, r.yaw_rate);
            self.mav(m);
            return;
        }
        let sp = [r.p[0], r.p[1], r.d];
        self.setpoint = Some(sp);
        // Outside `ados` no operator steers this hold (the axes are zeroed):
        // the FC's GUIDED keeps the target once given, so a reference at
        // rest is sent once, and a ground station's reposition is not
        // overwritten on the next tick.
        if self.control_mode() != ControlMode::Ados && r.at_rest() {
            let same = self
                .fc_hold
                .is_some_and(|h| h.iter().zip(sp).all(|(a, b)| (a - b).abs() < 0.05));
            if same {
                return;
            }
            self.fc_hold = Some(sp);
        } else {
            self.fc_hold = None;
        }
        let m = tx.setpoint_acc(
            MASK_POS_VEL_ACC_YAW,
            sp,
            [v[0], v[1], vd],
            [r.sh.a[0], r.sh.a[1], r.vert.a],
            yaw_cmd,
            0.0,
        );
        self.mav(m);
    }
}

/// How fast `v` still goes the way `ask` asked, m/s (the speed of `v` when
/// nothing was asked): a brake that leaves only a push sideways or back out
/// of the margin has stopped the asked motion.
fn progress(ask: V2, v: V2) -> f32 {
    let n = geo::norm(ask);
    if n < 1e-3 {
        geo::norm(v)
    } else {
        geo::dot(v, ask) / n
    }
}
