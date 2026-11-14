//! A `vantage` skill goal: plan toward the point that sees the lost
//! subject's predicted area, and tell the lease holder how it goes.

use ados_protocol::pilot::{AvoidMode, AvoidState, Goal, GoalStatus, VantageState};

use crate::core::{Out, Pilot};
use crate::geo::{self, V2};
use crate::planner;
use crate::vantage;

/// A vantage goal has arrived within this of its point...
const VANTAGE_ARRIVE_M: f32 = 1.0;
/// ...and below this ground speed.
const VANTAGE_STILL_MPS: f32 = 0.3;
/// `hold_reason` while a vantage goal finds no point that sees the
/// predicted area.
pub const HOLD_NO_VANTAGE: &str = "no-vantage";

impl Pilot {
    pub(super) fn replan_vantage(&mut self, goal: &Goal, pos: V2, margin: f32, now_ms: u64) {
        let Some(lease_id) = self.auth.lease().map(|l| l.id) else {
            return;
        };
        let Some(spec) = &goal.vantage else {
            self.plan = planner::Plan::hold(AvoidState::Clear);
            self.out.push(Out::GoalStatus(GoalStatus::new(
                lease_id,
                VantageState::NoVantage,
                0.0,
            )));
            return;
        };
        // No fresh scan, for any reason (never seen, stale, avoidance off):
        // the obstacle map does not show what hides the subject, and an
        // empty map would claim a full view from anywhere. The vehicle holds
        // and the holder hears no vantage, with no coverage.
        if !self.obstacles.scan_fresh(now_ms) {
            let state = match self.obstacle_data_state(now_ms) {
                Some(s) => s,
                None if self.settings.pilot.avoid == AvoidMode::Off => AvoidState::Off,
                None => AvoidState::Stale,
            };
            self.plan = planner::Plan::hold(state);
            self.out.push(Out::GoalStatus(GoalStatus::new(
                lease_id,
                VantageState::NoVantage,
                0.0,
            )));
            return;
        }
        let v_cap = goal.max_speed_mps.min(self.limits_now(now_ms).v_h).max(0.5);
        let fence = Some((self.fc.home_ne(), self.settings.pilot.max_dist_m));
        let c = vantage::choose(&self.obstacles, spec, pos, fence, margin, v_cap);
        let mut status = GoalStatus::new(lease_id, VantageState::NoVantage, c.coverage);
        status.occluder_ne = c.occluder.map(|o| o.ne);
        status.occluder_width_m = c.occluder.map(|o| o.width_m);
        match c.point {
            Some(p) => {
                let speed = self
                    .fc
                    .fresh_local(now_ms)
                    .map_or(f32::INFINITY, |l| l.vn.hypot(l.ve));
                status.vantage_ne = Some(p);
                status.state = if geo::dist(pos, p) < VANTAGE_ARRIVE_M && speed < VANTAGE_STILL_MPS
                {
                    VantageState::Arrived
                } else {
                    VantageState::Enroute
                };
                // A straight, free way to the point is clear flight, flown
                // and drawn straight; only a way round something is avoiding.
                self.plan = if planner::segment_clear(&self.obstacles.grid, pos, p, margin) {
                    let minc = self.obstacles.grid.clearance(p);
                    planner::Plan {
                        state: AvoidState::Clear,
                        target: Some(p),
                        goal: Some(p),
                        path: vec![pos, p],
                        min_clearance: minc.filter(|c| c.is_finite()),
                    }
                } else {
                    let minc = c
                        .path
                        .iter()
                        .filter_map(|q| self.obstacles.grid.clearance(*q))
                        .fold(f32::INFINITY, f32::min);
                    planner::Plan {
                        state: AvoidState::Detour,
                        target: Some(planner::along(&c.path, planner::LOOKAHEAD_M)),
                        goal: Some(p),
                        path: planner::decimate(&c.path, 16),
                        min_clearance: minc.is_finite().then_some(minc),
                    }
                };
            }
            // Nothing blocks the vehicle: no point sees the area. The hold
            // says so (`no-vantage`), not "blocked".
            None => self.plan = planner::Plan::hold(AvoidState::Clear),
        }
        self.out.push(Out::GoalStatus(status));
    }
}
