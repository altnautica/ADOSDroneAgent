//! ADOS Atlas world-model wire contract.
//!
//! The topic names and wire structs for the world-model program: a drone
//! captures pose-tagged keyframes, a compute node reconstructs a 3D world model
//! (splat / cloud / mesh / occupancy), and any plugin can consume the result as
//! shared data. Two topic roots:
//!
//! - `atlas.*` is the agent's own service namespace (capture state, the
//!   offloaded-pose return leg).
//! - `plugin.atlas.*` is the shared-data namespace any plugin subscribes to
//!   (pose / pointcloud / occupancy / splat / mesh descriptors).
//!
//! Heavy payloads ride the shared-memory ring (see [`crate::framebus`]) or the
//! stream lane; these topics carry small descriptors only. The envelope is
//! transport-agnostic: the identical struct travels on any bearer (direct LAN,
//! the WFB relay, or the cloud relay), so no transport strings are baked in. It
//! is also tier-aware: a light descriptor (pose plus a thumbnail or an occupancy
//! slice) is small enough for an in-flight relay link, while a full keyframe
//! (full-resolution image bytes plus the IMU window) is a LAN-bulk artifact.

use serde::{Deserialize, Serialize};

// --- Topics ---------------------------------------------------------------

/// Capture-session state (state, keyframe counts, VIO health). Host-published.
pub const ATLAS_CAPTURE_STATE_TOPIC: &str = "atlas.capture.state";

/// The compute node returns an offloaded pose to the drone on this topic. The
/// drone streamed an image to the node, the node ran SLAM, and the pose comes
/// back here for the drone to stamp into the keyframe envelope. This is the
/// localization return leg for NPU-less boards.
pub const ATLAS_POSE_OFFLOAD_TOPIC: &str = "atlas.pose.offload";

/// Shared-data: current 6-DoF pose plus world anchor (~10 Hz).
pub const PLUGIN_ATLAS_POSE_TOPIC: &str = "plugin.atlas.pose";

/// Shared-data: point-cloud descriptor (count, bounds, shm handle / url).
pub const PLUGIN_ATLAS_POINTCLOUD_TOPIC: &str = "plugin.atlas.pointcloud";

/// Shared-data: occupancy-grid descriptor (origin, resolution, dims, handle).
pub const PLUGIN_ATLAS_OCCUPANCY_TOPIC: &str = "plugin.atlas.occupancy";

/// Shared-data: "splat updated" descriptor (gaussian count, url / handle).
pub const PLUGIN_ATLAS_SPLAT_TOPIC: &str = "plugin.atlas.splat";

/// Shared-data: mesh descriptor (vertex / face count, handle / url).
pub const PLUGIN_ATLAS_MESH_TOPIC: &str = "plugin.atlas.mesh";

// --- Enums ----------------------------------------------------------------

/// Which camera on the rig produced a keyframe. Camera count is configurable
/// from one camera to an all-sides rig; the role tags each stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CameraRole {
    Primary,
    Aux,
    Down,
    Left,
    Right,
    Back,
    Up,
}

/// Delivery tier of a keyframe. A `Light` descriptor fits an in-flight relay
/// link (a thumbnail or an occupancy slice); a `Full` keyframe carries the
/// full-resolution image and IMU window for a LAN-bulk or post-flight pull.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum KeyframeTier {
    Light,
    Full,
}

/// Where a keyframe's pose came from. Both produce the identical envelope, so
/// nothing downstream forks; only the producer changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PoseSource {
    /// Computed on-board by the drone's own VIO (a VIO-capable board).
    LocalVio,
    /// Computed on the compute node from a streamed image and returned on
    /// [`ATLAS_POSE_OFFLOAD_TOPIC`] (an NPU-less board, first-class).
    OffloadedSlam,
}

/// Image encoding carried in a full keyframe.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ImageEncoding {
    Jpeg,
    /// HEVC keyframe (I-frame) bytes.
    HevcKeyframe,
}

// --- Structs --------------------------------------------------------------

/// Camera intrinsics for one `camera_id`. `k` is the 3x3 pinhole matrix in
/// row-major order; `distortion` names the model and its parameters.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CameraIntrinsics {
    /// Row-major 3x3 intrinsic matrix K.
    pub k: [f64; 9],
    pub distortion: Distortion,
}

/// A lens-distortion model name plus its parameters (e.g. `radtan`, `equidist`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Distortion {
    pub model: String,
    pub params: Vec<f64>,
}

/// A 6-DoF pose. `r` is a row-major 3x3 rotation (world-from-camera), `t` the
/// translation, `cov` an optional 6x6 covariance (36 row-major elements).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Pose {
    pub r: [f64; 9],
    pub t: [f64; 3],
    pub cov: Option<Vec<f64>>,
}

/// A geo anchor stamped on the first keyframe of a session so the local world
/// frame can be georeferenced.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct GlobalAnchor {
    pub lat: f64,
    pub lon: f64,
    pub alt_m: f64,
    pub yaw_rad: f64,
}

