//! The trajectory planner in the control loop ([`crate::planner::TrajPlanner`]).
//!
//! Under `pilot.planner = trajectory` it plans every skill goal (but a
//! vantage goal) and goto at [`crate::planner::TRAJ_PERIOD_MS`] within the
//! profile's goal limits, and the goal flies the committed trajectory: the
//! trajectory is the shaped motion, so the reference takes its position,
//! velocity and acceleration as they are while nothing slows it. Every
//! brake and cap still applies after it: one that slows the ask hands the
//! reference back to the shaper from where it is, and the next plan starts
//! from there, so the brake can only slow what the trajectory asks. Under
//! `reactive` it plans at the grid planner's period beside it and is
//! published as the `trajectory` shadow; a goto the grid planner finds
//! blocked then follows its route.

use ados_planning::{Limits, PlanError};
use ados_protocol::pilot::{AvoidState, Goal, Owner, SensingTier};

use crate::config::PlannerMode;
use crate::core::{Pilot, ShadowCmd};
use crate::geo::{self, V2};
use crate::model::limits;
use crate::planner::{self, Outcome, TrajGoal, TrajRef};

/// The reference keeps to the committed trajectory within this, m …
const TRACK_TOL_M: f32 = 0.5;
/// … and m/s.
const TRACK_TOL_MPS: f32 = 0.5;
/// The shadow's source name.
pub const SHADOW_SOURCE: &str = "trajectory";

fn reason(out: Outcome) -> Option<&'static str> {
    match out {
        Outcome::Committed => None,
        Outcome::Kept(PlanError::NoPath) => Some("no-path"),
        Outcome::Kept(PlanError::NoBackup) => Some("no-backup"),
        Outcome::Kept(PlanError::Timeout) => Some("search-budget"),
        Outcome::Blocked => Some("blocked"),
    }
}

impl Pilot {
    /// The trajectory planner flies the goals.
    pub(in crate::core) fn traj_flies(&self) -> bool {
        self.settings.pilot.planner == PlannerMode::Trajectory
    }

    /// What the trajectory planner's last replan did (flown or in shadow);
    /// `None` while it plans no goal.
    pub fn trajectory_outcome(&self) -> Option<Outcome> {
        self.traj.last
    }

    /// Plan `goal` on the trajectory planner from where the committed
    /// trajectory is now, while the reference the goal flies keeps to it
    /// (within [`TRACK_TOL_M`] and [`TRACK_TOL_MPS`]); else from that
    /// reference (the vehicle's own state when none flew just before): a
    /// reference a brake slowed, or one the grid planner flew in shadow
    /// mode, is planned from as it is.
    fn traj_replan(&mut self, goal: &Goal, pos: [f32; 3], now_ms: u64) -> Outcome {
        let r = self.goal_ref(pos, self.yaw(now_ms), now_ms);
        let start = match self.traj.sample(now_ms) {
            Some(t)
                if geo::dist(t.p, r.p) < TRACK_TOL_M && geo::dist(t.v, r.sh.v) < TRACK_TOL_MPS =>
            {
                [t.p, t.v, t.a]
            }
            _ => [r.p, r.sh.v, r.sh.a],
        };
        let max_speed = match self.owner() {
            Owner::Goto => limits(self.settings.pilot.profile).v_h,
            _ => goal.max_speed_mps,
        };
        // The speed limit (goal, nav cap, sensing rule) bounds the plan; its
        // acceleration and jerk are the profile's stick limits (the ones
        // the sensing rule brakes on), held to any nav cap's acceleration,
        // not the goal shaper's comfort limits: the trajectory is flown as
        // it is, already smooth.
        let (v_cap, _, _) = self.goal_v_cap(max_speed, now_ms);
        let pl = self.profile_limits();
        let a_max = pl.a_acc.min(pl.a_brk).min(self.limits_now(now_ms).a_h);
        let margin = self.margin();
        let live_3d = self.world.live_3d(now_ms);
        // The depth and camera tiers see ahead only: a plan keeps inside
        // their view.
        let forward =
            live_3d && matches!(self.world.tier(), SensingTier::Depth | SensingTier::Camera);
        let g = TrajGoal {
            pos: [pos[0], pos[1]],
            start,
            goal: [goal.n, goal.e],
            goal_vel: [goal.vn, goal.ve],
            margin,
            heading: forward.then(|| self.yaw(now_ms)),
            limits: Limits {
                v_max: v_cap,
                a_max,
                j_max: pl.jerk,
            },
            now_ms,
        };
        // On a 3D tier the map's observed-free voxels at flight height are
        // what was seen; the planar scans' seen ranges otherwise.
        let vox = live_3d.then_some((&self.world, pos[2]));
        self.traj.replan(&self.obstacles, vox, &g)
    }

    /// The trajectory planner flies `goal`: replan, and take its committed
    /// plan as the plan flown.
    pub(in crate::core) fn replan_traj(&mut self, goal: &Goal, pos: [f32; 3], now_ms: u64) {
        let out = self.traj_replan(goal, pos, now_ms);
        let at = [pos[0], pos[1]];
        self.plan = match out {
            Outcome::Blocked => planner::Plan::hold(AvoidState::Blocked),
            _ => self.traj.view(&self.obstacles, at, now_ms),
        };
    }

    /// The trajectory planner beside the grid planner: plan `goal` and
    /// publish the result as the shadow.
    pub(in crate::core) fn shadow_traj(&mut self, goal: &Goal, pos: [f32; 3], now_ms: u64) {
        let out = self.traj_replan(goal, pos, now_ms);
        let cmd = self.traj.sample(now_ms).map(|r| ShadowCmd {
            at_ms: now_ms,
            pos_ne: r.p,
            vel_ne: r.v,
            acc_ne: r.a,
            path_ne: self.traj.committed.path(now_ms, 16),
            committed: r.on_backup,
            reason: reason(out),
        });
        self.set_shadow(SHADOW_SOURCE, cmd);
    }

    /// A goto the grid planner found blocked: the trajectory planner's
    /// committed path, made in shadow, as the detour route, flown toward a
    /// point [`planner::LOOKAHEAD_M`] along it. `None` when it has none:
    /// blocked, or nothing committed (a plan kept from before is dropped
    /// once what it still flies for sure meets what is seen now).
    pub(in crate::core) fn goto_route(&self, pos: V2, now_ms: u64) -> Option<planner::Plan> {
        if matches!(self.traj.last, None | Some(Outcome::Blocked)) {
            return None;
        }
        let mut p = self.traj.view(&self.obstacles, pos, now_ms);
        p.target?;
        p.target = Some(planner::along(&p.path, planner::LOOKAHEAD_M));
        p.state = AvoidState::Detour;
        Some(p)
    }

    /// The committed trajectory's reference this tick while it flies the
    /// goal; `None` when the grid planner flies or nothing is committed.
    pub(super) fn traj_ref(&self, now_ms: u64) -> Option<TrajRef> {
        if !self.traj_flies() || self.plan.target.is_none() {
            return None;
        }
        self.traj.sample(now_ms)
    }
}
