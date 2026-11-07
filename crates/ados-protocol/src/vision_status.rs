//! The vision engine's work sidecar: proof that the engine is consuming frames.
//!
//! `ados-vision` takes every frame over a Unix socket (the video pipeline's
//! tap) and hands it on over shared memory and more Unix sockets. None of that
//! moves the process's `/proc/<pid>/io` `rchar`/`wchar` counters, which follow
//! only the `read(2)`/`write(2)` file path, so a fully fed engine reads as
//! flat there. The engine instead publishes the cumulative count of frames it
//! has consumed, plus how many of its inputs are delivering right now, and the
//! supervisor judges it on that count.
//!
//! The count only means something while an input is delivering: an engine
//! whose camera is absent, or whose tap has gone silent, is idle, not wedged.
//! [`VisionStatus::work_counter`] therefore answers only while at least one
//! input is live. A source that delivers nothing for a while is closed by the
//! engine and stops counting as live, so a silent upstream never ages into a
//! stall verdict; an engine wedged anywhere else keeps its inputs live with a
//! frozen count, which is exactly a stall.
//!
//! The sidecar also carries the detector's demand state ([`DetectionDemandStatus`]),
//! which `GET /api/vision/status` serves as its `detection` block.
//!
//! Producer: `ados-vision`, rewritten about once a second under its socket
//! directory. Consumers: the supervisor's work proof and the vision status route.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// The sidecar's file name under the run directory (`/run/ados` by default).
pub const VISION_STATUS_FILE: &str = "vision-status.json";

/// The sidecar schema version, stamped on write.
pub const VISION_STATUS_VERSION: u16 = 1;

/// The last failure of one camera's frame source (open, format negotiation or
/// stream end), kept until the source next delivers a frame.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceError {
    pub camera: String,
    pub error: String,
}

/// The engine's work state.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct VisionStatus {
    #[serde(default)]
    pub version: u16,
    /// Frames consumed from every input since the engine started.
    pub frames_consumed: u64,
    /// Inputs that have delivered a frame since they were last (re)opened and
    /// have not since failed or gone silent.
    pub live_inputs: u32,
    /// Captured buffers the driver flagged as corrupt and the engine dropped.
    #[serde(default)]
    pub error_frames: u64,
    /// Cameras whose source is currently failing, with the reason.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub source_errors: Vec<SourceError>,
    /// Where the detector runs, and which consumer leases keep it running.
    /// Absent from an engine that predates the demand gate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detection: Option<DetectionDemandStatus>,
    /// The depth model's state and, per camera, whether depth and surfaces
    /// run there. Absent from an engine that predates the depth layer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub depth: Option<DepthStatus>,
}

/// The detector's demand state: the mode and, per captured camera, whether
/// detection runs on it and the leases that hold it on.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DetectionDemandStatus {
    /// `on_demand` (run only under a lease) or `always` (every camera).
    pub mode: String,
    pub cameras: BTreeMap<String, CameraDemandStatus>,
}

/// One camera's demand state.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CameraDemandStatus {
    pub detecting: bool,
    #[serde(default)]
    pub leases: Vec<DemandLeaseStatus>,
}

/// One consumer's live lease on a camera.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DemandLeaseStatus {
    pub owner: String,
    /// Time left on the lease.
    pub remaining_ms: u64,
}

/// The depth model's state: whether it can run, at what pace, and per camera
/// what runs and how the last grid was scaled.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DepthStatus {
    /// `ready` (loaded), `no_model` (no depth model configured or installed)
    /// or `unavailable` (configured, but it did not load or its last run
    /// failed).
    pub state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_id: Option<String>,
    /// Why the state is not `ready`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// The shortest gap between two depth runs on one camera.
    pub interval_ms: u64,
    pub cameras: BTreeMap<String, CameraDepthStatus>,
}

/// One camera's depth state.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CameraDepthStatus {
    /// A depth lease is live: the grid is published.
    pub depth: bool,
    /// A surfaces lease is live: landing zones are published.
    pub surfaces: bool,
    /// Live leases, `owner` prefixed with the kind (`depth:operator`).
    #[serde(default)]
    pub leases: Vec<DemandLeaseStatus>,
    /// `lidar` or `none` for the last grid; absent before the first.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scale_source: Option<String>,
    /// Grids computed on this camera since the engine started.
    #[serde(default)]
    pub runs: u64,
}

impl VisionStatus {
    /// The work counter to judge the engine on: the frames consumed, but only
    /// while an input is delivering. `None` (no verdict) otherwise.
    pub fn work_counter(&self) -> Option<u64> {
        (self.live_inputs > 0).then_some(self.frames_consumed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_count_is_a_verdict_only_while_an_input_is_delivering() {
        let fed = VisionStatus {
            version: VISION_STATUS_VERSION,
            frames_consumed: 42,
            live_inputs: 1,
            ..Default::default()
        };
        assert_eq!(fed.work_counter(), Some(42));
        // No camera, or a silent tap: idle, never a stall.
        let idle = VisionStatus {
            live_inputs: 0,
            ..fed.clone()
        };
        assert_eq!(idle.work_counter(), None);
    }

    #[test]
    fn the_wire_shape_round_trips_and_tolerates_a_missing_version() {
        let s = VisionStatus {
            version: VISION_STATUS_VERSION,
            frames_consumed: 7,
            live_inputs: 2,
            error_frames: 1,
            source_errors: vec![SourceError {
                camera: "cam_fl".into(),
                error: "device busy".into(),
            }],
            detection: Some(DetectionDemandStatus {
                mode: "on_demand".into(),
                cameras: BTreeMap::from([(
                    "belly".into(),
                    CameraDemandStatus {
                        detecting: true,
                        leases: vec![DemandLeaseStatus {
                            owner: "operator".into(),
                            remaining_ms: 4_000,
                        }],
                    },
                )]),
            }),
            depth: Some(DepthStatus {
                state: "ready".into(),
                model_id: Some("depth_midas_v21_small".into()),
                reason: None,
                interval_ms: 500,
                cameras: BTreeMap::from([(
                    "main".into(),
                    CameraDepthStatus {
                        depth: true,
                        surfaces: false,
                        leases: vec![DemandLeaseStatus {
                            owner: "depth:operator".into(),
                            remaining_ms: 5_000,
                        }],
                        scale_source: Some("lidar".into()),
                        runs: 12,
                    },
                )]),
            }),
        };
        let text = serde_json::to_string(&s).unwrap();
        assert_eq!(serde_json::from_str::<VisionStatus>(&text).unwrap(), s);
        let old: VisionStatus =
            serde_json::from_str(r#"{"frames_consumed":3,"live_inputs":1}"#).unwrap();
        assert_eq!(old.version, 0);
        assert_eq!(old.work_counter(), Some(3));
    }
}
