//! COLMAP posed-triangulation seed for the native-Metal splat trainer.
//!
//! `msplat` (native Metal, ~10x faster than the portable wgpu trainer) has NO
//! random initialization: it copies a point cloud into the gaussian means, so a
//! pose-only dataset (a `transforms.json` with camera poses but no points)
//! trains to zero gaussians. This module produces the missing point cloud WITHOUT
//! a full structure-from-motion pass: it feeds COLMAP the poses the drone already
//! computed (VIO / FC-GPS fusion) and only asks it to TRIANGULATE — extract
//! features, match, and triangulate 3D points into the KNOWN camera geometry. That
//! is far cheaper than letting COLMAP solve the poses too, and it keeps the points
//! in the SAME world frame as the `transforms.json` cameras, so `msplat` reads the
//! seed and the poses as one consistent scene.
//!
//! The result lands at `<dataset>/points3D.ply` — the file `msplat`'s Nerfstudio
//! loader auto-discovers next to `transforms.json` — so seeding is a pure
//! pre-pass with no change to the trainer command.
//!
//! The pose conversion and the COLMAP model text are pure and unit-tested; the
//! four COLMAP invocations run only when `colmap` is on `PATH`. A seed that can
//! not produce enough points (COLMAP absent, a textureless scene, poor matches)
//! is not a failure of the pipeline: the caller falls back to the portable trainer
//! (Brush random-init), which trains from the poses alone.

use std::path::Path;
use std::process::Command;

/// The manifest a captured dataset carries (Nerfstudio format, written by the
/// keyframe persister).
const TRANSFORMS_FILE: &str = "transforms.json";
/// The images subdir COLMAP extracts features from.
const IMAGES_DIR: &str = "images";
/// The seed point cloud filename `msplat`'s loader auto-discovers next to the
/// manifest. A `.ply` with only x/y/z is enough to initialize the trainer.
const POINTS_FILE: &str = "points3D.ply";
/// The scratch dir the COLMAP database + intermediate models live under (inside
/// the dataset dir, so it is cleaned with the dataset and never pollutes the work
/// root).
const SEED_SCRATCH: &str = ".colmap-seed";

/// Fewer triangulated points than this and the seed is treated as unusable — the
/// caller trains with the portable random-init trainer instead of feeding `msplat`
/// a cloud too sparse to initialize a scene. A real posed triangulation of even a
/// small capture yields hundreds to thousands of points.
pub const MIN_SEED_POINTS: u64 = 32;

/// Why a seed could not be produced. Every variant means "fall back to the
/// portable trainer", never "abort the job".
#[derive(Debug)]
pub enum SeedError {
    /// The manifest was missing, unreadable, or not valid JSON.
    Manifest(String),
    /// A COLMAP step exited non-zero (or could not be spawned).
    Colmap { step: &'static str, message: String },
    /// A filesystem fault writing the COLMAP model or reading the result.
    Io(std::io::Error),
}

impl std::fmt::Display for SeedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SeedError::Manifest(m) => write!(f, "seed manifest: {m}"),
            SeedError::Colmap { step, message } => write!(f, "colmap {step}: {message}"),
            SeedError::Io(e) => write!(f, "seed io: {e}"),
        }
    }
}

impl From<std::io::Error> for SeedError {
    fn from(e: std::io::Error) -> Self {
        SeedError::Io(e)
    }
}

/// One camera view for the COLMAP input model: the image name COLMAP keys on
/// (basename, matching the database), the pinhole intrinsics + optional
/// radial-tangential distortion, and the world-to-camera pose (COLMAP's own
/// convention) derived from the manifest's camera-to-world matrix.
#[derive(Debug, Clone, PartialEq)]
struct SeedView {
    name: String,
    w: u32,
    h: u32,
    fx: f64,
    fy: f64,
    cx: f64,
    cy: f64,
    /// (k1, k2, p1, p2) when the manifest carried OPENCV distortion, else None
    /// (a plain PINHOLE camera).
    distortion: Option<[f64; 4]>,
    /// COLMAP world-to-camera quaternion (qw, qx, qy, qz).
    quat: [f64; 4],
    /// COLMAP world-to-camera translation.
    trans: [f64; 3],
}

