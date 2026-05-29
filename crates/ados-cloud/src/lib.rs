//! Cloud relay for the ADOS Drone Agent.
//!
//! The cloud relay bridges the agent to Mission Control's cloud backend: it
//! pushes a periodic status heartbeat, polls a command queue, relays MAVLink and
//! WebRTC signaling, and polls for software updates. This crate ports those
//! pieces from `src/ados/services/cloud/` + the update poller from
//! `src/ados/services/ota/`, keeping the wire contracts byte-identical so the
//! move to Rust is invisible to the receiver.
//!
//! This layer carries the frozen heartbeat wire model, the config + pairing
//! readers, the update poller (with its live HTTPS source), the shared TLS
//! config, the MQTT layer (transport seam, telemetry/status gateway, bounded
//! MAVLink relay, WebRTC signaling relay), the periodic loops (heartbeat /
//! command-poll / pairing-beacon), the command dispatcher (idempotent, plugin
//! lifecycle over the frozen supervisor), and the WFB auto-pair supervisor.
//!
//! Modules:
//! - [`heartbeat`] — the frozen `agent/status` wire model (camelCase root,
//!   snake_case `radio` sub-block, `None`-stripping).
//! - [`ota`] — the GitHub Releases update poller (ETag, full-agent tag filter,
//!   SHA256 verify) + the live HTTPS source.
//! - [`tls`] — the shared RustCrypto-backed rustls client config.
//! - [`mqtt`] — the broker transport seam, the telemetry/status gateway, the
//!   bounded MAVLink relay, and the WebRTC signaling relay.
//! - [`loops`] — the heartbeat / command-poll / pairing-beacon loops.
//! - [`dispatch`] — the cloud command dispatcher (idempotency, download
//!   allowlist, plugin lifecycle over the frozen `PluginSupervisor`).
//! - [`auto_pair`] — the WFB auto-pair supervisor (hosted here for the
//!   no-self-kill invariant; forwards bind over the supervisor control socket).
//! - [`pairing`] — the pairing-state reader the loops gate on.
//! - [`config`] — the slice of `/etc/ados/config.yaml` the relay reads.

pub mod auto_pair;
pub mod config;
pub mod dispatch;
pub mod heartbeat;
pub mod loops;
pub mod mqtt;
pub mod ota;
pub mod pairing;
pub mod tls;

pub use config::CloudConfig;
pub use dispatch::{CommandResult, CommandStatus};
pub use heartbeat::{HeartbeatPayload, RadioBlock, RemoteAccess, ServiceEntry};
pub use mqtt::{
    BoundedPublishQueue, MavlinkMqttRelay, MqttGateway, MqttQos, MqttTransport, RumqttcTransport,
    WebrtcSignalingRelay,
};
pub use ota::{
    verify_sha256, version_tuple, GithubSource, UpdateChecker, UpdateConfig, UpdateManifest,
};
pub use pairing::PairingState;
