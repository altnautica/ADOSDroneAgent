//! The 50 Hz control loop: failsafes, owner transitions driven by sticks, and
//! the setpoint for whoever owns motion.

use ados_protocol::pilot::{
    AvoidMode, AvoidState, ControlMode, GoalKind, Owner, Sticks, VehicleClass,
};

use super::{Out, Pilot, PLAN_PERIOD_MS, STICKS_PERIOD_MS};
use crate::fc::{heartbeat, MASK_VELOCITY};
use crate::flows::banner;
use crate::geo::{self, V2};
use crate::planner;

mod brake;
mod brake_slew;
mod energy;
mod failsafes;
mod goal_brake;
mod hold_flight;
mod motion_ref;
mod obstacle_data;
mod plan;
mod route;
mod sensing;
mod stall;
mod sticks;
mod subject_guard;
pub(super) mod supervisor;
mod trail;
mod vantage_goal;
pub use brake::*;
pub(super) use motion_ref::MotionRef;
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
        // Set again below only while this tick flies a goal or the sticks;
        // what the last tick sent stays for the brake's slew.
        self.prev_command = self.command.take();
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
        self.airframe.tick(&self.fc, now_ms);
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
        // The trajectory planner replans at its own, faster period.
        let period = if self.traj_flies() {
            planner::TRAJ_PERIOD_MS
        } else {
            PLAN_PERIOD_MS
        };
        if recovered || now_ms.saturating_sub(self.plan_ms) >= period {
            self.plan_ms = now_ms;
            if let Some(p) = self.pos_ne(now_ms) {
                self.obstacles.rebuild(p, now_ms);
            }
            self.replan(now_ms);
        }
        self.confined_tick(now_ms);
        self.sensing_tick(now_ms);
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
        let Some(pos3) = self.pos(now_ms) else {
            return;
        };
        let pos = [pos3[0], pos3[1]];
        let margin = self.margin();
        let goal = match self.owner() {
            Owner::Skill => self.auth.lease().and_then(|l| l.goal.clone()),
            Owner::Goto => self.goto.map(|g| {
                ados_protocol::pilot::Goal::new(GoalKind::Position, g, self.limits_now(now_ms).v_h)
            }),
            _ => None,
        };
        let Some(goal) = goal else {
            self.drop_routes();
            self.plan = planner::Plan::hold(AvoidState::Clear);
            return;
        };
        if goal.kind == GoalKind::Vantage {
            self.drop_routes();
            self.replan_vantage(&goal, pos, margin, now_ms);
            return;
        }
        if self.settings.pilot.avoid == AvoidMode::Off {
            self.drop_routes();
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
            Some(AvoidState::Stale) => {
                self.drop_routes();
                self.plan = planner::Plan::hold(AvoidState::Stale);
            }
            // No obstacle sensor: the goal flies straight, reported as such.
            Some(s) => {
                self.drop_routes();
                let t = [goal.n, goal.e];
                self.plan = planner::Plan {
                    state: s,
                    target: Some(t),
                    goal: Some(t),
                    path: vec![pos, t],
                    min_clearance: None,
                };
            }
            None if self.traj_flies() => {
                self.held_route.clear();
                self.replan_traj(&goal, pos3, now_ms);
            }
            None => {
                let speed_mps = self
                    .fc
                    .fresh_local(now_ms)
                    .map_or(0.0, |l| l.vn.hypot(l.ve));
                let motion = planner::Motion { speed_mps, now_ms };
                let held = &mut self.held_route;
                self.plan =
                    planner::plan_committed(&self.obstacles, &goal, pos, margin, motion, held);
                self.shadow_traj(&goal, pos3, now_ms);
                // A goto has no detour spots: blocked, it follows the
                // trajectory planner's route when there is one.
                if self.owner() == Owner::Goto && self.plan.state == AvoidState::Blocked {
                    if let Some(p) = self.goto_route(pos, now_ms) {
                        self.plan = p;
                    }
                }
            }
        }
    }

    /// Drop the routes both planners hold (no goal they plan).
    fn drop_routes(&mut self) {
        self.held_route.clear();
        self.traj.clear();
    }

    /// How many times the planner changed the route it flies: straight to a
    /// detour or back, or a detour to another spot ([`planner::Commit`]).
    pub fn route_switches(&self) -> u32 {
        self.held_route.switches
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
            // The operator's sticks fly the stick limits; the pilot's own
            // holds and landing, with no stick session, the profile's.
            Owner::Pilot if self.sessions.owner.is_some() => {
                self.fly_sticks(axes, pos, yaw, dt, now_ms)
            }
            Owner::Pilot => self.fly_hold(axes, pos, yaw, dt, now_ms),
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