/// Convert a Nerfstudio camera-to-world matrix (OpenGL/Blender convention, the
/// form the keyframe persister writes) to COLMAP's world-to-camera pose
/// (quaternion + translation, OpenCV convention). The persister built the matrix
/// by negating the Y and Z columns of the OpenCV camera-to-world rotation and
/// keeping the camera position; this reverses that (negate the Y/Z columns back
/// to recover the OpenCV rotation), transposes to world-to-camera, and derives the
/// translation `-R_w2c * C` from the camera center `C`. The world coordinates are
/// unchanged, so the triangulated points land in the same frame as the manifest's
/// camera positions.
fn opengl_c2w_to_colmap_w2c(m: &[[f64; 4]; 4]) -> ([f64; 4], [f64; 3]) {
    // Recover the OpenCV camera-to-world rotation: negate columns 1 and 2.
    let r_c2w = [
        [m[0][0], -m[0][1], -m[0][2]],
        [m[1][0], -m[1][1], -m[1][2]],
        [m[2][0], -m[2][1], -m[2][2]],
    ];
    // Camera center in world.
    let c = [m[0][3], m[1][3], m[2][3]];
    // World-to-camera rotation is the transpose.
    let r_w2c = [
        [r_c2w[0][0], r_c2w[1][0], r_c2w[2][0]],
        [r_c2w[0][1], r_c2w[1][1], r_c2w[2][1]],
        [r_c2w[0][2], r_c2w[1][2], r_c2w[2][2]],
    ];
    // t_w2c = -R_w2c * C.
    let trans = [
        -(r_w2c[0][0] * c[0] + r_w2c[0][1] * c[1] + r_w2c[0][2] * c[2]),
        -(r_w2c[1][0] * c[0] + r_w2c[1][1] * c[1] + r_w2c[1][2] * c[2]),
        -(r_w2c[2][0] * c[0] + r_w2c[2][1] * c[1] + r_w2c[2][2] * c[2]),
    ];
    (mat3_to_quat(&r_w2c), trans)
}

/// Unit quaternion `(qw, qx, qy, qz)` from a 3x3 rotation matrix (row-major),
/// Shepperd's method with the largest-diagonal branch for numerical stability.
fn mat3_to_quat(r: &[[f64; 3]; 3]) -> [f64; 4] {
    let trace = r[0][0] + r[1][1] + r[2][2];
    let (qw, qx, qy, qz) = if trace > 0.0 {
        let s = (trace + 1.0).sqrt() * 2.0; // s = 4 * qw
        (
            0.25 * s,
            (r[2][1] - r[1][2]) / s,
            (r[0][2] - r[2][0]) / s,
            (r[1][0] - r[0][1]) / s,
        )
    } else if r[0][0] > r[1][1] && r[0][0] > r[2][2] {
        let s = (1.0 + r[0][0] - r[1][1] - r[2][2]).sqrt() * 2.0; // s = 4 * qx
        (
            (r[2][1] - r[1][2]) / s,
            0.25 * s,
            (r[0][1] + r[1][0]) / s,
            (r[0][2] + r[2][0]) / s,
        )
    } else if r[1][1] > r[2][2] {
        let s = (1.0 + r[1][1] - r[0][0] - r[2][2]).sqrt() * 2.0; // s = 4 * qy
        (
            (r[0][2] - r[2][0]) / s,
            (r[0][1] + r[1][0]) / s,
            0.25 * s,
            (r[1][2] + r[2][1]) / s,
        )
    } else {
        let s = (1.0 + r[2][2] - r[0][0] - r[1][1]).sqrt() * 2.0; // s = 4 * qz
        (
            (r[1][0] - r[0][1]) / s,
            (r[0][2] + r[2][0]) / s,
            (r[1][2] + r[2][1]) / s,
            0.25 * s,
        )
    };
    let n = (qw * qw + qx * qx + qy * qy + qz * qz).sqrt();
    if n == 0.0 {
        [1.0, 0.0, 0.0, 0.0]
    } else {
        [qw / n, qx / n, qy / n, qz / n]
    }
}

