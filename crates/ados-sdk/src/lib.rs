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
//!   `telemetry`, `peripheral_manager`, `camera`, `config`, `process`, and
//!   `lifecycle` sub-clients. Ports `ados.plugins.ipc.context`.
//! - [`drivers`] — the hardware driver traits (`CameraDriver`, `GimbalDriver`,
//!   `LidarDriver`, `GpsDriver`, `EscDriver`, `PayloadActuatorDriver`) and
//!   their candidate / capability / sample types. Ports `ados.sdk.drivers`.
//! - [`Plugin`] + [`run_plugin`] — the lifecycle hook trait and the runner
//!   entry that reads `--socket` / `--token` / `--agent-id` off argv and env,
//!   connects, and drives `on_install` .. `on_disable`. Ports
//!   `ados.plugins.runner` for the `runtime: rust` case.
//!
//! The agent capability catalog is re-exported from `ados-protocol` as
//! [`capabilities`]; the SDK does not maintain its own copy.

pub mod client;
pub mod context;
pub mod drivers;
pub mod lifecycle;

pub use client::{ClientError, EventCallback, PluginIpcClient};
pub use context::{
    CameraClient, ConfigClient, EventsClient, LifecycleClient, MavlinkClient, PeripheralClient,
    PluginContext, ProcessClient, TelemetryClient,
};
pub use lifecycle::{run_plugin, run_plugin_with, Plugin, RunnerArgs, RunnerError};

/// The generated agent capability catalog, re-exported from `ados-protocol` so
/// a plugin author references one source of truth for capability ids. The
/// source of truth is `crates/ados-protocol/capabilities.toml`.
pub mod capabilities {
    pub use ados_protocol::capabilities::*;
}
