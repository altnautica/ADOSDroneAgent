//! Turn received Atlas keyframes into a reconstructor-ingestible dataset.
//!
//! Each keyframe carries the compressed image bytes plus a world-from-camera
//! pose and the camera intrinsics (the drone computed the pose on-board from VIO
//! or from an offloaded SLAM return). This module streams each keyframe's image
//! to disk as it arrives and accumulates the lightweight per-frame camera record,
//! then on bag-finalize writes a Nerfstudio `transforms.json` next to the images.
//!
//! A gaussian-splat trainer (Brush) ingests a directory that holds a
//! `transforms.json` plus an `images/` subdir directly through its Nerfstudio
//! reader, so a captured session becomes a real splat input with no
//! structure-from-motion pre-pass when the poses are already known. The same
//! directory feeds a COLMAP pre-pass when poses must be refined.
//!
//! Memory stays bounded: the image bytes are written to disk immediately and only
//! the small per-frame record (a file path, a 4x4 matrix, and the intrinsics) is
//! held in memory until finalize.

use std::collections::HashMap;
use std::path::PathBuf;

use ados_protocol::atlas::{Distortion, ImageEncoding, KeyframeEnvelope};
use serde::Serialize;

/// Subdirectory under a dataset directory that holds the keyframe images.
const IMAGES_DIR: &str = "images";
/// The Nerfstudio dataset manifest filename the trainer reads.
const TRANSFORMS_FILE: &str = "transforms.json";

/// The dataset id a session's keyframes persist under. Matches the id the ingest
/// enqueues its reconstruct job against, so the trainer's `input_path` resolves
/// to this directory.
pub fn dataset_id_for(session_id: &str) -> String {
    format!("ds-{session_id}")
}

/// One frame entry in `transforms.json`: the image path (relative to the
/// manifest) plus the 4x4 camera-to-world matrix and the per-frame intrinsics, so
/// a multi-camera rig with differing optics is exact (the trainer honours
/// per-frame intrinsics, falling back to the scene-level ones when a frame omits
/// them).
#[derive(Debug, Clone, Serialize)]
struct NerfFrame {
    file_path: String,
    transform_matrix: [[f64; 4]; 4],
    fl_x: f64,
    fl_y: f64,
    cx: f64,
    cy: f64,
    w: u32,
    h: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    k1: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    k2: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    p1: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    p2: Option<f64>,
}

/// The Nerfstudio `transforms.json` document: scene-level intrinsics (taken from
/// the first keyframe) plus the per-frame array. `camera_model` is `OPENCV`, the
/// pinhole + radial-tangential model the keyframe intrinsics K and `radtan`
/// distortion are expressed in.
#[derive(Debug, Clone, Serialize)]
struct NerfScene {
    camera_model: String,
    fl_x: f64,
    fl_y: f64,
    cx: f64,
    cy: f64,
    w: u32,
    h: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    k1: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    k2: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    p1: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    p2: Option<f64>,
    frames: Vec<NerfFrame>,
}

/// The on-disk file extension for an image encoding. The capture path encodes
/// keyframes as JPEG; the HEVC keyframe branch is carried for completeness.
fn image_ext(encoding: ImageEncoding) -> &'static str {
    match encoding {
        ImageEncoding::Jpeg => "jpg",
        ImageEncoding::HevcKeyframe => "h265",
    }
}

/// The (k1, k2, p1, p2) radial-tangential coefficients for a distortion model the
/// `OPENCV` camera model understands, or all `None` for an unrecognised model or
/// a parameter vector too short to read (never guessed — an unknown model emits a
/// plain pinhole with no distortion).
fn radtan_coeffs(d: &Distortion) -> (Option<f64>, Option<f64>, Option<f64>, Option<f64>) {
    let is_radtan = matches!(
        d.model.to_ascii_lowercase().as_str(),
        "radtan"
            | "radtan4"
            | "radial-tangential"
            | "opencv"
            | "brown"
            | "brown-conrady"
            | "plumb_bob"
            | "plumb-bob"
    );
    if is_radtan && d.params.len() >= 4 {
        (
            Some(d.params[0]),
            Some(d.params[1]),
            Some(d.params[2]),
            Some(d.params[3]),
        )
    } else {
        (None, None, None, None)
    }
}

