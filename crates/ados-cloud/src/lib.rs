//! Cloud relay for the ADOS Drone Agent.
//!
//! The cloud relay bridges the agent to Mission Control's cloud backend: it
//! pushes a periodic status heartbeat, polls a command queue, relays MAVLink and
//! WebRTC signaling, and polls for software updates. This crate ports those
//! pieces from `src/ados/services/cloud/` + the update poller from
//! `src/ados/services/ota/`, keeping the wire contracts byte-identical so the
//! move to Rust is invisible to the receiver.
//!
//! This is the foundation layer: the frozen heartbeat wire model, the config
//! reader, and the oneshot update poller. The long-running tasks (the MQTT
//! gateway, the MAVLink and signaling relays, the heartbeat / command-poll /
//! beacon loops, and the local auto-pair supervisor) are added incrementally on
//! top of these.
//!
//! Modules:
//! - [`heartbeat`] — the frozen `agent/status` wire model (camelCase root,
//!   snake_case `radio` sub-block, `None`-stripping).
//! - [`ota`] — the GitHub Releases update poller (ETag, full-agent tag filter,
//!   SHA256 verify).
//! - [`config`] — the slice of `/etc/ados/config.yaml` the relay reads.

pub mod config;
pub mod heartbeat;
pub mod ota;

pub use config::CloudConfig;
pub use heartbeat::{HeartbeatPayload, RadioBlock, RemoteAccess, ServiceEntry};
pub use ota::{verify_sha256, version_tuple, UpdateChecker, UpdateConfig, UpdateManifest};
