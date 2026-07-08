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
//! - [`framebus`] — the vision frame transport: the `vision.frame` descriptor
//!   wire shape and the shared-memory ring layout frames travel in.
//! - [`plugin`] — the plugin RPC envelope (msgpack, short keys) and the
//!   pipe-delimited HMAC-SHA256 capability token.
//! - [`state`] — the vehicle-state codec: a v1 newline-JSON reader for the
//!   migration window and a v2 length-prefixed msgpack codec.
//! - [`logd`] — the logging/telemetry wire contracts: versioned ingest frames
//!   (logs, telemetry, events, hardware snapshots), the read-API request and
//!   response envelope, and the secret-field redaction shared at ingest.
//! - [`capabilities`] — the generated agent capability catalog (the single
//!   source of truth is `capabilities.toml`; do not edit the generated file).
//! - [`dispatch`] — the generated plugin RPC dispatch table mapping each wire
//!   method to its dispatch-level required capability (source of truth is the
//!   `[[method]]` section of `capabilities.toml`).
//! - [`wfb_tables`] — the generated WFB adapter classification and
//!   management-WiFi deny-set tables (source of truth is `wfb-adapters.toml`).
//! - [`contracts`] — the generated IPC contract + sidecar version registry: the
//!   single source of truth for every wire-contract version integer (source of
//!   truth is `contracts.toml`; do not edit the generated file).
//! - [`sidecar`] — the best-effort helper for checking an on-disk state
//!   sidecar's schema version against the value this build expects: warns (never
//!   fails) on a drift so a stale sidecar from an older agent still reads.
//! - [`pairing_posture`] — the data-plane auth primitives (pairing-state read,
//!   constant-time key compare, on-box loopback trust, access decision) shared
//!   by the native HTTP control surface and the direct MAVLink WebSocket proxy.
//! - [`dashboard_session`] — self-contained HMAC session tokens the dashboard
//!   PIN gate mints, keyed off the pairing key + the PIN salt so a reset revokes
//!   every live session; accepted as an alternative data-plane credential.

pub mod atlas;
pub mod capabilities;
pub mod compute;
pub mod contracts;
pub mod crypto;
pub mod dashboard_session;
pub mod dispatch;
pub mod frame;
pub mod framebus;
pub mod hwcaps;
pub mod ipc;
pub mod logd;
#[cfg(feature = "mavlink")]
pub mod mavlink;
pub mod pairing_posture;
pub mod plugin;
pub mod rest;
pub mod sidecar;
pub mod state;
pub mod tap;
pub mod wfb_tables;
pub mod ws_ticket;