/// Build the Nerfstudio `transform_matrix` (a 4x4 camera-to-world in the
/// OpenGL/Blender convention the trainer reads) from a world-from-camera pose
/// expressed in the OpenCV camera convention (+X right, +Y down, +Z forward — the
/// convention the keyframe's pinhole K and `radtan` distortion are in). The
/// conversion negates the camera-frame Y and Z basis vectors (post-multiply by
/// `diag(1, -1, -1, 1)`), the same flip a Nerfstudio dataset writer applies to
/// COLMAP / OpenCV extrinsics; the camera position is unchanged.
fn opencv_c2w_to_opengl_matrix(r: &[f64; 9], t: &[f64; 3]) -> [[f64; 4]; 4] {
    [
        [r[0], -r[1], -r[2], t[0]],
        [r[3], -r[4], -r[5], t[1]],
        [r[6], -r[7], -r[8], t[2]],
        [0.0, 0.0, 0.0, 1.0],
    ]
}

/// The per-frame record for one keyframe (intrinsics from its own K, pose flipped
/// into the trainer's convention, image path relative to the manifest).
fn nerf_frame(kf: &KeyframeEnvelope, file_path: String) -> NerfFrame {
    let k = &kf.camera.k;
    let (k1, k2, p1, p2) = radtan_coeffs(&kf.camera.distortion);
    NerfFrame {
        file_path,
        transform_matrix: opencv_c2w_to_opengl_matrix(&kf.pose.r, &kf.pose.t),
        // K is row-major [fx, 0, cx, 0, fy, cy, 0, 0, 1].
        fl_x: k[0],
        fl_y: k[4],
        cx: k[2],
        cy: k[5],
        w: kf.image.width,
        h: kf.image.height,
        k1,
        k2,
        p1,
        p2,
    }
}

/// The scene document built from a session's accumulated frames; the scene-level
/// intrinsics are the first frame's (a single-camera dataset is then fully
/// described at the scene level, and a multi-camera one is corrected per frame).
fn scene_from_frames(frames: Vec<NerfFrame>) -> NerfScene {
    let f0 = &frames[0];
    NerfScene {
        camera_model: "OPENCV".to_string(),
        fl_x: f0.fl_x,
        fl_y: f0.fl_y,
        cx: f0.cx,
        cy: f0.cy,
        w: f0.w,
        h: f0.h,
        k1: f0.k1,
        k2: f0.k2,
        p1: f0.p1,
        p2: f0.p2,
        frames: frames.clone(),
    }
}

/// Accumulates a single dataset's frame records until finalize.
#[derive(Debug, Default)]
struct DatasetAccum {
    frames: Vec<NerfFrame>,
}

/// Streams keyframe images to disk and writes the Nerfstudio manifest per
/// dataset. One persister serves the whole receive loop; it keys by dataset id so
/// interleaved sessions never cross-contaminate.
#[derive(Debug, Default)]
pub struct KeyframePersister {
    work_root: PathBuf,
    datasets: HashMap<String, DatasetAccum>,
}

impl KeyframePersister {
    /// A persister that writes datasets under `work_root` (the same root the
    /// reconstructor reads `input_path` from).
    pub fn new(work_root: impl Into<PathBuf>) -> Self {
        Self {
            work_root: work_root.into(),
            datasets: HashMap::new(),
        }
    }

    /// Persist one keyframe: write its image to
    /// `<work_root>/<dataset_id>/images/<kf_id>.<ext>` and record its frame entry.
    /// The image bytes are written immediately (memory stays bounded); only the
    /// small frame record is retained. Returns an I/O error on a filesystem fault;
    /// the caller logs and drops (a single lost keyframe is not fatal).
    pub fn persist(&mut self, kf: &KeyframeEnvelope) -> std::io::Result<()> {
        let dataset_id = dataset_id_for(&kf.session_id);
        let dataset_dir = self.work_root.join(&dataset_id);
        let images_dir = dataset_dir.join(IMAGES_DIR);
        std::fs::create_dir_all(&images_dir)?;
        let file_path = format!("{IMAGES_DIR}/{}.{}", kf.kf_id, image_ext(kf.image.encoding));
        std::fs::write(dataset_dir.join(&file_path), &kf.image.bytes)?;
        self.datasets
            .entry(dataset_id)
            .or_default()
            .frames
            .push(nerf_frame(kf, file_path));
        Ok(())
    }

    /// Frames recorded for a dataset so far (the persisted-image count, distinct
    /// from the received-event count the ingest tracks).
    pub fn frame_count(&self, dataset_id: &str) -> usize {
        self.datasets
            .get(dataset_id)
            .map(|d| d.frames.len())
            .unwrap_or(0)
    }

