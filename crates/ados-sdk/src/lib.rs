//! Plugin author SDK for the ADOS Drone Agent.
//!
//! A Rust plugin links this crate to talk to the agent's plugin host over the
//! same wire the Python SDK uses: length-prefixed msgpack [`Envelope`] frames
//! over a per-plugin Unix domain socket, gated by a pipe-delimited HMAC
//! capability token. The wire itself lives in `ados-protocol` and is reused
//! unchanged, so a Rust plugin and the Rust (or Python) host interoperate
//! byte-for-byte.
//!
//! The surface mirrors the Python `ados.sdk` package:
//!
//! - [`PluginIpcClient`] — the async client: connect to the socket, run the
//!   `hello` handshake, send requests keyed by a `r<n>` request id, drain the
//!   reader loop, and dispatch event / MAVLink pushes to topic-matched
//!   callbacks. Ports `ados.plugins.ipc_client`.
//! - [`PluginContext`] — the plugin-facing facade with `events`, `mavlink`,
//!   `telemetry`, `peripheral_manager`, `camera`, `config`, `process`,
//!   `cloud`, and `lifecycle` sub-clients. Ports `ados.plugins.ipc.context`.
//! - [`drivers`] — the hardware driver traits (`CameraDriver`, `GimbalDriver`,
//!   `LidarDriver`, `GpsDriver`, `EscDriver`, `PayloadActuatorDriver`) and
//!   their candidate / capability / sample types. Ports `ados.sdk.drivers`.
//! - [`Plugin`] + [`run_plugin`] — the lifecycle hook trait and the runner
//!   entry that reads `--socket` / `--token` / `--agent-id` off argv and env,
//!   connects, and drives `on_install` .. `on_disable`. Ports
//!   `ados.plugins.runner` for the `runtime: rust` case.
//! - [`http`] — the operator-facing HTTP socket of an `agent.http` plugin: the
//!   host-provided path and a listener bind helper.
//!
//! The agent capability catalog is re-exported from `ados-protocol` as
//! [`capabilities`]; the SDK does not maintain its own copy.

pub mod client;
pub mod context;
pub mod drivers;
pub mod http;
pub mod lifecycle;
pub mod msp;
pub mod testing;
pub mod vision;

pub use client::{ClientError, EventCallback, OffloadAdvertisement, PluginIpcClient};
pub use context::{
    CameraClient, CloudClient, ConfigClient, EventsClient, LifecycleClient, MavlinkClient,
    MdnsClient, NodeClient, PeripheralClient, PluginContext, ProcessClient, TelemetryClient,
};
pub use lifecycle::{run_plugin, run_plugin_with, Plugin, RunnerArgs, RunnerError};
pub use vision::{Frame, FrameCallback, Odometry, Pose, VisionClient, VIO_COMPONENT_ID};

/// The env var the host sets on every plugin unit to this node's profile.
pub const NODE_PROFILE_ENV: &str = "ADOS_NODE_PROFILE";

/// This node's profile (`drone`, `ground-station`, `workstation` or
/// `compute`), as the host exported it into the plugin's environment. `drone`
/// only when the variable is unset or empty, which a host-started plugin never
/// sees.
pub fn node_profile() -> String {
    std::env::var(NODE_PROFILE_ENV)
        .ok()
        .map(|p| p.trim().to_string())
        .filter(|p| !p.is_empty())
        .unwrap_or_else(|| "drone".to_string())
}

/// The generated agent capability catalog, re-exported from `ados-protocol` so
/// a plugin author references one source of truth for capability ids. The
/// source of truth is `crates/ados-protocol/capabilities.toml`.
pub mod capabilities {
    pub use ados_protocol::capabilities::*;
}

/// The `node.info` reply types ([`NodeClient::info`]), re-exported from
/// `ados-protocol`, which the host serializes them from.
pub mod node_info {
    pub use ados_protocol::node_info::{
        BoardInfo, CameraInfo, GroundStationInfo, NodeInfo, StreamGeometry,
    };
}

/// The `mdns.advertise` / `mdns.browse` types ([`MdnsClient`]), re-exported
/// from `ados-protocol`, which the host decodes and serializes them with.
pub mod mdns {
    pub use ados_protocol::plugin_mdns::{Advertised, DiscoveredService, RESERVED_SERVICE_TYPES};
}