/// Read an f64 field from a JSON object, falling back to a scene-level default.
fn num(obj: &serde_json::Value, key: &str, scene: &serde_json::Value) -> Option<f64> {
    obj.get(key)
        .or_else(|| scene.get(key))
        .and_then(|v| v.as_f64())
}

/// Parse a Nerfstudio `transforms.json` into per-view COLMAP inputs. Each frame's
/// intrinsics fall back to the scene-level ones; the image name is the basename of
/// `file_path` (COLMAP keys images by their name under `--image_path`). Distortion
/// is carried only when all four OPENCV coefficients are present (never guessed).
fn parse_transforms(manifest: &serde_json::Value) -> Result<Vec<SeedView>, SeedError> {
    let frames = manifest
        .get("frames")
        .and_then(|v| v.as_array())
        .ok_or_else(|| SeedError::Manifest("no frames array".into()))?;
    let mut views = Vec::with_capacity(frames.len());
    for (i, frame) in frames.iter().enumerate() {
        let m = frame
            .get("transform_matrix")
            .and_then(parse_matrix4)
            .ok_or_else(|| SeedError::Manifest(format!("frame {i}: bad transform_matrix")))?;
        let fx = num(frame, "fl_x", manifest)
            .ok_or_else(|| SeedError::Manifest(format!("frame {i}: no fl_x")))?;
        let fy = num(frame, "fl_y", manifest).unwrap_or(fx);
        let cx = num(frame, "cx", manifest)
            .ok_or_else(|| SeedError::Manifest(format!("frame {i}: no cx")))?;
        let cy = num(frame, "cy", manifest)
            .ok_or_else(|| SeedError::Manifest(format!("frame {i}: no cy")))?;
        let w = num(frame, "w", manifest).unwrap_or(0.0) as u32;
        let h = num(frame, "h", manifest).unwrap_or(0.0) as u32;
        let distortion = match (
            num(frame, "k1", manifest),
            num(frame, "k2", manifest),
            num(frame, "p1", manifest),
            num(frame, "p2", manifest),
        ) {
            (Some(k1), Some(k2), Some(p1), Some(p2)) => Some([k1, k2, p1, p2]),
            _ => None,
        };
        let name = frame
            .get("file_path")
            .and_then(|v| v.as_str())
            .map(image_name)
            .ok_or_else(|| SeedError::Manifest(format!("frame {i}: no file_path")))?;
        let (quat, trans) = opengl_c2w_to_colmap_w2c(&m);
        views.push(SeedView {
            name,
            w,
            h,
            fx,
            fy,
            cx,
            cy,
            distortion,
            quat,
            trans,
        });
    }
    if views.is_empty() {
        return Err(SeedError::Manifest("no frames".into()));
    }
    Ok(views)
}

/// The COLMAP image name for a manifest `file_path`: the basename, since COLMAP
/// keys images by their path relative to `--image_path` (the `images/` dir), so
/// `images/12.jpg` becomes `12.jpg`.
fn image_name(file_path: &str) -> String {
    file_path
        .rsplit('/')
        .next()
        .unwrap_or(file_path)
        .to_string()
}

