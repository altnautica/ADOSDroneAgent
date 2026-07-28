//! Onboard swarm autonomy — the layer that flies the fleet.
//!
//! Consumes the swarm neighbour table (`ados-swarmbus`) and commands the flight
//! controller through the router's existing `SET_POSITION_TARGET_GLOBAL_INT`
//! path. It adds NO radio traffic of its own: every input is already on the air
//! for other reasons, so a 24-drone swarm costs exactly the beacon bandwidth and
//! nothing more.
//!
//! # Shape
//!
//! Every control law in this crate is a PURE FUNCTION over a slice of
//! [`NeighborState`] in a local NED frame. No clock, no socket, no radio, no
//! flight controller. The only stateful piece is [`SwarmController`], which
//! holds the handful of latches a safety layer needs (the hard-override dwell,
//! the staleness deadline, the climb datum) and does the geodetic-to-local frame
//! conversion at the boundary. That split is what makes the whole layer testable
//! on a laptop and is why the four flight scenarios run as ordinary unit tests.
//!
//! # The precedence ladder
//!
//! Highest first: hard separation, operator direct command, formation, flocking,
//! hold. The ACTIVE level — not the commanded one — is what
//! [`SwarmController::precedence`] reports and what rides beacon status bits
//! 5..7 to the operator screen. A drone whose separation layer has taken over
//! reads `hard-separation`, never the mode somebody asked for.
//!
//! # What is deliberately not here
//!
//! * No leader election. The operator screen is the single authority by
//!   construction; electing a leader among drones over a lossy broadcast invites
//!   a split brain — a "leader" that still hears peers but has lost the operator
//!   — for no benefit.
//! * No failsafe of its own. ArduPilot's `FS_GCS_ENABLE` / `FS_LONG_ACTN` and
//!   the geofence are the backstop and act independently. When this layer has
//!   nothing trustworthy to fly on it emits NOTHING and lets the FC hold; it
//!   never competes with the autopilot's own failsafes.

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
pub mod swarmbus;

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
pub use swarmbus::{fixes_from_payload, precedence_from_wire, EXTRA_EMERGENCY, EXTRA_PRECEDENCE};

/// How long an empty neighbour table is tolerated before this layer stops
/// emitting setpoints entirely.
///
/// Six missed beacons at the 2 Hz beacon rate. Mirrors
/// `ados_swarmbus::NEIGHBOR_STALE`, which is the authority; the constant is
/// restated here so every control law in this crate can be reasoned about (and
/// tested) without the transport in scope, and [`crate::controller`] pins the
/// two together.
pub const NEIGHBOR_STALE: std::time::Duration = std::time::Duration::from_secs(3);
