//! The routed return: a return home the FC flies (SmartRTL, RTL) that the
//! supervisor stopped for an obstacle on its way is taken over by the pilot
//! in an open place when it routes round obstacles (avoid `bypass`, or the
//! trajectory planner flying). Instead of holding until the way clears (or
//! landing where it is when it never does), the pilot flies its planner to
//! home in GUIDED, as a goto that routes round what it sees, at the height
//! it holds, and lands once over home. The low-battery reserve counts the
//! planned way home ([`Pilot::return_way_m`]), not the straight line.
//!
//! Avoid `brake` keeps stop-and-hold: a blocked return holds and lands on
//! its countdown ([`super::stall`]). So does a confined place: its passages
//! are flown back along the path flown out, never a new route.

use ados_protocol::pilot::{AvoidMode, Owner, ReturnRoute};

use super::super::Pilot;
use super::supervisor::Resume;
use super::{max_abs, GOTO_ARRIVE_M, PREEMPT_AXIS};
use crate::flows::banner;
use crate::geo::{self, V2};

/// Over home and slower than this: the routed return lands, m/s.
const HOME_SLOW_MPS: f32 = 0.5;

/// A return the pilot flies itself.
#[derive(Debug, Clone)]
pub struct Routed {
    /// When the pilot took it over.
    pub since_ms: u64,
    /// The planned way still to fly to home, m.
    pub remaining_m: f32,
    /// The straight line to home, m.
    pub straight_m: f32,
}

impl Pilot {
    /// Every tick after the supervisor: take a blocked FC return over, or
    /// fly the one taken over until it lands at home.
    pub(super) fn route_home(&mut self, now_ms: u64) {
        if self.sup.routed.is_some() {
            self.fly_routed_return(now_ms);
        } else {
            self.take_blocked_return(now_ms);
        }
    }

    /// A goto routes round what blocks it: avoid `bypass`, or the
    /// trajectory planner flying the goals (the grid planner under `brake`
    /// stops and holds a blocked goto).
    fn routes_round(&self) -> bool {
        let avoid = self.settings.pilot.avoid;
        avoid == AvoidMode::Bypass || (avoid == AvoidMode::Brake && self.traj_flies())
    }

    /// A supervisor hold of an FC return, settled in GUIDED: route home.
    fn take_blocked_return(&mut self, now_ms: u64) {
        let Some(h) = &self.sup.hold else {
            return;
        };
        if h.resume != Resume::Return
            || h.route
            || h.braking_ms.is_some()
            || !self.in_guided()
            || self.owner() != Owner::Pilot
            || self.confined()
            || !self.routes_round()
            || self.obstacle_data_state(now_ms).is_some()
            || self.position_unverified(now_ms).is_some()
        {
            return;
        }
        let Some(pos) = self.pos(now_ms) else {
            return;
        };
        // The FC's home (the local origin until it reports one), as every
        // return the supervisor watches heads for.
        let home = self.fc.home_ne();
        self.end_supervision();
        self.hold_reason = None;
        self.goto = Some([home[0], home[1], pos[2]]);
        self.set_owner(Owner::Goto, "rth-routed");
        self.plan_ms = 0;
        let straight_m = geo::dist([pos[0], pos[1]], home);
        self.sup.routed = Some(Routed {
            since_ms: now_ms,
            remaining_m: straight_m,
            straight_m,
        });
        self.banner = Some(banner("pilot.banner.return_routed", &[], now_ms));
        tracing::info!(target: "ados_pilot", mode = "routed", straight_m, "pilot_return");
    }

    /// Keep the planned way current; land once over home and slow. An
    /// operator, an action or a failsafe that took the vehicle ends it.
    fn fly_routed_return(&mut self, now_ms: u64) {
        let Some(l) = self.fc.fresh_local(now_ms) else {
            return;
        };
        let pos = [l.n, l.e];
        let home = self.fc.home_ne();
        let near = geo::dist(pos, home) < GOTO_ARRIVE_M;
        // The goto's own arrival hands to the pilot's hold first.
        let ours = match self.owner() {
            Owner::Goto => true,
            Owner::Pilot => near && self.goto.is_none(),
            _ => false,
        };
        if !ours || !self.fc.armed || self.fc.landed() || max_abs(self.raw_axes()) > PREEMPT_AXIS {
            self.end_routed_return("superseded");
            return;
        }
        if near && l.vn.hypot(l.ve) < HOME_SLOW_MPS {
            self.end_routed_return("home");
            self.land_in_place(now_ms);
            return;
        }
        let rem = self.planned_way_m(pos, home);
        if let Some(r) = &mut self.sup.routed {
            r.remaining_m = rem;
            r.straight_m = geo::dist(pos, home);
        }
    }

    fn end_routed_return(&mut self, why: &str) {
        if self.sup.routed.take().is_none() {
            return;
        }
        if self.owner() == Owner::Goto || why == "home" {
            self.goto = None;
        }
        tracing::info!(target: "ados_pilot", mode = "routed", reason = why, "pilot_return_end");
    }

    /// The planned way from `pos` to `home`: the trajectory planner's route
    /// when it holds one to home, else the plan's path on from its end; never
    /// shorter than the straight line.
    fn planned_way_m(&self, pos: V2, home: V2) -> f32 {
        let straight = geo::dist(pos, home);
        let planned = self.traj.route_remaining_m(pos, home).or_else(|| {
            let p = &self.plan.path;
            let last = *p.last()?;
            let along: f32 = p.windows(2).map(|w| geo::dist(w[0], w[1])).sum();
            Some(geo::dist(pos, p[0]) + along + geo::dist(last, home))
        });
        planned.unwrap_or(straight).max(straight)
    }

    /// The way a return home flies from `pos`: the planned way while the
    /// pilot routes it, else the FC's SmartRTL path back.
    pub(super) fn return_way_m(&self, pos: V2, home: V2) -> f32 {
        match &self.sup.routed {
            Some(r) => r.remaining_m,
            None => self.sup.trail.return_len_m(pos, home),
        }
    }

    /// The routed return on the state frame.
    pub(in crate::core) fn return_route_wire(&self) -> Option<ReturnRoute> {
        let r = self.sup.routed.as_ref()?;
        Some(ReturnRoute::new(
            r.remaining_m,
            r.straight_m,
            r.since_ms + self.epoch_offset_ms,
        ))
    }
}
