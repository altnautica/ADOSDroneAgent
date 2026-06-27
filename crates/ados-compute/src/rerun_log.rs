//! The Rerun-aligned world-model recording adapter.
//!
//! The world model is logged as Rerun entity paths so the wire contract and the
//! visualizer are one model. The Atlas keyframe envelope is already a Rerun
//! frame (a `Pinhole` for the intrinsics, a `Transform3D` for the pose, an
//! `Image`, and IMU `Scalars`), so this is the thin adapter that maps an
//! envelope + the world-model descriptors onto the entity tree:
//!
//! ```text
//! world/                       the reconstruction (static, grows)
//!   splat | points | mesh | occupancy
//! world/camera/<camera_id>/    one subtree per camera
//!   (pinhole) (transform) rgb
//! world/imu/                   SLAM-health time series
//!   accel/{x,y,z} gyro/{x,y,z}
//! ```
//!
//! This produces the structured log entries the Rerun viewer renders; the
//! viewer embed and the on-disk `.rrd` / MCAP write are the GCS-side
//! visualization layer. Keeping the adapter pure (no Rerun SDK) makes the
//! mapping testable and keeps the compute crate light.

use serde::{Deserialize, Serialize};

use ados_protocol::atlas::{
    ImageEncoding, KeyframeEnvelope, MeshDescriptor, OccupancyDescriptor, PointCloudDescriptor,
    SplatDescriptor,
};

/// A Rerun archetype, the subset the Atlas world model logs. The variant names
/// are the manifest discriminators a viewer maps, so they are the Rerun
/// archetype names verbatim (no `rename_all`, which would mangle `Transform3D`
/// into `transform3_d`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "archetype")]
pub enum RerunArchetype {
    /// Camera intrinsics (row-major 3x3 K) + the image plane size.
    Pinhole {
        image_from_camera: [f64; 9],
        width: u32,
        height: u32,
    },
    /// A 6-DoF pose: row-major 3x3 rotation + translation (world-from-camera).
    Transform3D {
        rotation: [f64; 9],
        translation: [f64; 3],
    },
    /// An encoded RGB image (`jpeg` | `hevc-keyframe`).
    Image {
        width: u32,
        height: u32,
        encoding: String,
    },
    /// One scalar sample on a timeline (an IMU axis).
    Scalar { value: f64 },
    /// A point cloud summary (count + axis-aligned bounds).
    Points3D { count: u64, bounds: [f64; 6] },
    /// A mesh summary.
    Mesh3D { vertex_count: u64, face_count: u64 },
    /// A gaussian-splat slab summary.
    SplatSlab { gaussian_count: u64, step: u64 },
    /// An occupancy voxel grid summary.
    VoxelGrid {
        dims: [u32; 3],
        resolution_m: f32,
        origin: [f64; 3],
    },
}

/// One entry to log: the entity path, the time it is logged at, and the data.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RerunLogEntry {
    pub entity_path: String,
    pub timeline_ms: i64,
    #[serde(flatten)]
    pub archetype: RerunArchetype,
}

fn encoding_str(enc: ImageEncoding) -> &'static str {
    match enc {
        ImageEncoding::Jpeg => "jpeg",
        ImageEncoding::HevcKeyframe => "hevc-keyframe",
    }
}