/// One IMU sample in a keyframe's pre-integration window.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ImuSample {
    pub t_ms: i64,
    pub gyro: [f64; 3],
    pub accel: [f64; 3],
}

/// Image bytes carried in a keyframe. For a `Light` tier this may be a
/// thumbnail; for a `Full` tier it is the full-resolution frame.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KeyframeImage {
    pub encoding: ImageEncoding,
    pub width: u32,
    pub height: u32,
    pub bytes: Vec<u8>,
}

/// Per-keyframe boolean flags.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct KeyframeFlags {
    pub is_loop_closure: bool,
    pub is_session_start: bool,
    pub is_session_end: bool,
}

/// A pose-tagged keyframe sent drone-to-compute. Extends the splat-capture
/// envelope with the camera identity so multi-camera rigs are unambiguous, and
/// with the tier and pose-source so the same struct serves the LAN-bulk and the
/// in-flight-relay paths and the VIO-vs-offloaded-pose producers.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KeyframeEnvelope {
    pub session_id: String,
    pub kf_id: u64,
    pub ts_unix_ms: i64,
    pub camera_id: String,
    pub camera_role: CameraRole,
    pub tier: KeyframeTier,
    pub image: KeyframeImage,
    pub camera: CameraIntrinsics,
    pub pose: Pose,
    pub pose_source: PoseSource,
    pub global_anchor: Option<GlobalAnchor>,
    pub imu_window: Vec<ImuSample>,
    pub flags: KeyframeFlags,
}

/// The pose the compute node returns to the drone on
/// [`ATLAS_POSE_OFFLOAD_TOPIC`] after running SLAM on a streamed image.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OffloadedPose {
    pub session_id: String,
    pub kf_id: u64,
    pub camera_id: String,
    pub pose: Pose,
    /// Always [`PoseSource::OffloadedSlam`] on this lane; carried for symmetry.
    pub source: PoseSource,
    pub ts_ms: i64,
}

/// Capture-session lifecycle state published on [`ATLAS_CAPTURE_STATE_TOPIC`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CaptureState {
    Idle,
    Capturing,
    Paused,
    Finalizing,
    Bagged,
}

/// SLAM / VIO health summary carried with the capture state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VioHealth {
    Good,
    Degraded,
    Lost,
}

/// The descriptor on [`ATLAS_CAPTURE_STATE_TOPIC`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CaptureStatus {
    pub session_id: String,
    pub state: CaptureState,
    pub keyframes: u64,
    pub vio_health: VioHealth,
    /// Count of enabled cameras (1 to N); the fusion layer keys off this.
    pub camera_count: u32,
    pub ingest_rate_hz: f32,
}

/// Shared-data descriptor on [`PLUGIN_ATLAS_POSE_TOPIC`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PoseDescriptor {
    pub pose: Pose,
    pub anchor: Option<GlobalAnchor>,
    pub ts_ms: i64,
}

/// Shared-data descriptor on [`PLUGIN_ATLAS_POINTCLOUD_TOPIC`]. The heavy buffer
/// rides the shm ring (`shm_name`/`slot`/`seq`, see [`crate::framebus`]) or a
/// stream-lane `url`; this carries the summary only.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PointCloudDescriptor {
    pub point_count: u64,
    /// Axis-aligned bounds: `[min_x, min_y, min_z, max_x, max_y, max_z]`.
    pub bounds: [f64; 6],
    pub shm_name: Option<String>,
    pub slot: Option<u32>,
    pub seq: Option<u64>,
    pub url: Option<String>,
}

/// Shared-data descriptor on [`PLUGIN_ATLAS_OCCUPANCY_TOPIC`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OccupancyDescriptor {
    /// World-frame origin of voxel `(0,0,0)`.
    pub origin: [f64; 3],
    pub resolution_m: f32,
    /// Grid dimensions in voxels `[nx, ny, nz]`.
    pub dims: [u32; 3],
    pub shm_name: Option<String>,
    pub slot: Option<u32>,
    pub seq: Option<u64>,
}

/// Shared-data descriptor on [`PLUGIN_ATLAS_SPLAT_TOPIC`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SplatDescriptor {
    pub gaussian_count: u64,
    /// Training step this descriptor reflects (monotonic for live sessions).
    pub step: u64,
    pub url: Option<String>,
    pub handle: Option<String>,
}

/// Shared-data descriptor on [`PLUGIN_ATLAS_MESH_TOPIC`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MeshDescriptor {
    pub vertex_count: u64,
    pub face_count: u64,
    pub url: Option<String>,
    pub handle: Option<String>,
}

