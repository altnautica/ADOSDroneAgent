//! IPC client seams the route surface reads + writes.
//!
//! The status and telemetry routes do not hold vehicle state themselves; the
//! MAVLink service owns it and publishes a snapshot on `/run/ados/state.sock`.
//! [`state_client`] is the read side of that seam: it connects, decodes the
//! self-describing snapshot frame (newline JSON or length-prefixed msgpack), and
//! holds the latest snapshot for a route to project. It only ever reads, and a
//! missing socket is normal (an idle or unpaired agent before the state hub is
//! up), so the routes degrade to an empty snapshot rather than fail.
//!
//! [`mavlink_client`] is the write side of the command seam: the command route
//! builds a MAVLink frame and writes it length-prefixed to
//! `/run/ados/mavlink.sock`, which the router forwards to the FC. An absent
//! socket returns an error the route maps to a 503 (no FC link), so a command is
//! never silently dropped.
//!
//! [`logd_client`] is the read side of the hardware-store seam: the status route
//! reads the most-recent CPU / memory / disk / temperature snapshots back from
//! the logging store's query socket (the continuous collector samples them), so
//! the surface never probes the host itself. A missing store degrades the route
//! to its zero-valued health default rather than failing it.

pub mod atlas_control_client;
pub mod logd_client;
pub mod mavlink_client;
pub mod plugin_control_client;
pub mod state_client;
pub mod vision_client;

pub use atlas_control_client::{AtlasControlClient, AtlasControlError};
pub use logd_client::LogdQueryClient;
pub use mavlink_client::MavlinkIpcClient;
pub use plugin_control_client::{PluginControlClient, PluginControlError};
pub use state_client::StateIpcClient;
pub use vision_client::VisionIpcClient;
