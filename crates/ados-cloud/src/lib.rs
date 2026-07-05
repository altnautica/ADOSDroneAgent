//! Cloud relay for the ADOS Drone Agent.
//!
//! The cloud relay bridges the agent to Mission Control's cloud backend: it
//! pushes a periodic status heartbeat, polls a command queue, and relays MAVLink
//! and WebRTC signaling. This crate ports those pieces from
//! `src/ados/services/cloud/`, keeping the wire contracts byte-identical so the
//! move to Rust is invisible to the receiver.
//!
//! This layer carries the frozen heartbeat wire model, the config + pairing
//! readers, the shared TLS config, the MQTT layer (transport seam,
//! telemetry/status gateway, bounded MAVLink relay, WebRTC signaling relay), the
//! periodic loops (heartbeat / command-poll / pairing-beacon), the command
//! dispatcher (idempotent, plugin lifecycle over the frozen supervisor), the
//! ground-station relay bridge (uplink-aware MQTT supervision + data-cap
//! downshift + GS status heartbeat).
//!
//! Modules:
//! - [`heartbeat`] — the frozen `agent/status` wire model (camelCase root,
//!   snake_case `radio` sub-block, `None`-stripping).
//! - [`tls`] — the shared RustCrypto-backed rustls client config.
//! - [`mqtt`] — the broker transport seam, the telemetry/status gateway, the
//!   bounded MAVLink relay, and the WebRTC signaling relay.
//! - [`loops`] — the heartbeat / command-poll / pairing-beacon loops.
//! - [`dispatch`] — the cloud command dispatcher (idempotency, download
//!   allowlist, plugin lifecycle over the frozen `PluginSupervisor`).
//! - [`ground_station`] — the uplink-aware cloud relay bridge: explicit MQTT
//!   teardown/reconnect on uplink change, data-cap downshift, and the 30 s GS
//!   `/agent/status` heartbeat.
//! - [`pairing`] — the pairing-state reader the loops gate on.
//! - [`config`] — the slice of `/etc/ados/config.yaml` the relay reads.
//! - [`log_push`] — the explicit, account-gated cloud export of a chosen log
//!   window from the durable on-device store, driven by an operator-triggered
//!   request file and default-off.

pub mod atlas_bearer;
pub mod config;
pub mod dispatch;
pub mod ground_station;
pub mod heartbeat;
pub mod log_push;
pub mod loops;
pub mod mqtt;
pub mod pairing;
pub mod tls;

pub use atlas_bearer::CloudBearer;
pub use config::CloudConfig;
pub use dispatch::{CommandResult, CommandStatus};
pub use ground_station::{CloudRelayBridge, GsHeartbeat, ThrottleState, UplinkSnapshot};
pub use heartbeat::{HeartbeatPayload, RadioBlock, RemoteAccess, ServiceEntry};
pub use log_push::{spawn_log_push_watcher, PushRequest, PushResult};
pub use mqtt::{
    BoundedPublishQueue, MavlinkMqttRelay, MqttGateway, MqttQos, MqttTransport, RumqttcTransport,
    WebrtcSignalingRelay,
};
pub use pairing::PairingState;
