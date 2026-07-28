//! CBBA task allocation across the fleet.
//!
//! Consensus-Based Bundle Algorithm: each drone greedily builds a bundle of tasks
//! it would like (phase 1), broadcasts its bid vector, and resolves conflicts
//! against every peer's vector with a fixed decision table (phase 2,
//! [`consensus`]). No coordinator, no leader, no central assignment.
//!
//! # Why one round is enough here
//!
//! The literature bounds convergence at `N_min · D` iterations, where `D` is the
//! diameter of the interaction graph — a mesh of point-to-point links needs
//! `D` hops for one agent's bid to reach the far side. A shared broadcast medium
//! has `D = 1`: every drone hears every drone's bid directly, so the diameter
//! factor vanishes and the bound collapses to `N_min`. When the greedy bundles
//! are already conflict-free, that is literally ONE round — one build, one
//! broadcast, done, which [`converge_broadcast`] measures rather than assumes.
//!
//! # Rate
//!
//! Bids are EVENT-DRIVEN: a task-set change or a reallocation, never a periodic
//! tick. This module deliberately exposes no timer.

pub mod bid;
pub mod consensus;

use std::collections::BTreeMap;

pub use bid::{BidVector, BID_BYTES_PER_AGENT, BID_BYTES_PER_TASK};
pub use consensus::TableAction;

use crate::geo::Ned;

/// Per-second reward discount. The score is `Σ λ^{t_j} r_j` over the path's
/// arrival times, which is what gives CBBA its diminishing-marginal-gain
/// property — the property the convergence proof rests on. A plain
/// `reward − distance` score does NOT satisfy it and can livelock.
pub const CBBA_DISCOUNT: f64 = 0.95;

/// Assumed transit speed for scoring, m/s. Only ratios matter, so this need not
/// match the vehicle exactly; it must be positive and the same on every drone.
pub const CBBA_SPEED_MPS: f64 = 8.0;

/// Default tasks per drone.
pub const CBBA_BUNDLE_CAPACITY: usize = 5;

/// A task to be allocated: a waypoint or survey cell.
#[derive(Debug, Clone, PartialEq)]
pub struct CbbaTask {
    pub id: String,
    /// Position in the fleet's shared local NED frame.
    pub pos: Ned,
    pub reward: f32,
}

impl CbbaTask {
    pub fn new(id: impl Into<String>, pos: Ned, reward: f32) -> Self {
        Self {
            id: id.into(),
            pos,
            reward,
        }
    }
}

/// What this drone is currently assigned — the read-only pair the settings page
/// surfaces (`swarm.tasks.assigned_task_id`, `swarm.tasks.bundle_position`).
/// Never the bid internals: the operator wants the assignment, not the auction.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TaskAssignment {
    pub task_id: Option<String>,
    pub bundle_position: Option<usize>,
}

/// One drone's auction state.
#[derive(Debug, Clone)]
pub struct CbbaAgent {
    pub slot: u8,
    /// Where this drone starts its path, in the shared local frame.
    pub pos: Ned,
    pub speed: f64,
    pub capacity: usize,
    pub discount: f64,
    bundle: Vec<usize>,
    path: Vec<usize>,
    vector: BidVector,
    scratch: Vec<usize>,
}

impl CbbaAgent {
    pub fn new(slot: u8, pos: Ned, n_tasks: usize, n_agents: usize) -> Self {
        Self {
            slot,
            pos,
            speed: CBBA_SPEED_MPS,
            capacity: CBBA_BUNDLE_CAPACITY,
            discount: CBBA_DISCOUNT,
            bundle: Vec::new(),
            path: Vec::new(),
            vector: BidVector::new(n_tasks, n_agents),
            scratch: Vec::new(),
        }
    }

    pub fn with_capacity(mut self, capacity: usize) -> Self {
        self.capacity = capacity.max(1);
        self
    }

    pub fn vector(&self) -> &BidVector {
        &self.vector
    }

    /// Tasks in execution order.
    pub fn path(&self) -> &[usize] {
        &self.path
    }

    /// Tasks in the order they were added — the order releases walk.
    pub fn bundle(&self) -> &[usize] {
        &self.bundle
    }

    /// The next task and its position in the bundle.
    pub fn assignment(&self, tasks: &[CbbaTask]) -> TaskAssignment {
        let Some(&next) = self.path.first() else {
            return TaskAssignment::default();
        };
        TaskAssignment {
            task_id: tasks.get(next).map(|t| t.id.clone()),
            bundle_position: self.bundle.iter().position(|&j| j == next),
        }
    }

    /// Phase 1: greedily grow the bundle while a task's marginal score beats the
    /// standing winning bid. Returns whether anything was added.
    pub fn build_bundle(&mut self, tasks: &[CbbaTask]) -> bool {
        let mut changed = false;
        while self.bundle.len() < self.capacity && self.path.len() < tasks.len() {
            let base = path_score(self.pos, self.speed, self.discount, tasks, &self.path);
            let mut best: Option<(usize, usize, f32)> = None;
            for j in 0..tasks.len() {
                if self.path.contains(&j) {
                    continue;
                }
                let mut here: Option<(usize, f64)> = None;
                for at in 0..=self.path.len() {
                    self.scratch.clear();
                    self.scratch.extend_from_slice(&self.path);
                    self.scratch.insert(at, j);
                    let marginal =
                        path_score(self.pos, self.speed, self.discount, tasks, &self.scratch)
                            - base;
                    if here.is_none_or(|(_, m)| marginal > m) {
                        here = Some((at, marginal));
                    }
                }
                let Some((at, marginal)) = here else { continue };
                // Compare at WIRE precision. The score is computed in f64 but the
                // bid vector carries f32, and `f32(x) < x` for most x — so an f64
                // comparison against a widened f32 makes a drone find its OWN
                // losing bid beatable, re-bid the identical value, get outbid
                // again, and oscillate until the round bound. Rounding first makes
                // "beats the standing bid" exact and terminating.
                let marginal = marginal as f32;
                // Strictly beat the standing bid, and prefer the lowest task index
                // on a tie so every drone's greedy step is reproducible.
                if marginal > self.vector.y[j] && best.is_none_or(|(_, _, m)| marginal > m) {
                    best = Some((j, at, marginal));
                }
            }
            let Some((j, at, marginal)) = best else { break };
            self.path.insert(at, j);
            self.bundle.push(j);
            self.vector.y[j] = marginal;
            self.vector.z[j] = Some(self.slot);
            changed = true;
        }
        changed
    }

