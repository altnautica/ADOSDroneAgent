//! The controller's public data types: modes, directives, own and neighbour
//! state, and what one tick produced.

use std::time::Duration;

use crate::precedence::ModePrecedence;
use crate::setpoint::Setpoint;

/// The operator-commandable behaviour mode (`swarm.mode`).
///
/// `hard-separation` and `operator` are deliberately absent: those are precedence
/// levels the arbitration DERIVES, not modes anybody can ask for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SwarmMode {
    #[default]
    /// No behaviour layer is driving.
    Hold,
    /// Flocking drives the vehicle.
    Flocking,
    /// A named formation drives the vehicle.
    Formation,
}

impl SwarmMode {
    /// The stable wire string for this mode.
    pub const fn as_wire(self) -> &'static str {
        match self {
            Self::Hold => "hold",
            Self::Flocking => "flocking",
            Self::Formation => "formation",
        }
    }

    /// Parse `swarm.mode`. Anything unrecognised is `hold` - an unknown mode must
    /// never fly.
    pub fn from_wire(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "flocking" => Self::Flocking,
            "formation" => Self::Formation,
            _ => Self::Hold,
        }
    }
}

/// A swarm-level operator directive.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum OperatorDirective {
    /// Reposition to a geodetic point.
    Goto {
        /// Target latitude, degrees.
        lat_deg: f64,
        /// Target longitude, degrees.
        lon_deg: f64,
        /// Target altitude, metres.
        alt_m: f64,
    },
    /// Stop where you are.
    Hold,
}

/// This drone's own state, as the host already holds it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OwnState {
    /// This drone's fleet slot.
    pub slot: u8,
    /// Latitude, degrees.
    pub lat_deg: f64,
    /// Longitude, degrees.
    pub lon_deg: f64,
    /// Altitude relative to home, metres - the frame every setpoint uses.
    pub alt_rel_m: f64,
    /// Velocity north, metres/second.
    pub vn: f64,
    /// Velocity east, metres/second.
    pub ve: f64,
    /// Velocity down, metres/second.
    pub vd: f64,
    /// Whether the flight controller reports armed.
    pub armed: bool,
    /// Whether the FC reports GUIDED. No setpoint is emitted otherwise.
    pub guided: bool,
    /// How long ago the POSITION above was last refreshed by the flight
    /// controller.
    ///
    /// Not the age of the last message of any kind: a heartbeat keeps arriving
    /// long after a position stops, and that is exactly the case this exists to
    /// catch. `None` means no position has ever been seen, treated as
    /// infinitely stale.
    pub fix_age: Option<Duration>,
}

impl Default for OwnState {
    fn default() -> Self {
        Self {
            slot: 1,
            lat_deg: 0.0,
            lon_deg: 0.0,
            alt_rel_m: 0.0,
            vn: 0.0,
            ve: 0.0,
            vd: 0.0,
            armed: false,
            guided: false,
            fix_age: None,
        }
    }
}

/// A neighbour straight off the table: geodetic, already dead-reckoned forward to
/// `now` by the caller.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct NeighborFix {
    /// The neighbour's fleet slot.
    pub slot: u8,
    /// Latitude, degrees.
    pub lat_deg: f64,
    /// Longitude, degrees.
    pub lon_deg: f64,
    /// Altitude, metres.
    pub alt_m: f64,
    /// Velocity north, metres/second.
    pub vn: f64,
    /// Velocity east, metres/second.
    pub ve: f64,
    /// Down-positive, MAVLink convention: a climbing neighbour is negative.
    pub vd: f64,
    /// The beacon status byte verbatim (see [`crate::status`]).
    pub status: u8,
    /// This drone's bus sender id compared with the neighbour's
    /// (`own.cmp(&neighbour)`), `Equal` when either is unknown. Breaks the hard
    /// override's tie when two drones share a slot.
    pub sender_order: std::cmp::Ordering,
}

/// Why a tick emitted nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Suppression {
    /// `swarm.enabled` is false.
    Disabled,
    /// The neighbour table has been empty for [`crate::NEIGHBOR_STALE`].
    NeighborsStale,
    /// The vehicle is not armed.
    NotArmed,
    /// The FC is not in GUIDED.
    NotGuided,
    /// This drone's OWN position has not been refreshed inside
    /// [`crate::OWN_STATE_STALE`].
    ///
    /// Distinct from [`Suppression::NeighborsStale`], which is about what the
    /// fleet is saying. This one is about what we know of ourselves, and it is
    /// the more dangerous of the two: every neighbour's relative position is
    /// computed in a frame anchored on our own fix, so a frozen own position
    /// silently corrupts the whole picture rather than emptying it.
    OwnStateStale,
    /// Nothing is driving and the safety term has nothing to say.
    NothingToDo,
}

/// One tick's result.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ControlOutcome {
    /// The command to send this tick, if any.
    pub setpoint: Option<Setpoint>,
    /// The level ACTUALLY driving, which is what rides beacon status bits 5..7.
    pub precedence: ModePrecedence,
    /// Whether beacon status bit 2 should be raised on the next beacon.
    pub emergency: bool,
    /// Why nothing was sent, when `setpoint` is `None`.
    pub suppressed: Option<Suppression>,
}

/// Cheap counters for the status snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SwarmControlCounters {
    /// Setpoints emitted since start.
    pub setpoints_emitted: u64,
    /// Ticks on which output was suppressed.
    pub ticks_suppressed: u64,
    /// Times the hard-separation override engaged.
    pub hard_engagements: u64,
}