/// Parse a JSON 4x4 array (`[[..],[..],[..],[..]]`) into a matrix, or `None` if
/// the shape is wrong.
fn parse_matrix4(v: &serde_json::Value) -> Option<[[f64; 4]; 4]> {
    let rows = v.as_array()?;
    if rows.len() != 4 {
        return None;
    }
    let mut m = [[0.0f64; 4]; 4];
    for (i, row) in rows.iter().enumerate() {
        let cols = row.as_array()?;
        if cols.len() != 4 {
            return None;
        }
        for (j, c) in cols.iter().enumerate() {
            m[i][j] = c.as_f64()?;
        }
    }
    Some(m)
}

/// The COLMAP `cameras.txt` for the views: one camera per view (camera id = view
/// index + 1), OPENCV when distortion is present else PINHOLE, so a multi-camera
/// rig with differing optics is exact and a plain pinhole capture stays simple.
fn cameras_txt(views: &[SeedView]) -> String {
    let mut s = String::from("# Camera list\n");
    for (i, v) in views.iter().enumerate() {
        let id = i + 1;
        match v.distortion {
            Some([k1, k2, p1, p2]) => s.push_str(&format!(
                "{id} OPENCV {} {} {} {} {} {} {k1} {k2} {p1} {p2}\n",
                v.w, v.h, v.fx, v.fy, v.cx, v.cy
            )),
            None => s.push_str(&format!(
                "{id} PINHOLE {} {} {} {} {} {}\n",
                v.w, v.h, v.fx, v.fy, v.cx, v.cy
            )),
        }
    }
    s
}

/// The COLMAP `images.txt` for the views: two lines per image — the pose line
/// (`IMAGE_ID QW QX QY QZ TX TY TZ CAMERA_ID NAME`) then an EMPTY 2D-point line
/// (the triangulator fills the observations). Image id = camera id = index + 1.
fn images_txt(views: &[SeedView]) -> String {
    let mut s = String::from("# Image list\n");
    for (i, v) in views.iter().enumerate() {
        let id = i + 1;
        let [qw, qx, qy, qz] = v.quat;
        let [tx, ty, tz] = v.trans;
        s.push_str(&format!(
            "{id} {qw} {qx} {qy} {qz} {tx} {ty} {tz} {id} {}\n\n",
            v.name
        ));
    }
    s
}

/// Read the vertex count out of a PLY header (`element vertex <n>`). COLMAP's
/// PLY export writes this line; it is enough to gate the seed on the point count
/// without parsing the whole cloud.
fn parse_ply_vertex_count(header: &str) -> u64 {
    for line in header.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("element vertex ") {
            if let Ok(n) = rest.trim().parse::<u64>() {
                return n;
            }
        }
        if line == "end_header" {
            break;
        }
    }
    0
}

/// Read the vertex count of a PLY file from its header (the first bytes), 0 on any
/// read error or a missing count.
fn ply_vertex_count(path: &Path) -> u64 {
    let Ok(bytes) = std::fs::read(path) else {
        return 0;
    };
    // The header is ASCII and short; read only up to end_header.
    let head = String::from_utf8_lossy(&bytes[..bytes.len().min(4096)]);
    parse_ply_vertex_count(&head)
}

/// Whether a usable seed already sits next to the manifest (an idempotent re-run
/// or an externally-provided cloud), so the COLMAP pass is skipped.
fn existing_seed(dataset_dir: &Path) -> Option<u64> {
    let p = dataset_dir.join(POINTS_FILE);
    if p.is_file() {
        let n = ply_vertex_count(&p);
        if n >= MIN_SEED_POINTS {
            return Some(n);
        }
    }
    None
}

/// Run one COLMAP subcommand, mapping a spawn failure or non-zero exit into a
/// [`SeedError::Colmap`] tagged with the step name.
fn run_colmap(step: &'static str, args: &[String]) -> Result<(), SeedError> {
    let output = Command::new("colmap")
        .args(args)
        .output()
        .map_err(|e| SeedError::Colmap {
            step,
            message: format!("spawn: {e}"),
        })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(SeedError::Colmap {
            step,
            message: format!("exit {}: {}", output.status, stderr.trim()),
        });
    }
    Ok(())
}