    /// Phase 2: integrate a peer's bid vector. Returns whether the ASSIGNMENT
    /// changed; the information timestamps are merged either way but do not count,
    /// since a monotonic clock would otherwise make the auction never settle.
    pub fn receive(&mut self, sender: u8, theirs: &BidVector, roster: &[u8], now: u16) -> bool {
        if theirs.y.len() != self.vector.y.len() || theirs.s.len() != self.vector.s.len() {
            return false;
        }
        let mut changed = false;
        for j in 0..self.vector.y.len() {
            let action = consensus::decide(j, sender, self.slot, theirs, &self.vector, roster);
            changed |= consensus::apply(action, j, theirs, &mut self.vector);
        }
        consensus::merge_stamps(&mut self.vector, theirs, roster, sender, now);
        changed
    }

    /// Drop the first task this drone no longer holds and everything added after
    /// it. Those later bids were priced against a path that no longer exists, so
    /// keeping them would leave the fleet holding stale prices.
    pub fn release_lost(&mut self) -> bool {
        let Some(from) = self
            .bundle
            .iter()
            .position(|&j| self.vector.z[j] != Some(self.slot))
        else {
            return false;
        };
        for n in (from..self.bundle.len()).rev() {
            let j = self.bundle[n];
            if n > from {
                self.vector.y[j] = 0.0;
                self.vector.z[j] = None;
            }
            if let Some(at) = self.path.iter().position(|&t| t == j) {
                self.path.remove(at);
            }
            self.bundle.pop();
        }
        true
    }
}

/// Time-discounted path reward: `Σ λ^{arrival time} · reward`.
fn path_score(start: Ned, speed: f64, discount: f64, tasks: &[CbbaTask], path: &[usize]) -> f64 {
    let speed = if speed.is_finite() && speed > 0.0 {
        speed
    } else {
        CBBA_SPEED_MPS
    };
    let mut at = start;
    let mut t = 0.0;
    let mut score = 0.0;
    for &j in path {
        let Some(task) = tasks.get(j) else { continue };
        t += (task.pos - at).norm() / speed;
        score += discount.powf(t) * task.reward as f64;
        at = task.pos;
    }
    score
}

/// What [`converge_broadcast`] measured.
#[derive(Debug, Clone, PartialEq)]
pub struct ConvergenceReport {
    /// Communication rounds in which the assignment changed. This is the figure
    /// the diameter-1 claim is about.
    pub rounds: usize,
    /// Whether the auction reached a fixed point inside the round bound.
    pub converged: bool,
    /// Whether every drone ends up believing the same thing.
    pub consensus: bool,
    /// Whether no task is claimed by two drones.
    pub conflict_free: bool,
    /// Final paths, by slot.
    pub assignment: BTreeMap<u8, Vec<usize>>,
}

/// Run the auction to a fixed point over a broadcast medium: every drone hears
/// every drone in every round.
pub fn converge_broadcast(
    agents: &mut [CbbaAgent],
    tasks: &[CbbaTask],
    roster: &[u8],
) -> ConvergenceReport {
    // Hard bound so a pathological input terminates. `N_min · D` with `D = 1` is
    // the theoretical figure; this is generous by a whole factor of the fleet.
    let bound = tasks.len() * agents.len() + 2;
    let mut rounds = 0;
    let mut converged = false;
    for round in 1..=bound {
        let mut changed = false;
        for a in agents.iter_mut() {
            changed |= a.build_bundle(tasks);
        }
        let snapshots: Vec<(u8, BidVector)> =
            agents.iter().map(|a| (a.slot, a.vector.clone())).collect();
        for a in agents.iter_mut() {
            for (slot, v) in &snapshots {
                if *slot == a.slot {
                    continue;
                }
                changed |= a.receive(*slot, v, roster, round as u16);
            }
            changed |= a.release_lost();
        }
        if !changed {
            converged = true;
            break;
        }
        rounds = round;
    }
    ConvergenceReport {
        rounds,
        converged,
        consensus: consensus_reached(agents),
        conflict_free: conflicts(agents).is_empty(),
        assignment: agents.iter().map(|a| (a.slot, a.path.to_vec())).collect(),
    }
}

/// Tasks claimed by more than one drone.
pub fn conflicts(agents: &[CbbaAgent]) -> Vec<usize> {
    let mut count: BTreeMap<usize, usize> = BTreeMap::new();
    for a in agents {
        for &j in &a.path {
            *count.entry(j).or_default() += 1;
        }
    }
    count
        .into_iter()
        .filter(|(_, n)| *n > 1)
        .map(|(j, _)| j)
        .collect()
}

/// Whether every drone's winner vector agrees.
pub fn consensus_reached(agents: &[CbbaAgent]) -> bool {
    let mut iter = agents.iter();
    let Some(first) = iter.next() else {
        return true;
    };
    iter.all(|a| a.vector.z == first.vector.z)
}

#[cfg(test)]
mod tests;
