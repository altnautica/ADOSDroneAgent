//! Cloud relay for the ADOS Drone Agent.
//!
//! The cloud relay bridges the agent to Mission Control's cloud backend: it
//! pushes a periodic status heartbeat, polls a command queue, relays MAVLink and
//! WebRTC signaling, and polls for software updates. This crate ports those
//! pieces from `src/ados/services/cloud/` + the update poller from
//! `src/ados/services/ota/`, keeping the wire contracts byte-identical so the
//! move to Rust is invisible to the receiver.
//!
//! This layer carries the frozen heartbeat wire model, the config reader, the
//! update poller (now with its live HTTPS source), the shared TLS config, and
//! the MQTT layer (the broker transport seam, the telemetry/status gateway, the
//! bounded MAVLink relay, and the WebRTC signaling relay). The long-running
//! orchestration (the heartbeat / command-poll / beacon loops, the command
//! dispatcher, the local auto-pair supervisor, and the daemon) is added on top
//! of these.
//!
//! Modules:
//! - [`heartbeat`] — the frozen `agent/status` wire model (camelCase root,
//!   snake_case `radio` sub-block, `None`-stripping).
//! - [`ota`] — the GitHub Releases update poller (ETag, full-agent tag filter,
//!   SHA256 verify) + the live HTTPS source.
//! - [`tls`] — the shared RustCrypto-backed rustls client config.
//! - [`mqtt`] — the broker transport seam, the telemetry/status gateway, the
//!   bounded MAVLink relay, and the WebRTC signaling relay.
//! - [`config`] — the slice of `/etc/ados/config.yaml` the relay reads.

pub mod config;
pub mod heartbeat;
pub mod mqtt;
pub mod ota;
pub mod tls;

pub use config::CloudConfig;
pub use heartbeat::{HeartbeatPayload, RadioBlock, RemoteAccess, ServiceEntry};
pub use mqtt::{
    BoundedPublishQueue, MavlinkMqttRelay, MqttGateway, MqttQos, MqttTransport, RumqttcTransport,
    WebrtcSignalingRelay,
};
pub use ota::{
    verify_sha256, version_tuple, GithubSource, UpdateChecker, UpdateConfig, UpdateManifest,
};
