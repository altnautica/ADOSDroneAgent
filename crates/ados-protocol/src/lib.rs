//! Canonical Rust implementation of the ADOS Drone Agent inter-process wire
//! contracts.
//!
//! The agent is a multi-process system whose services talk over a small set of
//! frozen seams. This crate implements those seams once, so a Rust service can
//! join the existing bus without any change visible to the Python services or
//! the ground station.
//!
//! Modules:
//! - [`frame`] — 4-byte big-endian length-prefixed framing, shared by the
//!   MAVLink socket, the state socket v2, and the plugin RPC socket.
//! - [`plugin`] — the plugin RPC envelope (msgpack, short keys) and the
//!   pipe-delimited HMAC-SHA256 capability token.
//! - [`state`] — the vehicle-state codec: a v1 newline-JSON reader for the
//!   migration window and a v2 length-prefixed msgpack codec.
//! - [`capabilities`] — the generated agent capability catalog (the single
//!   source of truth is `capabilities.toml`; do not edit the generated file).

pub mod capabilities;
pub mod frame;
pub mod plugin;
pub mod state;
