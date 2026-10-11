// The wfb-stats sidecar `json!` literal has many keys; the macro expansion
// needs more headroom than the default recursion limit.
#![recursion_limit = "256"]
//! WFB-ng radio link manager for the ADOS Drone Agent.
//!
//! Owns the RTL8812EU adapter lifecycle (discovery, monitor-mode, injection
//! validation), spawns `wfb_tx` with proper process-group isolation so
//! the orphan-publisher bug class is structurally impossible, and drives the
//! FHSS hop supervisor and the TX liveness watchdog.
//!
//! Mirrors `services/wfb/{manager,hop_supervisor,bind_orchestrator}.py`; the
//! Python predecessors are deleted from the codebase once this crate passes
//! the on-rig bench gate on the drone profile.

pub mod adapter;
pub mod aux_cmd;
pub mod bitrate;
pub mod channel;
pub mod cmdsock;
pub mod config;
pub mod daemon;
pub mod hop;
pub mod link_proof;
pub mod link_quality;
pub mod link_state;
pub mod mcs_ladder;
pub mod paths;
pub mod process;
pub mod radio_cmd;
pub mod reg_event;
pub mod reg_gate;
pub mod reg_reassert;
pub mod rf_unverified;
pub mod tx_cmd;
pub mod tx_liveness;
pub mod watchdog;