/// Produce `<dataset_dir>/points3D.ply` by COLMAP posed triangulation over the
/// dataset's `transforms.json` + `images/`, reusing the manifest's known poses
/// (no pose search). Returns the triangulated point count. Idempotent: an existing
/// usable `points3D.ply` is returned without re-running COLMAP.
///
/// The four COLMAP steps — feature extraction, sequential matching (the drone's
/// keyframes are time-ordered), posed triangulation into the known geometry, and
/// PLY export — run under a scratch dir inside the dataset. SIFT runs on the CPU
/// by default (`seed_use_gpu` param overrides) so the seed is reliable on a
/// headless Mac / GPU-less node; the trainer, not the seed, is the speed-critical
/// stage. A `SeedError` (COLMAP absent, too few features, a bad manifest) is the
/// caller's signal to fall back to the portable random-init trainer.
pub fn seed_points(dataset_dir: &Path, params: &serde_json::Value) -> Result<u64, SeedError> {
    if let Some(n) = existing_seed(dataset_dir) {
        return Ok(n);
    }
    let manifest_path = dataset_dir.join(TRANSFORMS_FILE);
    let text = std::fs::read_to_string(&manifest_path)
        .map_err(|e| SeedError::Manifest(format!("read {}: {e}", manifest_path.display())))?;
    let manifest: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| SeedError::Manifest(format!("parse: {e}")))?;
    let views = parse_transforms(&manifest)?;

    let images_dir = dataset_dir.join(IMAGES_DIR);
    let scratch = dataset_dir.join(SEED_SCRATCH);
    let model_in = scratch.join("model-in");
    let model_out = scratch.join("model-out");
    let db = scratch.join("database.db");
    std::fs::create_dir_all(&model_in)?;
    std::fs::create_dir_all(&model_out)?;

    // The input model: our cameras + poses, no points (the triangulator fills them).
    std::fs::write(model_in.join("cameras.txt"), cameras_txt(&views))?;
    std::fs::write(model_in.join("images.txt"), images_txt(&views))?;
    std::fs::write(model_in.join("points3D.txt"), "# 3D points\n")?;

    let use_gpu = params
        .get("seed_use_gpu")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let gpu = if use_gpu { "1" } else { "0" };
    let db_s = db.to_string_lossy().into_owned();
    let images_s = images_dir.to_string_lossy().into_owned();

    run_colmap(
        "feature_extractor",
        &[
            "feature_extractor".into(),
            "--database_path".into(),
            db_s.clone(),
            "--image_path".into(),
            images_s.clone(),
            "--SiftExtraction.use_gpu".into(),
            gpu.into(),
        ],
    )?;
    run_colmap(
        "sequential_matcher",
        &[
            "sequential_matcher".into(),
            "--database_path".into(),
            db_s.clone(),
            "--SiftMatching.use_gpu".into(),
            gpu.into(),
        ],
    )?;
    run_colmap(
        "point_triangulator",
        &[
            "point_triangulator".into(),
            "--database_path".into(),
            db_s,
            "--image_path".into(),
            images_s,
            "--input_path".into(),
            model_in.to_string_lossy().into_owned(),
            "--output_path".into(),
            model_out.to_string_lossy().into_owned(),
        ],
    )?;
    let points_ply = dataset_dir.join(POINTS_FILE);
    run_colmap(
        "model_converter",
        &[
            "model_converter".into(),
            "--input_path".into(),
            model_out.to_string_lossy().into_owned(),
            "--output_path".into(),
            points_ply.to_string_lossy().into_owned(),
            "--output_type".into(),
            "PLY".into(),
        ],
    )?;

    Ok(ply_vertex_count(&points_ply))
}

