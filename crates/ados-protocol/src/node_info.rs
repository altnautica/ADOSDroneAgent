//! The `node.info` plugin method: the node facts a plugin may read without
//! reaching into the agent's sidecars, which its sandbox hides.
//!
//! The host answers from the same sources the agent's own services decide on
//! (the node profile, the HAL board sidecar, the ground-station role sentinel,
//! the camera-state sidecar and the video config). A source that is absent maps
//! to `null` on the wire and `None` here; the host never fills a gap with a
//! guess. The types are the wire shape: the host serializes them and the SDK
//! deserializes them, so the two cannot drift.

use serde::{Deserialize, Serialize};

/// The wire method name.
pub const METHOD: &str = "node.info";

/// The capability `node.info` is gated on.
pub const CAPABILITY: &str = "node.info.read";

/// Everything `node.info` answers.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NodeInfo {
    /// The node profile in wire form: `drone`, `ground-station`, `workstation`
    /// or `compute`.
    pub profile: String,
    /// The board, or `None` when the board sidecar is absent or unreadable (a
    /// node that has not probed yet).
    pub board: Option<BoardInfo>,
    /// The ground-station facts. `role` is `None` on every profile other than
    /// `ground-station`.
    pub ground_station: GroundStationInfo,
    /// The camera facts.
    pub camera: CameraInfo,
}

/// The board the node runs on.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BoardInfo {
    /// The board identity manifests' `compatibility.supported_boards` match
    /// against (the board sidecar's `name`).
    pub id: String,
    /// The hardware model string the board reports (the sidecar's `model`).
    pub name: String,
    /// The board declares a neural accelerator (`npu_tops > 0`). The same rule
    /// the offload decision and the status surfaces' `hasAccelerator` key on.
    pub has_npu: bool,
    /// The inference accelerators the board declares: `npu` when it has one,
    /// and `cpu-<runtime>` (e.g. `cpu-onnx`) when it declares CPU inference.
    /// Empty when it declares neither.
    pub accelerators: Vec<String>,
}

/// The ground-station facts.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GroundStationInfo {
    /// `direct`, `relay` or `receiver` on a ground station (a ground station
    /// with no role sentinel runs the direct plane, so reads `direct`); `None`
    /// on any other profile.
    pub role: Option<String>,
}

/// The camera facts.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CameraInfo {
    /// The video pipeline reports a discovered camera whose pipeline has not
    /// failed, and has re-stamped that report within the live window. `false`
    /// when the camera-state sidecar is absent, stale, or says otherwise.
    pub ready: bool,
    /// The geometry the primary encoder (the `main` stream) runs at, or `None`
    /// when the node has no config file or is a ground station (no onboard
    /// camera).
    pub main: Option<StreamGeometry>,
}

/// One stream's geometry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct StreamGeometry {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
}
