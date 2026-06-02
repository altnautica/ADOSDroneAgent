//! WFB-ng radio link manager for the ADOS Drone Agent.
//!
//! Owns the RTL8812EU adapter lifecycle (discovery, monitor-mode, injection
//! validation), spawns `wfb_tx` with proper process-group isolation so
//! the orphan-publisher bug class is structurally impossible, and drives the
//! FHSS hop supervisor and Rule-37 TX liveness watchdog.
//!
//! Mirrors `services/wfb/{manager,hop_supervisor,bind_orchestrator}.py`; the
//! Python predecessors are deleted from the codebase once this crate passes
//! the on-rig bench gate on the drone profile.

pub mod adapter;
pub mod bitrate;
pub mod channel;
pub mod cmdsock;
pub mod config;
pub mod hop;
pub mod link_proof;
pub mod link_quality;
pub mod link_state;
pub mod paths;
pub mod process;
pub mod reg_event;
pub mod rf_unverified;
pub mod watchdog;
