//! The planner's inputs, settings and outputs.

use std::fmt;

use crate::corridor::Corridor;
use crate::fallback::Shape;
use crate::kino::KinoConfig;
use crate::minco::Weights;
use crate::traj::Trajectory;

/// A vehicle state: position, velocity, acceleration.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct State {
    pub p: [f32; 3],
    pub v: [f32; 3],
    pub a: [f32; 3],
}

/// Dynamic limits (norms), SI units.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Limits {
    pub v_max: f32,
    pub a_max: f32,
    pub j_max: f32,
}

/// Why no plan came out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanError {
    /// No traversable, flyable path to the goal exists in the window.
    NoPath,
    /// Paths exist but none has a backup that stops in observed free space.
    NoBackup,
    /// The search budget ran out first.
    Timeout,
}

impl fmt::Display for PlanError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            PlanError::NoPath => "no path",
            PlanError::NoBackup => "no backup trajectory",
            PlanError::Timeout => "search budget exhausted",
        })
    }
}

impl std::error::Error for PlanError {}

/// Which front end finds the geometric path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrontEnd {
    /// Jump point search with up to [`Config::alternatives`] homotopy classes.
    Jps,
    /// Kinodynamic A* (one path).
    Kinodynamic,
}

/// Planner settings.
#[derive(Debug, Clone, Copy)]
pub struct Config {
    /// Vehicle body radius, metres.
    pub radius_m: f32,
    /// Clearance kept beyond the body, metres.
    pub margin_m: f32,
    /// Cost multiplier for unknown voxels.
    pub unknown_cost: f32,
    /// Heights the plan may use (`[low, high]` in the map frame); `None` for
    /// the whole map. A start outside the band may still be left.
    pub z_band: Option<[f32; 2]>,
    /// Search window padding around start and goal: horizontal, vertical.
    pub pad_m: [f32; 2],
    pub front_end: FrontEnd,
    /// Homotopy classes the JPS front end offers the back end (≥ 1).
    pub alternatives: usize,
    /// Horizontal radius of the tube (over every height) an accepted path
    /// forbids to later alternatives.
    pub tube_m: f32,
    /// Alternatives longer than this times the best path are dropped.
    pub alt_cost_ratio: f32,
    /// Heuristic inflation of the grid search: 1 is optimal; above it the
    /// search expands far fewer voxels and its path costs at most this
    /// factor more.
    pub heuristic_weight: f32,
    /// Largest corridor box, metres per axis.
    pub box_max_m: [f32; 3],
    /// Voxels a search may scan before giving up with
    /// [`PlanError::Timeout`].
    pub max_scan: u64,
    /// A goal inside blocked space moves to the nearest traversable voxel
    /// within this distance.
    pub goal_snap_m: f32,
    pub kino: KinoConfig,
    pub weights: Weights,
    pub max_iters: u32,
    /// Penalty samples per trajectory piece.
    pub samples: usize,
    /// The optimiser aims within this fraction of each dynamic limit.
    pub limit_aim: f32,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            radius_m: 0.25,
            margin_m: 0.3,
            unknown_cost: 2.0,
            z_band: None,
            pad_m: [4.0, 1.5],
            front_end: FrontEnd::Jps,
            alternatives: 3,
            tube_m: 1.0,
            alt_cost_ratio: 1.5,
            heuristic_weight: 1.5,
            box_max_m: [5.0, 5.0, 3.0],
            max_scan: 3_000_000,
            goal_snap_m: 1.0,
            kino: KinoConfig::default(),
            weights: Weights::default(),
            max_iters: 120,
            samples: 12,
            limit_aim: 0.95,
        }
    }
}

/// One plan: fly `exec` from `made_ms`; if no newer plan has arrived by
/// `t_commit` seconds in, fly `backup` from there instead. `backup` starts
/// at `exec`'s state at `t_commit` and ends at rest in observed free space.
#[derive(Debug, Clone, PartialEq)]
pub struct Plan {
    pub exec: Trajectory,
    pub backup: Trajectory,
    pub t_commit: f32,
    pub made_ms: u64,
    /// Where `exec` ends at rest: the goal, or the point the window clipped
    /// or snapped it to.
    pub end: [f32; 3],
    /// True when `end` is the requested goal.
    pub reaches_goal: bool,
    /// The front-end path the trajectory follows: voxel centres of its
    /// turning points (JPS) or its sampled states (kinodynamic).
    pub path: Vec<[f32; 3]>,
    /// The corridor `exec` was optimised in.
    pub corridor: Corridor,
    /// Homotopy classes the back end compared (0 for an emergency stop).
    pub candidates: u8,
    /// How `exec` was shaped (optimised, stretched, stop-and-go, or an
    /// emergency stop).
    pub shape: Shape,
}

impl Plan {
    /// Position, velocity, acceleration `t` seconds after `made_ms`, on the
    /// backup after `t_commit` when `on_backup`.
    pub fn sample(&self, t: f32, on_backup: bool) -> ([f32; 3], [f32; 3], [f32; 3]) {
        if on_backup && t > self.t_commit {
            self.backup.sample(t - self.t_commit)
        } else {
            self.exec.sample(t)
        }
    }
}

/// Time one [`crate::Planner::plan`] call spent in each stage, in the units
/// of the clock given to [`crate::Planner::set_stage_clock`] (zero without
/// one).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Stats {
    /// Window, start check and goal snap.
    pub prepare: f64,
    /// The first front-end search.
    pub front: f64,
    /// Homotopy alternatives.
    pub alternatives: f64,
    /// Corridor growth for every candidate.
    pub corridor: f64,
    /// Trajectory optimisation for every candidate.
    pub optimise: f64,
    /// Flyability checks and the fallback shapes.
    pub checks: f64,
    /// Backup trajectory.
    pub backup: f64,
}