macro_rules! impl_msgpack {
    ($($t:ty),+ $(,)?) => {
        $(impl $t {
            /// Encode as a msgpack map with named keys.
            pub fn to_msgpack(&self) -> Result<Vec<u8>, rmp_serde::encode::Error> {
                rmp_serde::to_vec_named(self)
            }
            /// Decode from a msgpack map.
            pub fn from_msgpack(bytes: &[u8]) -> Result<Self, rmp_serde::decode::Error> {
                rmp_serde::from_slice(bytes)
            }
        })+
    };
}

impl_msgpack!(
    KeyframeEnvelope,
    OffloadedPose,
    CaptureStatus,
    PoseDescriptor,
    PointCloudDescriptor,
    OccupancyDescriptor,
    SplatDescriptor,
    MeshDescriptor,
);

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_pose() -> Pose {
        Pose {
            r: [1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0],
            t: [1.5, -2.0, 0.5],
            cov: None,
        }
    }

    fn sample_keyframe() -> KeyframeEnvelope {
        KeyframeEnvelope {
            session_id: "sess-1".into(),
            kf_id: 7,
            ts_unix_ms: 1_700_000_000_000,
            camera_id: "front".into(),
            camera_role: CameraRole::Primary,
            tier: KeyframeTier::Full,
            image: KeyframeImage {
                encoding: ImageEncoding::Jpeg,
                width: 1280,
                height: 720,
                bytes: vec![1, 2, 3, 4],
            },
            camera: CameraIntrinsics {
                k: [900.0, 0.0, 640.0, 0.0, 900.0, 360.0, 0.0, 0.0, 1.0],
                distortion: Distortion {
                    model: "radtan".into(),
                    params: vec![0.0, 0.0, 0.0, 0.0],
                },
            },
            pose: sample_pose(),
            pose_source: PoseSource::LocalVio,
            global_anchor: Some(GlobalAnchor {
                lat: 12.97,
                lon: 77.59,
                alt_m: 920.0,
                yaw_rad: 0.0,
            }),
            imu_window: vec![ImuSample {
                t_ms: 1,
                gyro: [0.0, 0.0, 0.0],
                accel: [0.0, 0.0, 9.81],
            }],
            flags: KeyframeFlags {
                is_session_start: true,
                ..KeyframeFlags::default()
            },
        }
    }

    #[test]
    fn keyframe_envelope_round_trips() {
        let kf = sample_keyframe();
        let bytes = kf.to_msgpack().expect("encode");
        let back = KeyframeEnvelope::from_msgpack(&bytes).expect("decode");
        assert_eq!(kf, back);
        assert_eq!(back.camera_role, CameraRole::Primary);
        assert_eq!(back.tier, KeyframeTier::Full);
        assert_eq!(back.pose_source, PoseSource::LocalVio);
    }

    #[test]
    fn offloaded_pose_round_trips() {
        let op = OffloadedPose {
            session_id: "sess-1".into(),
            kf_id: 7,
            camera_id: "front".into(),
            pose: sample_pose(),
            source: PoseSource::OffloadedSlam,
            ts_ms: 1_700_000_000_000,
        };
        let bytes = op.to_msgpack().expect("encode");
        let back = OffloadedPose::from_msgpack(&bytes).expect("decode");
        assert_eq!(op, back);
        assert_eq!(back.source, PoseSource::OffloadedSlam);
    }

    #[test]
    fn world_model_descriptors_round_trip() {
        let status = CaptureStatus {
            session_id: "sess-1".into(),
            state: CaptureState::Capturing,
            keyframes: 42,
            vio_health: VioHealth::Good,
            camera_count: 1,
            ingest_rate_hz: 9.5,
        };
        let back = CaptureStatus::from_msgpack(&status.to_msgpack().unwrap()).unwrap();
        assert_eq!(status, back);

        let cloud = PointCloudDescriptor {
            point_count: 10_000,
            bounds: [-1.0, -1.0, -1.0, 1.0, 1.0, 1.0],
            shm_name: Some("atlas-cloud-0".into()),
            slot: Some(2),
            seq: Some(99),
            url: None,
        };
        assert_eq!(
            cloud,
            PointCloudDescriptor::from_msgpack(&cloud.to_msgpack().unwrap()).unwrap()
        );
    }

    #[test]
    fn topic_names_are_stable() {
        assert_eq!(ATLAS_CAPTURE_STATE_TOPIC, "atlas.capture.state");
        assert_eq!(ATLAS_POSE_OFFLOAD_TOPIC, "atlas.pose.offload");
        assert_eq!(PLUGIN_ATLAS_POSE_TOPIC, "plugin.atlas.pose");
        assert_eq!(PLUGIN_ATLAS_POINTCLOUD_TOPIC, "plugin.atlas.pointcloud");
        assert_eq!(PLUGIN_ATLAS_OCCUPANCY_TOPIC, "plugin.atlas.occupancy");
        assert_eq!(PLUGIN_ATLAS_SPLAT_TOPIC, "plugin.atlas.splat");
        assert_eq!(PLUGIN_ATLAS_MESH_TOPIC, "plugin.atlas.mesh");
    }
}
