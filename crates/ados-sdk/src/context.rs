//! Plugin-facing facade over the IPC client.
//!
//! Ports `ados.plugins.ipc.context`. [`PluginContext`] is the object handed to
//! every lifecycle hook. Each field is a thin capability-gated facade backed by
//! one [`PluginIpcClient`] call: the facade shapes arguments and decodes the
//! response; the host enforces capabilities. The facades share one client
//! behind an `Arc` so the context can be passed by reference into hooks while
//! the client owns the single connection.

use std::collections::BTreeMap;
use std::sync::Arc;

use ados_protocol::camera_control::{CameraControlRequest, CameraControlResult};
use rmpv::Value;

use crate::client::{ClientError, EventCallback, PluginIpcClient};
use crate::vision::VisionClient;

mod flight;
mod hardware;
mod media;
mod network;
mod runtime;

#[cfg(test)]
use flight::sticks_to_channels;
pub use media::{SensorsClient, VideoClient};
pub use network::{CloudClient, MdnsClient, NodeClient};

/// `ctx.events` — the event bus facade.
#[derive(Clone)]
pub struct EventsClient {
    ipc: Arc<PluginIpcClient>,
}

/// `ctx.mavlink` — read and write through the host's MAVLink router.
#[derive(Clone)]
pub struct MavlinkClient {
    ipc: Arc<PluginIpcClient>,
}

/// `ctx.msp` — read and write raw MSP to a Betaflight / iNav / KISS FC through
/// the host's MSP byte plane. The sibling of [`MavlinkClient`] for an FC that
/// speaks MSP; the codec that builds/parses the bytes is
/// [`ados_protocol::msp`].
#[derive(Clone)]
pub struct MspClient {
    ipc: Arc<PluginIpcClient>,
}

/// `ctx.telemetry` — read the vehicle state and extend the heartbeat schema.
#[derive(Clone)]
pub struct TelemetryClient {
    ipc: Arc<PluginIpcClient>,
}

/// `ctx.peripheral_manager` — register driver instances and claim cameras.
///
/// A driver is registered by an opaque reference id; the driver itself keeps
/// running in the plugin process. The host records the claim and routes the
/// driver kind through its registry. This mirrors `_driver_ref`: the host never
/// sees the live driver object. Each kind needs the catalog cap from
/// [`crate::client::driver_kind_capability`]. A host with no consumer for
/// registered drivers answers `not_implemented`, surfaced as `ClientError::Rpc`.
#[derive(Clone)]
pub struct PeripheralClient {
    ipc: Arc<PluginIpcClient>,
}

/// `ctx.camera` — path-level claim/release, a frame-pull primitive, and the
/// exposure and gain of cameras the vision engine owns.
#[derive(Clone)]
pub struct CameraClient {
    ipc: Arc<PluginIpcClient>,
}

/// `ctx.config` — live config kv plus the manifest-supplied static config.
///
/// `ctx.gpio` — host GPIO output.
///
/// Drive a status line or play a bounded buzzer pattern. The gpio service owns
/// the safe bounds, so a plugin requests a pattern and the service clamps it —
/// there is deliberately no client-side clamp to drift from the real ceiling.
#[derive(Clone)]
pub struct GpioClient {
    ipc: Arc<PluginIpcClient>,
}

/// `ctx.display` — the reserved data-driven display page.
///
/// A plugin owns one page of title, `(label, value)` rows and touch zones
/// without recompiling the display service; the host renders it from the sidecar
/// this writes.
#[derive(Clone)]
pub struct DisplayClient {
    ipc: Arc<PluginIpcClient>,
}

/// `ctx.radio` — the additive auxiliary application stream on the link.
///
/// Opens a plugin-owned lane alongside the existing telemetry/video planes,
/// gated so it only exists while the plugin is active.
#[derive(Clone)]
pub struct RadioClient {
    ipc: Arc<PluginIpcClient>,
}

/// `ctx.buttons` — front-panel presses.
///
/// The on-device input surface for a plugin whose operator has no screen and no
/// ground station. The host owns the bus and the short/long decode, so a plugin
/// never re-implements debounce or the action mapping and cannot drift from what
/// the panel's own UI thinks a press means.
///
/// Read-only and non-exclusive: several consumers watch the same bus, so
/// subscribing observes presses without consuming or remapping them.
#[derive(Clone)]
pub struct ButtonClient {
    ipc: Arc<PluginIpcClient>,
}

