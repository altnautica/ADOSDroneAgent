//! Native point-cloud seed for the gaussian-splat trainer.
//!
//! The native-Metal trainer (msplat) and the portable trainer (Brush) both
//! INITIALIZE from a `points3D.ply` sitting next to the Nerfstudio
//! `transforms.json`, loaded in the SAME world frame as the camera poses — by
//! each trainer's OWN reader, with NO COLMAP binary involved. So seeding reduces
//! to writing a plausible point cloud co-framed with our poses. We do NOT run
//! COLMAP structure-from-motion: COLMAP 3.12+ introduced a rig/frame/sensor model
//! that makes the classic known-pose triangulation recipe fragile and
//! version-divergent (brew-4.x enforces it, apt-3.x does not), and it is
//! unnecessary because we already have accurate FC-GPS/VIO-fused poses.
//!
//! This module writes a uniform-random cloud filling the box the cameras
//! surround. It is deterministic (seeded) and dependency-free. Crucially it is
//! built from the camera CENTERS (the pose translation, which is INVARIANT to the
//! OpenGL/OpenCV basis convention), so the cloud is co-framed with the poses on
//! either convention — there is no coordinate flip to get wrong. This is a
//! measured floor for gaussian-splat init: with accurate poses + many views +
//! densification it trains to near-SfM quality (arXiv:2404.12547, which motivates
//! exactly the fused-inertial/GPS-pose case), and it always works. A
//! monocular-depth or triangulated seed is a quality follow-up.

use std::path::Path;

/// The Nerfstudio manifest a captured dataset carries (written by the keyframe
/// persister).
const TRANSFORMS_FILE: &str = "transforms.json";
/// The seed point cloud filename each trainer's reader auto-discovers next to the
/// manifest.
const POINTS_FILE: &str = "points3D.ply";
/// Default seed point count — enough to give densification a dense start without
/// bloating load. Overridable via the `seed_points` job param.
const DEFAULT_SEED_POINTS: u64 = 80_000;
/// Default box inflation about the camera-center span, so the cloud covers the
/// scene the cameras surround (not just the flight path). Overridable via
/// `seed_inflate`.
const DEFAULT_INFLATE: f64 = 1.5;
/// A deterministic seed for the PRNG, so the same dataset yields the same cloud.
const RNG_SEED: u64 = 0x5EED_A110_C0DE_1234;

/// Fewer points than this and the seed is treated as unusable (the caller trains
/// with the random-init trainer instead). The generator far exceeds it; the floor
/// only guards a degenerate manifest.
pub const MIN_SEED_POINTS: u64 = 32;

/// Why a seed could not be produced. Every variant means "fall back to the
/// random-init trainer (Brush)", never "abort the job".
#[derive(Debug)]
pub enum SeedError {
    /// The manifest was missing, unreadable, not valid JSON, or had too few poses.
    Manifest(String),
    /// A filesystem fault reading the manifest or writing the cloud.
    Io(std::io::Error),
}

impl std::fmt::Display for SeedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SeedError::Manifest(m) => write!(f, "seed manifest: {m}"),
            SeedError::Io(e) => write!(f, "seed io: {e}"),
        }
    }
}

impl From<std::io::Error> for SeedError {
    fn from(e: std::io::Error) -> Self {
        SeedError::Io(e)
    }
}

/// A tiny deterministic PRNG (xorshift64*), so the seed cloud is reproducible
/// without a `rand`-crate dependency.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Rng(seed | 1)
    }
    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    /// A uniform f64 in `[0, 1)`.
    fn next_f64(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }
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

