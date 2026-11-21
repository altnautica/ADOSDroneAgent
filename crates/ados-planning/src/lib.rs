//! Obstacle-aware motion planning for a multirotor over an occupancy query.
//!
//! One [`Planner::plan`] call runs the local pipeline in a window of the map:
//!
//! 1. Front end: jump point search on the voxel lattice (unknown voxels
//!    traversable at a cost) with up to three homotopy-distinct paths, or
//!    kinodynamic A* over acceleration primitives. A single-layer map
//!    (a [`FixedHeight`] planar map) runs the same search in 2D.
//! 2. Safe flight corridor: overlapping boxes of traversable voxels grown
//!    along each path.
//! 3. Back end: MINCO piecewise-quintic trajectory with L-BFGS, trading
//!    time against jerk energy under corridor, velocity, acceleration and
//!    jerk penalties; every sample is checked against the map and limits.
//! 4. Backup: a second trajectory branching off at the commit time that
//!    stops at rest in observed free space. No plan is returned without one.
//!
//! [`GlobalRouter`] plans a coarse 2D route over a tile grid beyond the
//! local window. Everything is deterministic and allocation is reused
//! across calls.

// The numeric kernels (distance transform, banded solver, optimiser, tables)
// index several arrays with one loop counter; iterator zips would obscure
// the maths without saving any bounds checks that matter.
#![allow(clippy::needless_range_loop)]

mod backup;
mod corridor;
mod dense;
mod dirs;
mod fallback;
mod jps;
mod kino;
mod lattice;
mod lbfgs;
mod minco;
mod planner;
mod route;
mod topo;
mod traj;
mod types;
mod window;
mod world;

pub use corridor::Corridor;
pub use dense::DenseGrid;
pub use fallback::Shape;
pub use jps::SearchKind;
pub use kino::{KinoConfig, KinoPath};
pub use minco::Weights;
pub use planner::Planner;
pub use route::{needs_replan, GlobalRouter, Route, RouteConfig, TileGrid, ROUTE_REPLAN_MS};
pub use traj::{Segment, Trajectory};
pub use types::{Config, FrontEnd, Limits, Plan, PlanError, State, Stats};
pub use world::{Aabb, Cell, FixedHeight, OccupancyQuery, PlanarQuery};
