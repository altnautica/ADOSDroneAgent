//! Pure attitude/rate control laws — a `[lib]` with no I/O and no clock.
//!
//! Every function in this crate is a pure mapping over the body-rate / thrust
//! command set. It owns no serial port, no timer, no flight controller, no
//! `Instant`: the router rung that closes the loop supplies the live command,
//! the current attitude fix, and the staleness decision, and this crate decides
//! what (if anything) is worth commanding and clamps it into the saturable
//! range the airframe can actually hold.
//!
//! # Shape
//!
//! Mirrors `ados-swarm-control`: that crate keeps its control laws as pure
//! functions over `NeighborState` and converts them to a wire setpoint at the
//! router boundary. So here [`AttitudeCommand`] is a transport-independent data
//! value (body rates + thrust), the router turns it into a
//! `SET_ATTITUDE_TARGET` through `ados_protocol::mavlink::AttitudeSetpoint`.
//!
//! # What is deliberately not here
//!
//! * No failsafe of its own. ArduPilot's EKF failsafe, geofence and RTL act
//!   independently; when this layer has nothing trustworthy to command it emits
//!   NOTHING (the router's freshness gate suppresses) and lets the FC hold.
//! * No clock, so no time-to-live handling — freshness is the router's job and
//!   belongs with the writer that can measure it.
//!
//! # Gate discipline
//!
//! Until the G3 gate (a real Betaflight FC) passes, this work has only added a
//! second way to fly ArduPilot: it ships a control-law library plus the gate
//! test, never a live attitude command to an airframe. Hardware proof is
//! unproven in this crate's scope and is deliberately not claimed here.

pub mod command;

pub use command::{
    AttitudeCommand, AttitudeControlCounters, MAX_BODY_RATE_RAD_S, MAX_THRUST,
    MIN_THRUST, body_rate_ceiling, rate_command, thrust_range_ok,
};