    /// Write `<work_root>/<dataset_id>/transforms.json` from the recorded frames
    /// and return the dataset directory (the `input_path` the reconstructor reads).
    /// The accumulated frames are consumed, so a re-finalize (a re-sent terminal
    /// state on the lossy fire-and-forget lane) is idempotent: it returns the same
    /// directory when the manifest already exists. Returns `None` when no frame was
    /// persisted (an empty dataset has nothing to reconstruct).
    pub fn finalize(&mut self, dataset_id: &str) -> std::io::Result<Option<PathBuf>> {
        let dataset_dir = self.work_root.join(dataset_id);
        let Some(accum) = self.datasets.remove(dataset_id) else {
            // Already finalized this session, or never saw a frame: the manifest's
            // presence is the truth.
            return Ok(dataset_dir
                .join(TRANSFORMS_FILE)
                .exists()
                .then_some(dataset_dir));
        };
        if accum.frames.is_empty() {
            return Ok(None);
        }
        std::fs::create_dir_all(&dataset_dir)?;
        let scene = scene_from_frames(accum.frames);
        let json = serde_json::to_vec_pretty(&scene).map_err(std::io::Error::other)?;
        std::fs::write(dataset_dir.join(TRANSFORMS_FILE), json)?;
        Ok(Some(dataset_dir))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ados_protocol::atlas::{
        CameraIntrinsics, CameraRole, ImageEncoding, KeyframeFlags, KeyframeImage, KeyframeTier,
        Pose, PoseSource,
    };
    use std::path::Path;

    fn keyframe(session: &str, kf_id: u64, camera: &str, fx: f64) -> KeyframeEnvelope {
        KeyframeEnvelope {
            session_id: session.into(),
            kf_id,
            ts_unix_ms: 1_700_000_000_000 + kf_id as i64,
            camera_id: camera.into(),
            camera_role: CameraRole::Primary,
            tier: KeyframeTier::Full,
            image: KeyframeImage {
                encoding: ImageEncoding::Jpeg,
                width: 1280,
                height: 720,
                bytes: vec![0xFF, 0xD8, 0xFF, kf_id as u8],
            },
            camera: CameraIntrinsics {
                k: [fx, 0.0, 640.0, 0.0, fx, 360.0, 0.0, 0.0, 1.0],
                distortion: Distortion {
                    model: "radtan".into(),
                    params: vec![0.1, -0.2, 0.001, 0.002],
                },
            },
            // A non-trivial rotation so the OpenCV->OpenGL column flip is visible.
            pose: Pose {
                r: [1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 9.0],
                t: [1.5, -2.0, 0.5],
                cov: None,
            },
            pose_source: PoseSource::LocalVio,
            global_anchor: None,
            imu_window: Vec::new(),
            flags: KeyframeFlags::default(),
        }
    }

    fn read_transforms(dir: &Path) -> serde_json::Value {
        let bytes = std::fs::read(dir.join(TRANSFORMS_FILE)).expect("transforms.json written");
        serde_json::from_slice(&bytes).expect("transforms.json is valid json")
    }

    #[test]
    fn opencv_to_opengl_negates_y_and_z_columns_keeps_position() {
        let m = opencv_c2w_to_opengl_matrix(
            &[1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 9.0],
            &[10.0, 11.0, 12.0],
        );
        // Column 0 (X axis) unchanged, columns 1 and 2 (Y, Z) negated.
        assert_eq!(m[0], [1.0, -2.0, -3.0, 10.0]);
        assert_eq!(m[1], [4.0, -5.0, -6.0, 11.0]);
        assert_eq!(m[2], [7.0, -8.0, -9.0, 12.0]);
        assert_eq!(m[3], [0.0, 0.0, 0.0, 1.0]);
    }

    #[test]
    fn single_camera_persist_then_finalize_writes_brush_schema() {
        let dir = tempfile::tempdir().unwrap();
        let mut p = KeyframePersister::new(dir.path());
        for kf_id in 0..3 {
            p.persist(&keyframe("sessA", kf_id, "front", 900.0))
                .unwrap();
        }
        let dataset_id = dataset_id_for("sessA");
        assert_eq!(p.frame_count(&dataset_id), 3);

        let ds_dir = p
            .finalize(&dataset_id)
            .unwrap()
            .expect("a non-empty dataset");
        assert_eq!(ds_dir, dir.path().join(&dataset_id));

        // Each image landed under images/ with the kf_id name and the real bytes.
        for kf_id in 0..3u64 {
            let img = ds_dir.join(IMAGES_DIR).join(format!("{kf_id}.jpg"));
            let bytes = std::fs::read(&img).unwrap();
            assert_eq!(bytes, vec![0xFF, 0xD8, 0xFF, kf_id as u8]);
        }

        let t = read_transforms(&ds_dir);
        // Scene-level intrinsics + OPENCV camera model + radtan coeffs.
        assert_eq!(t["camera_model"], "OPENCV");
        assert_eq!(t["fl_x"], 900.0);
        assert_eq!(t["fl_y"], 900.0);
        assert_eq!(t["cx"], 640.0);
        assert_eq!(t["cy"], 360.0);
        assert_eq!(t["w"], 1280);
        assert_eq!(t["h"], 720);
        assert_eq!(t["k1"], 0.1);
        assert_eq!(t["k2"], -0.2);
        assert_eq!(t["p1"], 0.001);
        assert_eq!(t["p2"], 0.002);

        let frames = t["frames"].as_array().unwrap();
        assert_eq!(frames.len(), 3);
        // file_path is relative to the manifest (images/<kf_id>.jpg).
        assert_eq!(frames[0]["file_path"], "images/0.jpg");
        assert_eq!(frames[2]["file_path"], "images/2.jpg");
        // transform_matrix is a 4x4 with the Y/Z columns flipped from the pose R.
        let m = &frames[0]["transform_matrix"];
        assert_eq!(m[0][0], 1.0);
        assert_eq!(m[0][1], -2.0);
        assert_eq!(m[0][2], -3.0);
        assert_eq!(m[0][3], 1.5);
        assert_eq!(m[3], serde_json::json!([0.0, 0.0, 0.0, 1.0]));
        // Per-frame intrinsics are present so a multi-camera rig is exact.
        assert_eq!(frames[0]["fl_x"], 900.0);
        assert_eq!(frames[0]["w"], 1280);
    }

    #[test]
    fn multi_camera_keeps_per_frame_intrinsics() {
        let dir = tempfile::tempdir().unwrap();
        let mut p = KeyframePersister::new(dir.path());
        // Two cameras with different focal lengths in one session.
        p.persist(&keyframe("multi", 0, "front", 900.0)).unwrap();
        p.persist(&keyframe("multi", 1, "down", 700.0)).unwrap();
        let ds_dir = p.finalize(&dataset_id_for("multi")).unwrap().unwrap();
        let t = read_transforms(&ds_dir);
        let frames = t["frames"].as_array().unwrap();
        // Scene-level is the first frame's; each frame carries its own intrinsics.
        assert_eq!(t["fl_x"], 900.0);
        assert_eq!(frames[0]["fl_x"], 900.0);
        assert_eq!(frames[1]["fl_x"], 700.0);
    }

    #[test]
    fn an_unknown_distortion_model_omits_coefficients() {
        let dir = tempfile::tempdir().unwrap();
        let mut p = KeyframePersister::new(dir.path());
        let mut kf = keyframe("nodist", 0, "front", 800.0);
        kf.camera.distortion = Distortion {
            model: "fisheye-unknown".into(),
            params: vec![0.5, 0.5, 0.5, 0.5],
        };
        p.persist(&kf).unwrap();
        let ds_dir = p.finalize(&dataset_id_for("nodist")).unwrap().unwrap();
        let t = read_transforms(&ds_dir);
        // OPENCV pinhole with no guessed distortion coefficients.
        assert_eq!(t["camera_model"], "OPENCV");
        assert!(t.get("k1").is_none());
        assert!(t["frames"][0].get("p1").is_none());
    }

    #[test]
    fn finalize_with_no_frames_is_none() {
        let dir = tempfile::tempdir().unwrap();
        let mut p = KeyframePersister::new(dir.path());
        assert!(p.finalize(&dataset_id_for("empty")).unwrap().is_none());
    }

    #[test]
    fn re_finalize_is_idempotent_and_returns_the_same_dir() {
        let dir = tempfile::tempdir().unwrap();
        let mut p = KeyframePersister::new(dir.path());
        p.persist(&keyframe("again", 0, "front", 900.0)).unwrap();
        let first = p.finalize(&dataset_id_for("again")).unwrap().unwrap();
        // A re-sent terminal state: the frames were consumed, but the manifest
        // exists, so finalize returns the same directory rather than None.
        let second = p.finalize(&dataset_id_for("again")).unwrap().unwrap();
        assert_eq!(first, second);
    }
}
