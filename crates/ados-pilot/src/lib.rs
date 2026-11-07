//! `ados-pilot`: the on-board command authority.
//!
//! While the control mode is `ados` the pilot is the only producer of motion
//! setpoints: it shapes operator sticks into velocity and hold setpoints, runs
//! skill goals, gotos and swarm goals through its obstacle planner, applies the
//! geofence and failsafes, and streams `SET_POSITION_TARGET_LOCAL_NED` to a
//! flight controller flying GUIDED. The flight controller keeps stabilisation,
//! the EKF, mixing and its own last-resort failsafe, which watches the pilot's
//! heartbeat.
//!
//! The decision logic ([`core::Pilot`]) is a pure state machine driven by FC
//! messages, client requests and a 50 Hz tick; the binary wires it to the
//! MAVLink socket, the `pilot.v1` socket and the plugin event export.

pub mod authority;
pub mod config;
pub mod core;
pub mod events;
pub mod failsafe;
pub mod fc;
pub mod fc_profile;
pub mod fixedwing;
pub mod flows;
pub mod geo;
pub mod ipc;
pub mod model;
pub mod modes;
pub mod obstacles;
pub mod planner;
pub mod readiness;
pub mod runtime;
