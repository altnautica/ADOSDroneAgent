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
//!
//! A route the vehicle crawls along is chosen again. A passage wider than
//! twice the margin but narrower than twice the brake's keep-out is routed,
//! and the brake holds the vehicle to a creep in it. When the vehicle stays
//! under [`STALL_MPS`] for [`STALL_PLANS`] replans in a row with its goal
//! still far, the planner marks the narrows round where it stalled
//! ([`Narrows`], [`STALL_MS`] on the pilot clock) and chooses again, routing
//! round them; when nothing is reachable without them it routes through them
//! as before, so they never shut the only way out.

use ados_protocol::pilot::{AvoidState, Goal};

use super::{clearance, detour, direct, segment_clear, Choose, Plan, Spot, ROUTE_ROOM_M};
use crate::geo::{self, V2};
use crate::obstacles::{Grid, Obstacles};

/// Clearance beyond the margin the straight way to the goal needs before a
/// held detour gives way to it, metres.
pub const RELEASE_ROOM_M: f32 = 0.3;
/// Replans in a row the straight way must read clear with that room before
/// a held detour gives way to it (at the 200 ms replan period, 0.6 s).
pub const RELEASE_PLANS: u8 = 3;
/// A held detour's spot is chosen again once the goal has moved this far
/// from where it was chosen, metres; until then only its route is redrawn.
pub const RECHOOSE_GOAL_M: f32 = 3.0;
/// Under this horizontal speed the vehicle is crawling, m/s.
pub const STALL_MPS: f32 = 0.3;
/// Replans in a row crawling with the goal still far before the route is
/// chosen again (at the 200 ms replan period, 2 s).
pub const STALL_PLANS: u8 = 10;
/// The goal is still far beyond this distance, metres: a vehicle at its goal
/// hovers, it does not stall.
pub const STALL_FAR_M: f32 = 3.0;
/// Radius of the narrows marked round where the vehicle stalled, metres.
pub const STALL_RADIUS_M: f32 = 2.0;
/// How long the narrows stay marked, ms of the pilot clock.
pub const STALL_MS: u64 = 10_000;

/// Narrows marked where the vehicle stalled: inside [`STALL_RADIUS_M`] of
/// `at`, a route keeps the clearance the obstacle brake flies freely
/// (`room`, [`ROUTE_ROOM_M`] beyond the margin), except where it is no
/// narrower than where the vehicle is now (`here`), so the vehicle can
/// always leave the place it is in.
#[derive(Debug, Clone, Copy)]
pub struct Narrows {
    pub at: V2,
    pub room: f32,
    pub here: f32,
}

impl Narrows {
    /// True when a route may not pass `p`, whose clearance is `c`.
    pub fn bars(&self, p: V2, c: f32) -> bool {
        c < self.room && c < self.here && geo::dist(p, self.at) < STALL_RADIUS_M
    }

    /// True when no point of `a → b` is barred.
    fn segment_clear(&self, grid: &Grid, a: V2, b: V2) -> bool {
        let d = geo::dist(a, b);
        let n = (d / (crate::obstacles::CELL_M / 2.0)).ceil().max(1.0) as usize;
        (0..=n).all(|k| {
            let p = geo::add(a, geo::scale(geo::sub(b, a), k as f32 / n as f32));
            !self.bars(p, clearance(grid, p))
        })
    }
}

/// The vehicle's motion when a plan is made: its fresh horizontal speed
/// and the pilot clock (never wall time).
#[derive(Debug, Clone, Copy)]
pub struct Motion {
    pub speed_mps: f32,
    pub now_ms: u64,
}

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
    /// Replans in a row the vehicle has crawled with its goal far.
    stalled: u8,
    /// Where it last stalled, and until when (pilot clock, ms) the narrows
    /// there stay marked.
    stall: Option<(V2, u64)>,
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
        self.stalled = 0;
        self.stall = None;
    }

    fn hold(&mut self, plan: &Plan, spot: Spot, goal_at: V2) {
        if self.detour.is_none_or(|(s, _)| s != spot) {
            self.switches += 1;
        }
        self.detour = Some((spot, goal_at));
        self.path.clone_from(&plan.path);
    }

    /// Drop the held route, keeping the stall memory.
    fn release(&mut self) {
        if self.detour.is_some() {
            self.switches += 1;
        }
        self.detour = None;
        self.direct_clear = 0;
        self.path.clear();
    }

    /// The narrows marked where the vehicle last stalled, while they last.
    fn narrows(&mut self, grid: &Grid, from: V2, margin: f32, now_ms: u64) -> Option<Narrows> {
        let (at, until) = self.stall?;
        if now_ms >= until {
            self.stall = None;
            return None;
        }
        Some(Narrows {
            at,
            room: margin + ROUTE_ROOM_M,
            here: clearance(grid, from),
        })
    }
}

/// Plan one goal from `from`, holding the route in `held` from one replan to
/// the next ([`Commit`]): keep the goal when the straight way to it is clear,
/// otherwise route to the cheapest reachable detour candidate on the skill's
/// ring with A* over the clearance grid; a held detour stays held, and a
/// route the vehicle crawls along (`motion`) is chosen again, as the module
/// describes.
pub fn plan_committed(
    obs: &Obstacles,
    goal: &Goal,
    from: V2,
    margin: f32,
    motion: Motion,
    held: &mut Commit,
) -> Plan {
    let plan = plan_held(obs, goal, from, margin, motion.now_ms, held);
    let far = geo::dist(from, [goal.n, goal.e]) > STALL_FAR_M;
    let crawling = plan.state != AvoidState::Blocked && far && motion.speed_mps < STALL_MPS;
    held.stalled = if crawling {
        held.stalled.saturating_add(1)
    } else {
        0
    };
    if held.stalled < STALL_PLANS {
        return plan;
    }
    // Stalled: mark the narrows here and choose again round them, on the
    // same side of the ring but not near the route crawled along.
    held.stalled = 0;
    held.stall = Some((from, motion.now_ms.saturating_add(STALL_MS)));
    held.path.clear();
    if let Some((spot, _)) = held.detour {
        held.detour = Some((spot, [f32::INFINITY; 2]));
    }
    plan_held(obs, goal, from, margin, motion.now_ms, held)
}

fn plan_held(
    obs: &Obstacles,
    goal: &Goal,
    from: V2,
    margin: f32,
    now_ms: u64,
    held: &mut Commit,
) -> Plan {
    let grid = &obs.grid;
    let target = [goal.n, goal.e];
    let gc = clearance(grid, target);
    let narrows = held.narrows(grid, from, margin, now_ms);
    let narrows = narrows.as_ref();
    // The straight way, and the same with the stall's narrows kept out of.
    let open = gc >= margin && segment_clear(grid, from, target, margin);
    let straight = open && narrows.is_none_or(|n| n.segment_clear(grid, from, target));
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
            None if open => direct(grid, from, target, gc),
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
        let only = Choose::Only(spot);
        if let Some((plan, spot)) = detour(obs, goal, from, margin, only, &prefer, narrows) {
            held.hold(&plan, spot, chosen_at);
            return plan;
        }
    }
    // Blocked, the goal moved on or the vehicle stalled: choose again, near
    // the route flown and round the same side.
    let side = if spot.0 == 0.0 { 0.0 } else { spot.0.signum() };
    match detour(obs, goal, from, margin, Choose::Any { side }, &prefer) {
        Some((plan, spot)) => {
            held.hold(&plan, spot, target);
            plan
        }
        None if open => {
            held.release();
            direct(grid, from, target, gc)
        }
        None => {
            held.release();
            Plan::hold(AvoidState::Blocked)
        }
    }
}
