//! Route commitment: the planner holds the route it chose instead of
//! choosing again from scratch every replan. A detour is kept (on its spot of
//! the subject's ring, re-routed from where the vehicle is, staying on the
//! side of what it passes) until the straight way to the goal is clear with
//! room to spare for several replans in a row, the goal has moved far enough
//! to choose again, or the held route is blocked; chosen again, it keeps to
//! the side of the ring it went round unless the other side is clearly
//! better ([`super::SIDE_SWITCH_COST`]). The straight way at the margin's
//! edge, which reads clear and blocked on alternate replans, never flips the
//! vehicle between the two.

use ados_protocol::pilot::{AvoidState, Goal};

use super::{clearance, detour, direct, segment_clear, Choose, Plan, Spot};
use crate::geo::{self, V2};
use crate::obstacles::Obstacles;

/// Clearance beyond the margin the straight way to the goal needs before a
/// held detour gives way to it, metres.
pub const RELEASE_ROOM_M: f32 = 0.3;
/// Replans in a row the straight way must read clear with that room before
/// a held detour gives way to it (at the 200 ms replan period, 0.6 s).
pub const RELEASE_PLANS: u8 = 3;
/// A held detour's spot is chosen again once the goal has moved this far
/// from where it was chosen, metres; until then only its route is redrawn.
pub const RECHOOSE_GOAL_M: f32 = 3.0;

/// The route the planner holds between replans.
#[derive(Debug, Clone, Default)]
pub struct Commit {
    /// The detour held: its spot on the ring, and the goal's point when the
    /// spot was chosen.
    detour: Option<(Spot, V2)>,
    /// Replans in a row the straight way has read clear with room.
    direct_clear: u8,
    /// The held detour's route, for the next search to stay near.
    path: Vec<V2>,
    /// How many times the route changed: straight to detour or back, or a
    /// detour moved to another spot.
    pub switches: u32,
}

impl Commit {
    /// Drop the held route (no goal to plan, or planning off).
    pub fn clear(&mut self) {
        self.detour = None;
        self.direct_clear = 0;
        self.path.clear();
    }

    fn hold(&mut self, plan: &Plan, spot: Spot, goal_at: V2) {
        if self.detour.is_none_or(|(s, _)| s != spot) {
            self.switches += 1;
        }
        self.detour = Some((spot, goal_at));
        self.path.clone_from(&plan.path);
    }

    fn release(&mut self) {
        if self.detour.is_some() {
            self.switches += 1;
        }
        self.clear();
    }
}

/// Plan one goal from `from`, holding the route in `held` from one replan to
/// the next ([`Commit`]): keep the goal when the straight way to it is clear,
/// otherwise route to the cheapest reachable detour candidate on the skill's
/// ring with A* over the clearance grid; a held detour stays held as the
/// module describes.
pub fn plan_committed(
    obs: &Obstacles,
    goal: &Goal,
    from: V2,
    margin: f32,
    held: &mut Commit,
) -> Plan {
    let grid = &obs.grid;
    let target = [goal.n, goal.e];
    let gc = clearance(grid, target);
    let straight = gc >= margin && segment_clear(grid, from, target, margin);
    let Some((spot, chosen_at)) = held.detour else {
        if straight {
            return direct(grid, from, target, gc);
        }
        return match detour(obs, goal, from, margin, Choose::Any { side: 0.0 }, &[]) {
            Some((plan, spot)) => {
                held.hold(&plan, spot, target);
                held.direct_clear = 0;
                plan
            }
            None => Plan::hold(AvoidState::Blocked),
        };
    };
    let room = margin + RELEASE_ROOM_M;
    let roomy = straight && gc >= room && segment_clear(grid, from, target, room);
    held.direct_clear = if roomy {
        held.direct_clear.saturating_add(1)
    } else {
        0
    };
    if held.direct_clear >= RELEASE_PLANS {
        held.release();
        return direct(grid, from, target, gc);
    }
    let prefer = std::mem::take(&mut held.path);
    // The same spot, re-routed from here, while the goal has not moved far.
    if geo::dist(target, chosen_at) < RECHOOSE_GOAL_M {
        if let Some((plan, spot)) = detour(obs, goal, from, margin, Choose::Only(spot), &prefer) {
            held.hold(&plan, spot, chosen_at);
            return plan;
        }
    }
    // Blocked, or the goal moved on: choose again, near the route flown and
    // round the same side.
    let side = if spot.0 == 0.0 { 0.0 } else { spot.0.signum() };
    match detour(obs, goal, from, margin, Choose::Any { side }, &prefer) {
        Some((plan, spot)) => {
            held.hold(&plan, spot, target);
            plan
        }
        None if straight => {
            held.release();
            direct(grid, from, target, gc)
        }
        None => {
            held.release();
            Plan::hold(AvoidState::Blocked)
        }
    }
}