/// Map one keyframe to its Rerun log entries: the camera's pinhole + pose +
/// image under `world/camera/<id>`, and the IMU window as per-axis scalars under
/// `world/imu/{accel,gyro}`.
pub fn log_keyframe(kf: &KeyframeEnvelope) -> Vec<RerunLogEntry> {
    let cam = format!("world/camera/{}", kf.camera_id);
    let t = kf.ts_unix_ms;
    let mut entries = vec![
        RerunLogEntry {
            entity_path: cam.clone(),
            timeline_ms: t,
            archetype: RerunArchetype::Pinhole {
                image_from_camera: kf.camera.k,
                width: kf.image.width,
                height: kf.image.height,
            },
        },
        RerunLogEntry {
            entity_path: cam.clone(),
            timeline_ms: t,
            archetype: RerunArchetype::Transform3D {
                rotation: kf.pose.r,
                translation: kf.pose.t,
            },
        },
        RerunLogEntry {
            entity_path: format!("{cam}/rgb"),
            timeline_ms: t,
            archetype: RerunArchetype::Image {
                width: kf.image.width,
                height: kf.image.height,
                encoding: encoding_str(kf.image.encoding).into(),
            },
        },
    ];

    for sample in &kf.imu_window {
        for (axis, value) in ["x", "y", "z"].iter().zip(sample.accel) {
            entries.push(RerunLogEntry {
                entity_path: format!("world/imu/accel/{axis}"),
                timeline_ms: sample.t_ms,
                archetype: RerunArchetype::Scalar { value },
            });
        }
        for (axis, value) in ["x", "y", "z"].iter().zip(sample.gyro) {
            entries.push(RerunLogEntry {
                entity_path: format!("world/imu/gyro/{axis}"),
                timeline_ms: sample.t_ms,
                archetype: RerunArchetype::Scalar { value },
            });
        }
    }

    entries
}

/// Log a splat descriptor under `world/splat`.
pub fn log_splat(desc: &SplatDescriptor, timeline_ms: i64) -> RerunLogEntry {
    RerunLogEntry {
        entity_path: "world/splat".into(),
        timeline_ms,
        archetype: RerunArchetype::SplatSlab {
            gaussian_count: desc.gaussian_count,
            step: desc.step,
        },
    }
}

/// Log a point-cloud descriptor under `world/points`.
pub fn log_pointcloud(desc: &PointCloudDescriptor, timeline_ms: i64) -> RerunLogEntry {
    RerunLogEntry {
        entity_path: "world/points".into(),
        timeline_ms,
        archetype: RerunArchetype::Points3D {
            count: desc.point_count,
            bounds: desc.bounds,
        },
    }
}

/// Log a mesh descriptor under `world/mesh`.
pub fn log_mesh(desc: &MeshDescriptor, timeline_ms: i64) -> RerunLogEntry {
    RerunLogEntry {
        entity_path: "world/mesh".into(),
        timeline_ms,
        archetype: RerunArchetype::Mesh3D {
            vertex_count: desc.vertex_count,
            face_count: desc.face_count,
        },
    }
}

/// Log an occupancy descriptor under `world/occupancy`.
pub fn log_occupancy(desc: &OccupancyDescriptor, timeline_ms: i64) -> RerunLogEntry {
    RerunLogEntry {
        entity_path: "world/occupancy".into(),
        timeline_ms,
        archetype: RerunArchetype::VoxelGrid {
            dims: desc.dims,
            resolution_m: desc.resolution_m,
            origin: desc.origin,
        },
    }
}

/// An accumulating Rerun recording: the ordered log entries that, fed to the
/// Rerun viewer, render the building world. Serializes to a JSON manifest the
/// GCS viewer (or an MCAP writer) consumes.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RerunRecording {
    pub entries: Vec<RerunLogEntry>,
    /// Cameras whose static Pinhole intrinsics have already been logged, so each
    /// camera's pinhole is logged once (internal state, not serialized).
    #[serde(skip)]
    logged_cameras: std::collections::HashSet<String>,
}

impl RerunRecording {
    pub fn new() -> Self {
        Self::default()
    }

    /// Append a keyframe's entries. The camera's static Pinhole intrinsics are
    /// logged only on the first keyframe from that camera (per the world-model
    /// schema, "logged once"); the per-frame Transform / Image / IMU entries are
    /// always appended.
    pub fn push_keyframe(&mut self, kf: &KeyframeEnvelope) {
        let first_for_camera = self.logged_cameras.insert(kf.camera_id.clone());
        for entry in log_keyframe(kf) {
            if !first_for_camera && matches!(entry.archetype, RerunArchetype::Pinhole { .. }) {
                continue;
            }
            self.entries.push(entry);
        }
    }