/// `ctx.tools` — the plugin's MCP tools.
///
/// The host routes an MCP `tools/call` for one of the plugin's declared tools
/// to the live plugin as a `tool.invoke` request (gated on `mcp.expose`); the
/// registered handler runs it and its result is the reply.
#[derive(Clone)]
pub struct ToolsClient {
    ipc: Arc<PluginIpcClient>,
}

/// `ctx.flight` — the scoped guided-setpoint sender.
///
/// A flight-behaviour plugin commands the vehicle through this rather than raw
/// MAVLink writes, so the host gates the whole flight-command surface with one
/// capability. Single-shot by design: the host owns no flight mode and no
/// schedule, so a caller holding a velocity must re-send above the autopilot's
/// setpoint timeout or the vehicle brakes.
#[derive(Clone)]
pub struct FlightClient {
    ipc: Arc<PluginIpcClient>,
}

/// The static config is the manifest dict read at runner start; `get`/`set`
/// reach the host's live kv. Read order on the host side is drone scope (when
/// bound) -> global -> default, mirroring `_ConfigClient`.
#[derive(Clone)]
pub struct ConfigClient {
    ipc: Arc<PluginIpcClient>,
    static_config: Arc<BTreeMap<String, Value>>,
}

/// `ctx.process` — sandboxed vendor-binary spawn authorization.
#[derive(Clone)]
pub struct ProcessClient {
    ipc: Arc<PluginIpcClient>,
}

/// `ctx.lifecycle` — subscribe to GCS-side mount events.
///
/// `on_pause` fires when the operator switches away from the drone hosting this
/// plugin's UI; `on_resume` fires on switch-back. Both ride the plugin's own
/// `plugin.<id>.lifecycle.*` namespace, mirroring `_LifecycleClient`.
#[derive(Clone)]
pub struct LifecycleClient {
    ipc: Arc<PluginIpcClient>,
}

/// The object handed to every lifecycle hook. Every host-facing surface is a
/// capability-gated facade; the IPC client is an implementation detail.
///
/// Ports `ados.plugins.ipc.context.PluginContext`. The `peripherals` field is
/// an alias for `peripheral_manager`, matching the Python back-compat alias.
#[derive(Clone)]
pub struct PluginContext {
    pub plugin_id: String,
    pub plugin_version: String,
    pub agent_id: String,
    /// The plugin's per-drone data directory, when the host set one on the unit.
    /// `None` on a host that did not — a plugin must handle its absence rather
    /// than assume a path.
    pub data_dir: Option<std::path::PathBuf>,
    pub events: EventsClient,
    pub mavlink: MavlinkClient,
    /// `ctx.msp` — raw MSP read/write for a Betaflight / iNav / KISS FC.
    pub msp: MspClient,
    pub telemetry: TelemetryClient,
    pub peripheral_manager: PeripheralClient,
    /// Alias for `peripheral_manager` (Python `ctx.peripherals`).
    pub peripherals: PeripheralClient,
    pub camera: CameraClient,
    /// `ctx.vision` — subscribe to engine frames, register models, run
    /// inference, publish detections, and inject visual-odometry pose.
    pub vision: VisionClient,
    pub config: ConfigClient,
    /// Front-panel button presses; quiet on a board with no panel.
    pub buttons: ButtonClient,
    /// The scoped guided-setpoint sender.
    pub flight: FlightClient,
    /// Host GPIO output (status line, buzzer).
    pub gpio: GpioClient,
    /// The reserved data-driven display page.
    pub display: DisplayClient,
    /// The additive auxiliary radio stream.
    pub radio: RadioClient,
    /// The plugin's cloud stream and cloud records.
    pub cloud: CloudClient,
    /// Facts about this node: profile, board, ground-station role, camera.
    pub node: NodeClient,
    /// Local-network service discovery through the host's mDNS responder.
    pub mdns: MdnsClient,
    /// WebRTC playback sessions on the agent's camera legs.
    pub video: VideoClient,
    /// The sensor rig document.
    pub sensors: SensorsClient,
    pub process: ProcessClient,
    pub lifecycle: LifecycleClient,
    /// The plugin's MCP tool handler registration.
    pub tools: ToolsClient,
    ipc: Arc<PluginIpcClient>,
}

#[cfg(test)]
mod tests;
