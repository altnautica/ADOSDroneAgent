//! Perception / SLAM offload for NPU-less drones.
//!
//! A drone whose SBC has no NPU is a first-class autonomous configuration: it
//! offloads the heavy detection, tracking, and SLAM to a paired compute node and
//! flies its behaviours (follow, avoidance, navigation) on the offloaded
//! results. The hard rule, because it is a control loop: **the fast control loop
//! and the control-loop pose stay on the drone; the heavy perception and the
//! drift-corrected map pose go remote.**
//!
//! This crate is the pure agent-side logic of that split:
//! - [`pick_tier`] chooses where perception runs (local NPU / offload / hybrid)
//!   from the board's accelerator, a paired node, and the bearer.
//! - [`FreshnessGate`] tracks a return stream's age + link state.
//! - [`LockGate`] is the safety gate: stale or link-lost = lost (stop and hold),
//!   never extrapolate a stale result, never auto-re-acquire a dropped lock.
//! - [`OffloadSession`] composes them per mode (vision-only / slam-only / full)
//!   so a behaviour commands only on fresh, locked results.
//!
//! The transport (frames out, results back) is the world-model stream lane; this
//! crate owns the decision + the safety, not the I/O.

mod freshness;
mod session;
mod tier;

pub use freshness::{FreshnessGate, GateState, LockGate, LockState};
pub use session::{OffloadMode, OffloadSession, SessionStatus};
pub use tier::{pick_tier, PerceptionTier, TierInputs};
