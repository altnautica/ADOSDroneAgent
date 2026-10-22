#![doc = include_str!("../README.md")]

pub mod client;
pub mod context;
pub mod drivers;
pub mod http;
pub mod lifecycle;
pub mod msp;
pub mod testing;
pub mod vision;

/// The `camera.control.set` request and reply ([`CameraClient::set_controls`]).
/// The `reason` codes of a refused request are in
/// [`ados_protocol::camera_control::reason`].
pub use ados_protocol::camera_control::{CameraControlRequest, CameraControlResult};
pub use client::{
    ClientError, EventCallback, OffloadAdvertisement, PluginIpcClient, ToolHandler, WhepAnswer,
};
pub use context::{
    CameraClient, CloudClient, ConfigClient, EventsClient, LifecycleClient, MavlinkClient,
    MdnsClient, NodeClient, PeripheralClient, PluginContext, ProcessClient, SensorsClient,
    TelemetryClient, ToolsClient, VideoClient,
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
