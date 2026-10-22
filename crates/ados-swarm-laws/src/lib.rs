#![doc = include_str!("../README.md")]
#![warn(missing_docs)]

pub mod barrier;
pub mod cbba;
pub mod config;
pub mod controller;
pub mod flocking;
pub mod formation;
pub mod geo;
pub mod neighbor;
pub mod precedence;
pub mod scenarios;
pub mod separation;
pub mod setpoint;
pub mod sim;
pub mod status;

pub use barrier::{BarrierOutcome, BARRIER_GAIN_HZ};
pub use cbba::{
    conflicts, consensus_reached, converge_broadcast, BidVector, CbbaAgent, CbbaTask,
    ConvergenceReport, TaskAssignment,
};
pub use config::SwarmControlConfig;
pub use controller::{
    ControlOutcome, NeighborFix, OperatorDirective, OwnState, Suppression, SwarmControlCounters,
    SwarmController, SwarmMode, CONTROL_HZ, CONTROL_PERIOD, OPERATOR_DIRECTIVE_TTL,
};
pub use flocking::{FlockTerms, FlockTuning};
pub use formation::{Formation, FormationAnchor, FormationName};
pub use geo::{GeoOrigin, Ned};
pub use neighbor::{NearestSet, NeighborState};
pub use precedence::{arbitrate, precedence_rank, ModePrecedence};
pub use separation::{HardBreach, SeparationTuning};
pub use setpoint::{Setpoint, SetpointKind, MAX_COMMAND_SPEED_MPS};
pub use status::{
    STATUS_ARMED, STATUS_EMERGENCY, STATUS_GPS_OK, STATUS_GUIDED, STATUS_HERO,
    STATUS_PRECEDENCE_MASK, STATUS_PRECEDENCE_SHIFT,
};

/// How long a neighbour survives without a beacon, and how long an empty
/// neighbour table is tolerated before the controller stops emitting setpoints.
///
/// Six missed beacons at a 2 Hz beacon rate. Six rather than two because
/// outages of about a second are normal in real formation flight, and dropping
/// a neighbour that is still there is worse than carrying a slightly old one:
/// the separation layer would stop avoiding an aircraft that has not gone
/// anywhere. A transport that prunes its neighbour table should use this same
/// constant so the table and the control loop agree.
pub const NEIGHBOR_STALE: std::time::Duration = std::time::Duration::from_secs(3);

/// How stale this drone's OWN position may be before the controller stops
/// commanding, and before a transport should stop broadcasting it.
///
/// Tighter than [`NEIGHBOR_STALE`] on purpose. A neighbour going quiet empties
/// the picture, which the controller already refuses to fly on. Our own fix
/// going quiet does something worse: it stays plausible. The frame every
/// neighbour is measured in is anchored on it, so a frozen fix keeps producing
/// confident geometry, and every other drone dead-reckons a beacon built from
/// it forward in a direction the aircraft is no longer going. At 10 Hz a
/// three-second window is thirty ticks of commanding on a dead fix, so this is
/// one second.
pub const OWN_STATE_STALE: std::time::Duration = std::time::Duration::from_secs(1);