    /// Append a splat-descriptor entry.
    pub fn push_splat(&mut self, desc: &SplatDescriptor, timeline_ms: i64) {
        self.entries.push(log_splat(desc, timeline_ms));
    }

    /// Append a point-cloud entry.
    pub fn push_pointcloud(&mut self, desc: &PointCloudDescriptor, timeline_ms: i64) {
        self.entries.push(log_pointcloud(desc, timeline_ms));
    }

    /// Append a mesh entry.
    pub fn push_mesh(&mut self, desc: &MeshDescriptor, timeline_ms: i64) {
        self.entries.push(log_mesh(desc, timeline_ms));
    }

    /// Append an occupancy entry.
    pub fn push_occupancy(&mut self, desc: &OccupancyDescriptor, timeline_ms: i64) {
        self.entries.push(log_occupancy(desc, timeline_ms));
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Serialize the recording to a JSON manifest.
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ados_protocol::atlas::{
        CameraIntrinsics, CameraRole, Distortion, ImuSample, KeyframeFlags, KeyframeImage,
        KeyframeTier, Pose, PoseSource,
    };

    fn keyframe() -> KeyframeEnvelope {
        KeyframeEnvelope {
            session_id: "s".into(),
            kf_id: 1,
            ts_unix_ms: 1000,
            camera_id: "front".into(),
            camera_role: CameraRole::Primary,
            tier: KeyframeTier::Full,
            image: KeyframeImage {
                encoding: ImageEncoding::Jpeg,
                width: 1280,
                height: 720,
                bytes: vec![],
            },
            camera: CameraIntrinsics {
                k: [900.0, 0.0, 640.0, 0.0, 900.0, 360.0, 0.0, 0.0, 1.0],
                distortion: Distortion {
                    model: "radtan".into(),
                    params: vec![],
                },
            },
            pose: Pose {
                r: [1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0],
                t: [1.0, 2.0, 3.0],
                cov: None,
            },
            pose_source: PoseSource::LocalVio,
            global_anchor: None,
            imu_window: vec![ImuSample {
                t_ms: 999,
                gyro: [0.1, 0.2, 0.3],
                accel: [0.0, 0.0, 9.81],
            }],
            flags: KeyframeFlags::default(),
        }
    }

    #[test]
    fn keyframe_maps_to_camera_subtree_and_imu_scalars() {
        let entries = log_keyframe(&keyframe());
        let paths: Vec<&str> = entries.iter().map(|e| e.entity_path.as_str()).collect();
        // The camera subtree: pinhole + transform on the camera path, image on rgb.
        assert!(paths.contains(&"world/camera/front"));
        assert!(paths.contains(&"world/camera/front/rgb"));
        // Per-axis IMU scalars.
        assert!(paths.contains(&"world/imu/accel/z"));
        assert!(paths.contains(&"world/imu/gyro/x"));

        // 3 camera entries + 6 IMU scalars (3 accel + 3 gyro) for one sample.
        assert_eq!(entries.len(), 3 + 6);

        // The pinhole carries the intrinsics K, the transform the pose.
        let pinhole = entries
            .iter()
            .find(|e| matches!(e.archetype, RerunArchetype::Pinhole { .. }))
            .unwrap();
        match &pinhole.archetype {
            RerunArchetype::Pinhole {
                image_from_camera,
                width,
                height,
            } => {
                assert_eq!(image_from_camera[0], 900.0);
                assert_eq!((*width, *height), (1280, 720));
            }
            _ => unreachable!(),
        }
        // The accel-z scalar is gravity.
        let az = entries
            .iter()
            .find(|e| e.entity_path == "world/imu/accel/z")
            .unwrap();
        assert_eq!(az.archetype, RerunArchetype::Scalar { value: 9.81 });
        assert_eq!(az.timeline_ms, 999);
    }

    #[test]
    fn hevc_encoding_serializes_hyphenated() {
        let mut kf = keyframe();
        kf.image.encoding = ImageEncoding::HevcKeyframe;
        let img = log_keyframe(&kf)
            .into_iter()
            .find(|e| matches!(e.archetype, RerunArchetype::Image { .. }))
            .unwrap();
        match img.archetype {
            RerunArchetype::Image { encoding, .. } => assert_eq!(encoding, "hevc-keyframe"),
            _ => unreachable!(),
        }
    }

    #[test]
    fn world_model_descriptors_map_to_world_subtree_with_payloads() {
        let splat = log_splat(
            &SplatDescriptor {
                gaussian_count: 5,
                step: 50,
                url: None,
                handle: None,
            },
            7,
        );
        assert_eq!(splat.entity_path, "world/splat");
        assert_eq!(splat.timeline_ms, 7);
        assert_eq!(
            splat.archetype,
            RerunArchetype::SplatSlab {
                gaussian_count: 5,
                step: 50
            }
        );

        let cloud = log_pointcloud(
            &PointCloudDescriptor {
                point_count: 9,
                bounds: [-1.0, -1.0, -1.0, 1.0, 1.0, 1.0],
                shm_name: None,
                slot: None,
                seq: None,
                url: None,
            },
            1,
        );
        assert_eq!(cloud.entity_path, "world/points");
        assert_eq!(
            cloud.archetype,
            RerunArchetype::Points3D {
                count: 9,
                bounds: [-1.0, -1.0, -1.0, 1.0, 1.0, 1.0]
            }
        );

        let mesh = log_mesh(
            &MeshDescriptor {
                vertex_count: 1,
                face_count: 2,
                url: None,
                handle: None,
            },
            1,
        );
        assert_eq!(mesh.entity_path, "world/mesh");
        assert_eq!(
            mesh.archetype,
            RerunArchetype::Mesh3D {
                vertex_count: 1,
                face_count: 2
            }
        );

        let occ = log_occupancy(
            &OccupancyDescriptor {
                origin: [0.5, 0.0, 0.0],
                resolution_m: 0.05,
                dims: [10, 10, 4],
                shm_name: None,
                slot: None,
                seq: None,
            },
            1,
        );
        assert_eq!(occ.entity_path, "world/occupancy");
        assert_eq!(
            occ.archetype,
            RerunArchetype::VoxelGrid {
                dims: [10, 10, 4],
                resolution_m: 0.05,
                origin: [0.5, 0.0, 0.0]
            }
        );
    }

    #[test]
    fn pinhole_is_logged_once_per_camera() {
        let mut rec = RerunRecording::new();
        rec.push_keyframe(&keyframe()); // first: includes the pinhole (9 entries)
        rec.push_keyframe(&keyframe()); // same camera: pinhole dropped (8 entries)
        let pinholes = rec
            .entries
            .iter()
            .filter(|e| matches!(e.archetype, RerunArchetype::Pinhole { .. }))
            .count();
        assert_eq!(pinholes, 1);
        assert_eq!(rec.len(), 9 + 8);
    }

    #[test]
    fn recording_accumulates_and_serializes() {
        let mut rec = RerunRecording::new();
        assert!(rec.is_empty());
        rec.push_keyframe(&keyframe());
        rec.push_splat(
            &SplatDescriptor {
                gaussian_count: 1200,
                step: 50,
                url: None,
                handle: None,
            },
            1001,
        );
        assert_eq!(rec.len(), 9 + 1);
        let json = rec.to_json().unwrap();
        assert!(json.contains("world/splat"));
        assert!(json.contains("\"archetype\""));
        // The discriminators are the Rerun archetype names verbatim, not
        // snake_case digit-splits (would be "transform3_d" / "splat_slab").
        assert!(json.contains("\"Transform3D\""));
        assert!(json.contains("\"SplatSlab\""));
        assert!(!json.contains("transform3_d"));
        // Round-trips back to the same entries.
        let back: RerunRecording = serde_json::from_str(&json).unwrap();
        assert_eq!(back.entries, rec.entries);
    }
}