/// True when `colmap` resolves on `PATH` (the seed can run). Mirrors
/// [`crate::backends::is_tool_available`] but named for the seed's use.
pub fn colmap_available() -> bool {
    crate::backends::is_tool_available("colmap")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Multiply two row-major 3x3 matrices.
    fn matmul(a: &[[f64; 3]; 3], b: &[[f64; 3]; 3]) -> [[f64; 3]; 3] {
        let mut o = [[0.0; 3]; 3];
        for i in 0..3 {
            for j in 0..3 {
                for k in 0..3 {
                    o[i][j] += a[i][k] * b[k][j];
                }
            }
        }
        o
    }

    /// Reconstruct a rotation matrix from a quaternion (qw,qx,qy,qz) so a test can
    /// assert the quat encodes the intended rotation.
    fn quat_to_mat3(q: &[f64; 4]) -> [[f64; 3]; 3] {
        let [w, x, y, z] = *q;
        [
            [
                1.0 - 2.0 * (y * y + z * z),
                2.0 * (x * y - w * z),
                2.0 * (x * z + w * y),
            ],
            [
                2.0 * (x * y + w * z),
                1.0 - 2.0 * (x * x + z * z),
                2.0 * (y * z - w * x),
            ],
            [
                2.0 * (x * z - w * y),
                2.0 * (y * z + w * x),
                1.0 - 2.0 * (x * x + y * y),
            ],
        ]
    }

    fn approx(a: f64, b: f64) {
        assert!((a - b).abs() < 1e-9, "{a} != {b}");
    }

    #[test]
    fn identity_pose_maps_to_identity_world_to_camera() {
        // The persister flips an OpenCV identity c2w to diag(1,-1,-1) with zero
        // translation. Converting back must give an identity world-to-camera
        // rotation (transpose of identity) and zero translation.
        let m = [
            [1.0, 0.0, 0.0, 0.0],
            [0.0, -1.0, 0.0, 0.0],
            [0.0, 0.0, -1.0, 0.0],
            [0.0, 0.0, 0.0, 1.0],
        ];
        let (q, t) = opengl_c2w_to_colmap_w2c(&m);
        // R_c2w = identity → R_w2c = identity → quaternion is identity.
        let r = quat_to_mat3(&q);
        for i in 0..3 {
            for j in 0..3 {
                approx(r[i][j], if i == j { 1.0 } else { 0.0 });
            }
        }
        approx(t[0], 0.0);
        approx(t[1], 0.0);
        approx(t[2], 0.0);
    }

    #[test]
    fn translation_is_minus_r_w2c_times_camera_center() {
        // An OpenCV c2w with identity rotation and camera center C=(1,2,3). The
        // persister's OpenGL matrix negates the Y/Z rotation columns (still
        // identity-shaped here) and keeps the position. World-to-camera translation
        // must be -R_w2c * C = -C for identity rotation.
        let m = [
            [1.0, 0.0, 0.0, 1.0],
            [0.0, -1.0, 0.0, 2.0],
            [0.0, 0.0, -1.0, 3.0],
            [0.0, 0.0, 0.0, 1.0],
        ];
        let (_q, t) = opengl_c2w_to_colmap_w2c(&m);
        approx(t[0], -1.0);
        approx(t[1], -2.0);
        approx(t[2], -3.0);
    }

    #[test]
    fn round_trips_a_general_rotation_through_the_persister_flip() {
        // Start from a real OpenCV c2w rotation (a 90° yaw about the camera's Y),
        // apply the SAME flip the persister applies (negate Y/Z columns), then run
        // the seed conversion. The recovered world-to-camera rotation must be the
        // transpose of the original c2w rotation.
        let r_c2w = [[0.0, 0.0, 1.0], [0.0, 1.0, 0.0], [-1.0, 0.0, 0.0]];
        // Persister flip: negate columns 1 and 2 → the OpenGL matrix rotation.
        let m = [
            [r_c2w[0][0], -r_c2w[0][1], -r_c2w[0][2], 5.0],
            [r_c2w[1][0], -r_c2w[1][1], -r_c2w[1][2], 6.0],
            [r_c2w[2][0], -r_c2w[2][1], -r_c2w[2][2], 7.0],
            [0.0, 0.0, 0.0, 1.0],
        ];
        let (q, t) = opengl_c2w_to_colmap_w2c(&m);
        let r_w2c = quat_to_mat3(&q);
        // R_w2c must equal transpose(R_c2w): R_w2c * R_c2w = identity.
        let prod = matmul(&r_w2c, &r_c2w);
        for i in 0..3 {
            for j in 0..3 {
                approx(prod[i][j], if i == j { 1.0 } else { 0.0 });
            }
        }
        // t = -R_w2c * C, C = (5,6,7).
        let expect = [
            -(r_w2c[0][0] * 5.0 + r_w2c[0][1] * 6.0 + r_w2c[0][2] * 7.0),
            -(r_w2c[1][0] * 5.0 + r_w2c[1][1] * 6.0 + r_w2c[1][2] * 7.0),
            -(r_w2c[2][0] * 5.0 + r_w2c[2][1] * 6.0 + r_w2c[2][2] * 7.0),
        ];
        for k in 0..3 {
            approx(t[k], expect[k]);
        }
    }

    #[test]
    fn quat_is_a_unit_quaternion() {
        let m = [
            [0.36, 0.48, -0.8, 1.0],
            [-0.8, 0.6, 0.0, 2.0],
            [0.48, 0.64, 0.6, 3.0],
            [0.0, 0.0, 0.0, 1.0],
        ];
        let (q, _t) = opengl_c2w_to_colmap_w2c(&m);
        let n = (q[0] * q[0] + q[1] * q[1] + q[2] * q[2] + q[3] * q[3]).sqrt();
        approx(n, 1.0);
    }

    #[test]
    fn image_name_is_the_basename() {
        assert_eq!(image_name("images/12.jpg"), "12.jpg");
        assert_eq!(image_name("0.jpg"), "0.jpg");
        assert_eq!(image_name("a/b/c/frame_009.png"), "frame_009.png");
    }

    #[test]
    fn parses_scene_level_and_per_frame_intrinsics_with_distortion() {
        let manifest = serde_json::json!({
            "camera_model": "OPENCV",
            "fl_x": 900.0, "fl_y": 900.0, "cx": 640.0, "cy": 360.0,
            "w": 1280, "h": 720, "k1": 0.1, "k2": -0.2, "p1": 0.001, "p2": 0.002,
            "frames": [
                { "file_path": "images/0.jpg",
                  "transform_matrix": [[1,0,0,0],[0,-1,0,0],[0,0,-1,0],[0,0,0,1]] },
                // Per-frame override: a second camera with a different focal length.
                { "file_path": "images/1.jpg", "fl_x": 700.0, "fl_y": 700.0,
                  "transform_matrix": [[1,0,0,1],[0,-1,0,2],[0,0,-1,3],[0,0,0,1]] }
            ]
        });
        let views = parse_transforms(&manifest).unwrap();
        assert_eq!(views.len(), 2);
        assert_eq!(views[0].name, "0.jpg");
        assert_eq!(views[0].fx, 900.0);
        assert_eq!(views[0].distortion, Some([0.1, -0.2, 0.001, 0.002]));
        // The second frame overrides fl_x but inherits scene cx/cy + distortion.
        assert_eq!(views[1].fx, 700.0);
        assert_eq!(views[1].cx, 640.0);
        assert_eq!(views[1].distortion, Some([0.1, -0.2, 0.001, 0.002]));
    }

    #[test]
    fn a_pinhole_manifest_omits_distortion() {
        let manifest = serde_json::json!({
            "fl_x": 800.0, "cx": 400.0, "cy": 300.0, "w": 800, "h": 600,
            "frames": [ { "file_path": "images/0.jpg",
                "transform_matrix": [[1,0,0,0],[0,-1,0,0],[0,0,-1,0],[0,0,0,1]] } ]
        });
        let views = parse_transforms(&manifest).unwrap();
        assert_eq!(views[0].distortion, None);
        // fl_y falls back to fl_x when the manifest omits it.
        assert_eq!(views[0].fy, 800.0);
    }

    #[test]
    fn cameras_txt_picks_opencv_or_pinhole_per_view() {
        let views = parse_transforms(&serde_json::json!({
            "fl_x": 800.0, "cx": 400.0, "cy": 300.0, "w": 800, "h": 600,
            "frames": [
                { "file_path": "0.jpg", "k1": 0.1, "k2": 0.2, "p1": 0.0, "p2": 0.0,
                  "transform_matrix": [[1,0,0,0],[0,-1,0,0],[0,0,-1,0],[0,0,0,1]] },
                { "file_path": "1.jpg",
                  "transform_matrix": [[1,0,0,0],[0,-1,0,0],[0,0,-1,0],[0,0,0,1]] }
            ]
        }))
        .unwrap();
        let txt = cameras_txt(&views);
        assert!(txt.contains("1 OPENCV 800 600 800 800 400 300 0.1 0.2 0 0"));
        assert!(txt.contains("2 PINHOLE 800 600 800 800 400 300"));
    }

    #[test]
    fn images_txt_has_pose_line_then_empty_observation_line() {
        let views = vec![SeedView {
            name: "7.jpg".into(),
            w: 8,
            h: 6,
            fx: 8.0,
            fy: 8.0,
            cx: 4.0,
            cy: 3.0,
            distortion: None,
            quat: [1.0, 0.0, 0.0, 0.0],
            trans: [1.0, 2.0, 3.0],
        }];
        let txt = images_txt(&views);
        // IMAGE_ID QW QX QY QZ TX TY TZ CAMERA_ID NAME, then a blank line.
        assert!(txt.contains("1 1 0 0 0 1 2 3 1 7.jpg\n\n"));
    }

    #[test]
    fn ply_vertex_count_reads_the_header() {
        let ply = "ply\nformat ascii 1.0\nelement vertex 1234\nproperty float x\nend_header\n";
        assert_eq!(parse_ply_vertex_count(ply), 1234);
        assert_eq!(parse_ply_vertex_count("ply\nend_header\n"), 0);
        // A count after end_header is ignored (malformed).
        assert_eq!(
            parse_ply_vertex_count("ply\nend_header\nelement vertex 9\n"),
            0
        );
    }

    #[test]
    fn a_manifest_without_frames_is_a_manifest_error() {
        let err = parse_transforms(&serde_json::json!({ "fl_x": 1.0 })).unwrap_err();
        assert!(matches!(err, SeedError::Manifest(_)));
    }

    #[test]
    fn existing_usable_seed_short_circuits() {
        let dir = tempfile::tempdir().unwrap();
        // A points3D.ply with enough vertices already sits next to the (absent)
        // manifest → seed_points returns its count without touching COLMAP.
        std::fs::write(
            dir.path().join(POINTS_FILE),
            "ply\nformat ascii 1.0\nelement vertex 500\nend_header\n",
        )
        .unwrap();
        let n = seed_points(dir.path(), &serde_json::json!({})).unwrap();
        assert_eq!(n, 500);
    }

    #[test]
    fn a_too_sparse_existing_seed_does_not_short_circuit() {
        let dir = tempfile::tempdir().unwrap();
        // Below MIN_SEED_POINTS → not treated as usable; with no manifest present
        // the seed then fails at the manifest read (the caller falls back).
        std::fs::write(
            dir.path().join(POINTS_FILE),
            "ply\nelement vertex 4\nend_header\n",
        )
        .unwrap();
        let err = seed_points(dir.path(), &serde_json::json!({})).unwrap_err();
        assert!(matches!(err, SeedError::Manifest(_)));
    }
}