/// The camera CENTERS (the translation column of each frame's camera-to-world
/// matrix) from a Nerfstudio `transforms.json`. The center is invariant to the
/// OpenGL↔OpenCV basis flip, so a cloud built from these is co-framed with the
/// poses on either convention.
fn camera_centers(manifest: &serde_json::Value) -> Vec<[f64; 3]> {
    manifest
        .get("frames")
        .and_then(|v| v.as_array())
        .map(|frames| {
            frames
                .iter()
                .filter_map(|f| {
                    let m = f.get("transform_matrix").and_then(parse_matrix4)?;
                    Some([m[0][3], m[1][3], m[2][3]])
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The inflated axis-aligned bounding box (min, max) of a set of points: each
/// axis is expanded about its center by `inflate`, with a floor on the half-extent
/// so a nearly-planar orbit still gets depth. Guards a degenerate (single-point)
/// set with a unit box.
fn inflated_aabb(centers: &[[f64; 3]], inflate: f64) -> ([f64; 3], [f64; 3]) {
    let mut min = [f64::INFINITY; 3];
    let mut max = [f64::NEG_INFINITY; 3];
    for c in centers {
        for a in 0..3 {
            min[a] = min[a].min(c[a]);
            max[a] = max[a].max(c[a]);
        }
    }
    // Largest axis span → a floor on every half-extent (so a planar/collinear
    // camera path still yields a 3D box, and a single point yields a unit box).
    let span = (0..3).map(|a| max[a] - min[a]).fold(0.0_f64, f64::max);
    let floor = if span.is_finite() && span > 0.0 {
        span * 0.05
    } else {
        1.0
    };
    let mut lo = [0.0; 3];
    let mut hi = [0.0; 3];
    for a in 0..3 {
        let center = (min[a] + max[a]) / 2.0;
        let half = ((max[a] - min[a]) / 2.0).max(floor) * inflate;
        lo[a] = center - half;
        hi[a] = center + half;
    }
    (lo, hi)
}

/// Serialize a binary-little-endian PLY of `n` uniform-random points in the box
/// `[lo, hi]` (x/y/z float32; msplat/Brush default the color to mid-gray).
/// Deterministic given `seed`.
fn random_ply_bytes(lo: [f64; 3], hi: [f64; 3], n: u64, seed: u64) -> Vec<u8> {
    let mut rng = Rng::new(seed);
    let header = format!(
        "ply\nformat binary_little_endian 1.0\nelement vertex {n}\nproperty float x\nproperty float y\nproperty float z\nend_header\n"
    );
    let mut buf = Vec::with_capacity(header.len() + (n as usize) * 12);
    buf.extend_from_slice(header.as_bytes());
    for _ in 0..n {
        for a in 0..3 {
            let t = rng.next_f64();
            let v = (lo[a] + t * (hi[a] - lo[a])) as f32;
            buf.extend_from_slice(&v.to_le_bytes());
        }
    }
    buf
}

/// Read the vertex count out of a PLY header (`element vertex <n>`), enough to
/// gate the seed on the point count without parsing the cloud.
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

/// The vertex count of a PLY file from its header, 0 on any read error / missing
/// count.
fn ply_vertex_count(path: &Path) -> u64 {
    let Ok(bytes) = std::fs::read(path) else {
        return 0;
    };
    let head = String::from_utf8_lossy(&bytes[..bytes.len().min(4096)]);
    parse_ply_vertex_count(&head)
}

/// Whether a usable seed already sits next to the manifest (an idempotent re-run
/// or an externally-provided cloud), so regeneration is skipped.
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

/// Produce `<dataset_dir>/points3D.ply` — a uniform-random point cloud filling the
/// box the cameras surround, co-framed with the manifest's poses — for the
/// gaussian-splat trainer to initialize from. Returns the point count. Idempotent:
/// an existing usable `points3D.ply` is returned without regenerating.
///
/// A `SeedError` (no manifest, fewer than two poses) is the caller's signal to
/// train with the random-init trainer (Brush) instead.
pub fn seed_points(dataset_dir: &Path, params: &serde_json::Value) -> Result<u64, SeedError> {
    if let Some(n) = existing_seed(dataset_dir) {
        return Ok(n);
    }
    let manifest_path = dataset_dir.join(TRANSFORMS_FILE);
    let text = std::fs::read_to_string(&manifest_path)
        .map_err(|e| SeedError::Manifest(format!("read {}: {e}", manifest_path.display())))?;
    let manifest: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| SeedError::Manifest(format!("parse: {e}")))?;

    let centers = camera_centers(&manifest);
    if centers.len() < 2 {
        return Err(SeedError::Manifest(format!(
            "need >= 2 camera poses to seed, found {}",
            centers.len()
        )));
    }

    let inflate = params
        .get("seed_inflate")
        .and_then(|v| v.as_f64())
        .filter(|v| v.is_finite() && *v > 0.0)
        .unwrap_or(DEFAULT_INFLATE);
    let n = params
        .get("seed_points")
        .and_then(|v| v.as_u64())
        .filter(|v| *v >= MIN_SEED_POINTS)
        .unwrap_or(DEFAULT_SEED_POINTS);

    let (lo, hi) = inflated_aabb(&centers, inflate);
    let path = dataset_dir.join(POINTS_FILE);
    std::fs::write(&path, random_ply_bytes(lo, hi, n, RNG_SEED))?;
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest(centers: &[[f64; 3]]) -> serde_json::Value {
        let frames: Vec<_> = centers
            .iter()
            .enumerate()
            .map(|(i, c)| {
                serde_json::json!({
                    "file_path": format!("images/{i}.jpg"),
                    "transform_matrix": [
                        [1.0, 0.0, 0.0, c[0]],
                        [0.0, -1.0, 0.0, c[1]],
                        [0.0, 0.0, -1.0, c[2]],
                        [0.0, 0.0, 0.0, 1.0],
                    ],
                })
            })
            .collect();
        serde_json::json!({ "frames": frames })
    }

    #[test]
    fn camera_centers_reads_the_translation_column() {
        let m = manifest(&[[1.0, 2.0, 3.0], [4.0, 5.0, 6.0]]);
        assert_eq!(camera_centers(&m), vec![[1.0, 2.0, 3.0], [4.0, 5.0, 6.0]]);
        // No frames → empty.
        assert!(camera_centers(&serde_json::json!({})).is_empty());
    }

    #[test]
    fn inflated_aabb_covers_and_inflates_the_center_span() {
        // Centers span [0,10] on x, [0,0] on y (planar), [0,2] on z.
        let centers = [[0.0, 0.0, 0.0], [10.0, 0.0, 2.0]];
        let (lo, hi) = inflated_aabb(&centers, 1.5);
        // x: center 5, half 5*1.5=7.5 → [-2.5, 12.5].
        assert!((lo[0] - (-2.5)).abs() < 1e-9);
        assert!((hi[0] - 12.5).abs() < 1e-9);
        // y is degenerate (span 0) → gets the floor (5% of the 10 span = 0.5),
        // inflated 1.5 → half 0.75 → [-0.75, 0.75], never a zero-thickness slab.
        assert!(hi[1] > lo[1]);
        assert!((hi[1] - lo[1]) > 0.5);
    }

    #[test]
    fn a_single_or_no_center_still_yields_a_finite_box() {
        let (lo, hi) = inflated_aabb(&[[5.0, 5.0, 5.0]], 1.5);
        for a in 0..3 {
            assert!(hi[a] > lo[a], "axis {a} must be non-degenerate");
            assert!(lo[a].is_finite() && hi[a].is_finite());
        }
    }

    #[test]
    fn random_ply_is_a_valid_binary_ply_with_points_in_the_box() {
        let lo = [-1.0, -2.0, -3.0];
        let hi = [1.0, 2.0, 3.0];
        let bytes = random_ply_bytes(lo, hi, 500, RNG_SEED);
        // Header parses to the right count.
        let head = String::from_utf8_lossy(&bytes[..bytes.len().min(4096)]);
        assert_eq!(parse_ply_vertex_count(&head), 500);
        assert!(head.contains("format binary_little_endian 1.0"));
        // Body is exactly 500 * 3 * 4 bytes after the header.
        let header_len = head.find("end_header\n").unwrap() + "end_header\n".len();
        assert_eq!(bytes.len() - header_len, 500 * 12);
        // Every point is inside the box (read the first few floats back).
        let body = &bytes[header_len..];
        for p in 0..500 {
            for a in 0..3 {
                let off = p * 12 + a * 4;
                let v = f32::from_le_bytes([body[off], body[off + 1], body[off + 2], body[off + 3]])
                    as f64;
                assert!(
                    v >= lo[a] - 1e-4 && v <= hi[a] + 1e-4,
                    "point {p} axis {a} = {v}"
                );
            }
        }
    }

    #[test]
    fn rng_is_deterministic() {
        let a = random_ply_bytes([0.0; 3], [1.0; 3], 100, RNG_SEED);
        let b = random_ply_bytes([0.0; 3], [1.0; 3], 100, RNG_SEED);
        assert_eq!(a, b, "same seed → identical cloud");
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
    fn seed_points_writes_a_cloud_and_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(TRANSFORMS_FILE),
            serde_json::to_vec(&manifest(&[
                [0.0, 0.0, 0.0],
                [1.0, 0.0, 0.0],
                [0.0, 1.0, 0.0],
                [1.0, 1.0, 1.0],
            ]))
            .unwrap(),
        )
        .unwrap();
        // A bounded count for the test.
        let params = serde_json::json!({ "seed_points": 1000 });
        let n = seed_points(dir.path(), &params).unwrap();
        assert_eq!(n, 1000);
        let ply = dir.path().join(POINTS_FILE);
        assert!(ply.is_file());
        assert_eq!(ply_vertex_count(&ply), 1000);
        // A re-run finds the existing usable cloud and returns it without rewriting
        // (idempotent), even with different params.
        let n2 = seed_points(dir.path(), &serde_json::json!({ "seed_points": 5000 })).unwrap();
        assert_eq!(n2, 1000);
    }

    #[test]
    fn seed_points_needs_at_least_two_poses() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(TRANSFORMS_FILE),
            serde_json::to_vec(&manifest(&[[0.0, 0.0, 0.0]])).unwrap(),
        )
        .unwrap();
        let err = seed_points(dir.path(), &serde_json::json!({})).unwrap_err();
        assert!(matches!(err, SeedError::Manifest(_)));
    }

    #[test]
    fn a_missing_manifest_is_a_manifest_error() {
        let dir = tempfile::tempdir().unwrap();
        let err = seed_points(dir.path(), &serde_json::json!({})).unwrap_err();
        assert!(matches!(err, SeedError::Manifest(_)));
    }

    #[test]
    fn existing_usable_seed_short_circuits() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(POINTS_FILE),
            "ply\nformat ascii 1.0\nelement vertex 500\nend_header\n",
        )
        .unwrap();
        // Returns the existing count without needing a manifest.
        let n = seed_points(dir.path(), &serde_json::json!({})).unwrap();
        assert_eq!(n, 500);
    }
}
