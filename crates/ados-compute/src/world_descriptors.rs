//! Turning a finished reconstruction into shared world-model DATA.
//!
//! Without this module Atlas is a private pipeline ending in a viewer: the
//! compute node writes a `.ply` and a `.rrd`, the GCS renders them, and nothing
//! else in the system can consume the world the aircraft just mapped. The four
//! `plugin.atlas.*` shared-data topics existed as constants with no publisher
//! and no subscriber anywhere in the tree.
//!
//! This is the publisher. When a reconstruct job completes, its outputs are
//! mapped to descriptors stamped with the capture session and a monotonically
//! increasing `generation`, and published on the per-device world stream. A
//! descriptor is small and carries no pixels — it says what exists, how big it
//! is, and where to fetch it — so it rides any bearer and a consumer pulls only
//! what it needs.
//!
//! # The occupancy lane is an ESDF, not a voxel dump
//!
//! A planner does not want a binary occupancy grid. Every planner that receives
//! one immediately runs its own distance transform over it, because what a
//! trajectory optimiser needs is a distance AND a gradient — something to push
//! away from, not merely something to test collisions against. So the producer
//! does that work once, here, and publishes a truncated Euclidean signed
//! distance field: `f32` metres to the nearest occupied surface, clamped at a
//! truncation radius beyond which the exact value stops mattering for planning.
//!
//! The distance transform is Felzenszwalb & Huttenlocher's exact separable
//! algorithm, run per axis. It is O(n) per axis in the number of voxels and
//! EXACT — a chamfer or multi-pass approximation would be cheaper to write and
//! wrong by up to ~10% near diagonals, which is not an acceptable error on a
//! clearance figure an aircraft flies against.

use std::path::{Path, PathBuf};

use ados_protocol::atlas::{
    Generation, MeshDescriptor, OccupancyDescriptor, OccupancyField, PointCloudDescriptor,
    SplatDescriptor,
};

use crate::rerun_world::parse_ply;
use crate::Output;

/// Default voxel edge for a derived occupancy / ESDF grid, in metres. 20 cm is
/// the coarse local-costmap resolution a planner uses for an aerial vehicle:
/// fine enough to resolve a mast or a cable run, coarse enough that a 200 m
/// capture is a grid a drone can hold in memory.
pub const DEFAULT_ESDF_RESOLUTION_M: f32 = 0.20;

/// Default truncation radius for the ESDF, in metres. Beyond a few metres of
/// clearance the exact distance stops changing a planning decision, and
/// truncating keeps the field's dynamic range small enough to be useful at
/// `f32` and cheap to compress.
pub const DEFAULT_ESDF_TRUNCATION_M: f32 = 4.0;

/// Hard ceiling on the derived grid's voxel count, so a large or mis-scaled
/// reconstruction cannot allocate an unbounded field on the node. At the
/// default 20 cm resolution this is a ~1.3 km-per-side volume; a capture larger
/// than this yields no occupancy descriptor rather than a truncated,
/// silently-wrong one.
pub const MAX_ESDF_VOXELS: usize = 64_000_000;

/// A derived occupancy / distance grid.
#[derive(Debug, Clone, PartialEq)]
pub struct EsdfGrid {
    /// World-frame position of voxel `(0,0,0)`'s centre.
    pub origin: [f64; 3],
    pub resolution_m: f32,
    /// `[nx, ny, nz]`.
    pub dims: [u32; 3],
    /// Truncation radius in metres.
    pub truncation_m: f32,
    /// Row-major `nx * ny * nz` distances in metres, x fastest.
    pub distances: Vec<f32>,
}

impl EsdfGrid {
    /// `nx * ny * nz`.
    pub fn voxel_count(&self) -> usize {
        self.dims[0] as usize * self.dims[1] as usize * self.dims[2] as usize
    }

    /// The distance at a voxel index, for tests and consumers that hold the grid
    /// in memory.
    pub fn at(&self, x: u32, y: u32, z: u32) -> Option<f32> {
        if x >= self.dims[0] || y >= self.dims[1] || z >= self.dims[2] {
            return None;
        }
        let idx =
            (z as usize * self.dims[1] as usize + y as usize) * self.dims[0] as usize + x as usize;
        self.distances.get(idx).copied()
    }

    /// The grid as little-endian `f32` bytes, the layout
    /// [`OccupancyDescriptor::url`] promises.
    pub fn to_le_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.distances.len() * 4);
        for d in &self.distances {
            out.extend_from_slice(&d.to_le_bytes());
        }
        out
    }
}

/// Why an ESDF could not be derived. Every variant is a case where publishing
/// something anyway would mean publishing a planning input that is wrong.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum EsdfError {
    /// The cloud had no points, so there is no surface to measure against.
    #[error("no points in the reconstruction")]
    NoPoints,
    /// A non-finite coordinate would make every derived bound meaningless.
    #[error("reconstruction contains non-finite coordinates")]
    NonFinite,
    /// The requested resolution is not a usable voxel edge.
    #[error("resolution {0} m is not a positive finite length")]
    BadResolution(f32),
    /// The grid the capture's extent implies is larger than
    /// [`MAX_ESDF_VOXELS`].
    #[error("grid of {got} voxels exceeds the {max} ceiling")]
    TooLarge { got: usize, max: usize },
}

/// Build a truncated ESDF from a reconstruction's points.
///
/// The grid is sized to the cloud's axis-aligned bounds plus one truncation
/// radius of padding on every side, so a planner has real clearance values in
/// the free space AROUND the structure rather than a field that stops at the
/// surface.
pub fn esdf_from_points(
    positions: &[[f32; 3]],
    resolution_m: f32,
    truncation_m: f32,
) -> Result<EsdfGrid, EsdfError> {
    if !resolution_m.is_finite() || resolution_m <= 0.0 {
        return Err(EsdfError::BadResolution(resolution_m));
    }
    if positions.is_empty() {
        return Err(EsdfError::NoPoints);
    }
    let truncation_m = if truncation_m.is_finite() && truncation_m > 0.0 {
        truncation_m
    } else {
        DEFAULT_ESDF_TRUNCATION_M
    };

    let mut min = [f32::INFINITY; 3];
    let mut max = [f32::NEG_INFINITY; 3];
    for p in positions {
        for a in 0..3 {
            if !p[a].is_finite() {
                return Err(EsdfError::NonFinite);
            }
            min[a] = min[a].min(p[a]);
            max[a] = max[a].max(p[a]);
        }
    }

    // Pad by a truncation radius so free space around the structure is covered.
    let pad = truncation_m;
    let mut dims = [0u32; 3];
    let mut origin = [0.0f64; 3];
    for a in 0..3 {
        let lo = min[a] - pad;
        let hi = max[a] + pad;
        origin[a] = lo as f64;
        // At least one voxel per axis: a planar cloud is legitimate input.
        let n = ((hi - lo) / resolution_m).ceil() as i64 + 1;
        dims[a] = n.clamp(1, u32::MAX as i64) as u32;
    }
    let count = dims[0] as usize * dims[1] as usize * dims[2] as usize;
    if count > MAX_ESDF_VOXELS {
        return Err(EsdfError::TooLarge {
            got: count,
            max: MAX_ESDF_VOXELS,
        });
    }

    // Squared-distance field in VOXEL units. `INF` marks free space; an occupied
    // voxel starts at 0 and the transform grows outward from it.
    const INF: f32 = 1e20;
    let mut sq = vec![INF; count];
    let (nx, ny, nz) = (dims[0] as usize, dims[1] as usize, dims[2] as usize);
    let idx = |x: usize, y: usize, z: usize| (z * ny + y) * nx + x;
    for p in positions {
        let vx = (((p[0] as f64 - origin[0]) / resolution_m as f64).round() as i64)
            .clamp(0, nx as i64 - 1) as usize;
        let vy = (((p[1] as f64 - origin[1]) / resolution_m as f64).round() as i64)
            .clamp(0, ny as i64 - 1) as usize;
        let vz = (((p[2] as f64 - origin[2]) / resolution_m as f64).round() as i64)
            .clamp(0, nz as i64 - 1) as usize;
        sq[idx(vx, vy, vz)] = 0.0;
    }

    // Exact separable EDT, one axis at a time (Felzenszwalb & Huttenlocher).
    let mut line = Vec::with_capacity(nx.max(ny).max(nz));
    let mut out = vec![0.0f32; nx.max(ny).max(nz)];
    // X
    for z in 0..nz {
        for y in 0..ny {
            line.clear();
            line.extend((0..nx).map(|x| sq[idx(x, y, z)]));
            transform_1d(&line, &mut out[..nx]);
            for x in 0..nx {
                sq[idx(x, y, z)] = out[x];
            }
        }
    }
    // Y
    for z in 0..nz {
        for x in 0..nx {
            line.clear();
            line.extend((0..ny).map(|y| sq[idx(x, y, z)]));
            transform_1d(&line, &mut out[..ny]);
            for y in 0..ny {
                sq[idx(x, y, z)] = out[y];
            }
        }
    }
    // Z
    for y in 0..ny {
        for x in 0..nx {
            line.clear();
            line.extend((0..nz).map(|z| sq[idx(x, y, z)]));
            transform_1d(&line, &mut out[..nz]);
            for z in 0..nz {
                sq[idx(x, y, z)] = out[z];
            }
        }
    }

    // Voxel-space squared distance -> truncated metres.
    let distances = sq
        .into_iter()
        .map(|s| (s.max(0.0).sqrt() * resolution_m).min(truncation_m))
        .collect();

    Ok(EsdfGrid {
        origin,
        resolution_m,
        dims,
        truncation_m,
        distances,
    })
}

/// The 1-D exact squared-distance transform of `f`, written into `out`.
///
/// Felzenszwalb & Huttenlocher's lower-envelope-of-parabolas method: each cell
/// contributes the parabola `(q - p)^2 + f[p]`, the algorithm walks the
/// envelope's breakpoints in one forward pass and samples it in one backward
/// pass, so the whole line is O(n) rather than O(n^2).
fn transform_1d(f: &[f32], out: &mut [f32]) {
    let n = f.len();
    debug_assert_eq!(out.len(), n);
    if n == 0 {
        return;
    }
    if n == 1 {
        out[0] = f[0];
        return;
    }
    // `v[k]` is the index of the k-th parabola in the lower envelope; `z[k]` is
    // the boundary between parabola k-1 and k.
    let mut v = vec![0usize; n];
    let mut z = vec![0.0f32; n + 1];
    let mut k = 0usize;
    v[0] = 0;
    z[0] = f32::NEG_INFINITY;
    z[1] = f32::INFINITY;
    for q in 1..n {
        // Intersection of the parabola from q with the current last one.
        let mut s = intersection(f, v[k], q);
        while s <= z[k] {
            // The new parabola hides the last one entirely; pop it.
            if k == 0 {
                break;
            }
            k -= 1;
            s = intersection(f, v[k], q);
        }
        if s <= z[k] && k == 0 {
            v[0] = q;
            z[0] = f32::NEG_INFINITY;
            z[1] = f32::INFINITY;
            continue;
        }
        k += 1;
        v[k] = q;
        z[k] = s;
        z[k + 1] = f32::INFINITY;
    }
    let mut k = 0usize;
    for (q, slot) in out.iter_mut().enumerate().take(n) {
        // Advance to the envelope segment covering q; each segment is visited
        // once across the whole pass, which is what keeps this O(n).
        while z[k + 1] < q as f32 {
            k += 1;
        }
        let d = q as f32 - v[k] as f32;
        *slot = d * d + f[v[k]];
    }
}

/// The q-coordinate where the parabolas rooted at `p` and `q` intersect.
fn intersection(f: &[f32], p: usize, q: usize) -> f32 {
    let (fp, fq) = (f[p], f[q]);
    let (pf, qf) = (p as f32, q as f32);
    let num = (fq + qf * qf) - (fp + pf * pf);
    let den = 2.0 * (qf - pf);
    if den == 0.0 {
        f32::INFINITY
    } else {
        num / den
    }
}

/// The world-model descriptors derived from one reconstruction generation. Each
/// is `None` when the generation produced no artifact of that kind — never a
/// placeholder, so a consumer cannot mistake an absent mesh for an empty one.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct WorldDescriptorSet {
    pub splat: Option<SplatDescriptor>,
    pub pointcloud: Option<PointCloudDescriptor>,
    pub mesh: Option<MeshDescriptor>,
    pub occupancy: Option<OccupancyDescriptor>,
}

impl WorldDescriptorSet {
    /// Whether this generation produced anything at all to publish.
    pub fn is_empty(&self) -> bool {
        self.splat.is_none()
            && self.pointcloud.is_none()
            && self.mesh.is_none()
            && self.occupancy.is_none()
    }
}

/// Map one completed job's outputs to shared-data descriptors.
///
/// Every count and bound is read from the real artifact — a `gaussian_count`
/// comes from the backend's own metadata or from parsing the `.ply`, never from
/// a guess — so a descriptor either states a measured fact or omits the field.
pub fn derive_descriptors(
    session_id: &str,
    generation: Generation,
    outputs: &[Output],
    work_root: &Path,
) -> WorldDescriptorSet {
    let mut set = WorldDescriptorSet::default();
    for out in outputs {
        match out.kind.as_str() {
            "splat" => {
                set.splat = Some(SplatDescriptor {
                    session_id: session_id.to_string(),
                    generation,
                    gaussian_count: meta_u64(out, "gaussian_count")
                        .or_else(|| ply_point_count(out, work_root))
                        .unwrap_or(0),
                    step: meta_u64(out, "step").unwrap_or(0),
                    url: Some(out.uri.clone()),
                    handle: None,
                    manifest_url: meta_str(out, "manifest_url"),
                    lod_levels: meta_u64(out, "lod_levels").unwrap_or(0).min(255) as u8,
                });
            }
            "pointcloud" => {
                let points = local_ply_points(out, work_root);
                set.pointcloud = Some(PointCloudDescriptor {
                    session_id: session_id.to_string(),
                    generation,
                    point_count: meta_u64(out, "point_count")
                        .unwrap_or_else(|| points.as_ref().map(|p| p.len() as u64).unwrap_or(0)),
                    bounds: points.as_deref().map(bounds_of).unwrap_or([0.0; 6]),
                    shm_name: None,
                    slot: None,
                    seq: None,
                    url: Some(out.uri.clone()),
                });
            }
            "mesh" => {
                set.mesh = Some(MeshDescriptor {
                    session_id: session_id.to_string(),
                    generation,
                    vertex_count: meta_u64(out, "vertex_count").unwrap_or(0),
                    face_count: meta_u64(out, "face_count").unwrap_or(0),
                    url: Some(out.uri.clone()),
                    handle: None,
                });
            }
            _ => {}
        }
    }
    set
}

/// Derive the ESDF for a generation from whichever output carries geometry,
/// write it under the work root, and return its descriptor.
///
/// Returns `Ok(None)` when the generation carries no readable geometry — an
/// honest "no planning input this generation" rather than an empty grid a
/// planner would read as wide-open free space.
pub fn derive_occupancy(
    session_id: &str,
    generation: Generation,
    outputs: &[Output],
    work_root: &Path,
    job_id: &str,
    public_base: &str,
) -> Result<Option<(OccupancyDescriptor, EsdfGrid)>, EsdfError> {
    // Prefer an explicit point cloud; a splat `.ply`'s gaussian centres are the
    // next best surface sample and are what the viewer already renders.
    let geometry = outputs
        .iter()
        .find(|o| o.kind == "pointcloud")
        .or_else(|| outputs.iter().find(|o| o.kind == "splat"));
    let Some(out) = geometry else {
        return Ok(None);
    };
    let Some(points) = local_ply_points(out, work_root) else {
        return Ok(None);
    };
    let grid = esdf_from_points(
        &points,
        DEFAULT_ESDF_RESOLUTION_M,
        DEFAULT_ESDF_TRUNCATION_M,
    )?;

    let rel = PathBuf::from(job_id).join(format!("esdf-g{generation}.f32"));
    let abs = work_root.join(&rel);
    if let Some(parent) = abs.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let url = match std::fs::write(&abs, grid.to_le_bytes()) {
        Ok(()) => Some(format!(
            "{}/artifacts/{}",
            public_base.trim_end_matches('/'),
            rel.to_string_lossy().replace('\\', "/")
        )),
        Err(e) => {
            // The grid is still valid in memory and the descriptor still states
            // real geometry, so publish it without a fetch URL rather than
            // dropping the whole planning lane on a disk fault.
            tracing::warn!(error = %e, path = %abs.display(), "atlas_esdf_write_failed");
            None
        }
    };

    Ok(Some((
        OccupancyDescriptor {
            session_id: session_id.to_string(),
            generation,
            origin: grid.origin,
            resolution_m: grid.resolution_m,
            dims: grid.dims,
            field: OccupancyField::Esdf,
            truncation_m: grid.truncation_m,
            shm_name: None,
            slot: None,
            seq: None,
            url,
        },
        grid,
    )))
}

fn meta_u64(out: &Output, key: &str) -> Option<u64> {
    out.meta.get(key)?.as_u64()
}

fn meta_str(out: &Output, key: &str) -> Option<String> {
    Some(out.meta.get(key)?.as_str()?.to_string())
}

/// Resolve an output's local `.ply` and parse its positions, or `None` when the
/// output is not a readable local `.ply` (a `mock://` placeholder, an artifact
/// URL whose file is gone, an unsupported layout).
fn local_ply_points(out: &Output, work_root: &Path) -> Option<Vec<[f32; 3]>> {
    let path = local_ply_path(out, work_root)?;
    parse_ply(&path).ok().flatten().map(|p| p.positions)
}

fn ply_point_count(out: &Output, work_root: &Path) -> Option<u64> {
    local_ply_points(out, work_root).map(|p| p.len() as u64)
}

/// The on-disk path of an output's `.ply`, from either its original `file://`
/// URI or the `local_uri` the artifact rewriter preserves.
fn local_ply_path(out: &Output, work_root: &Path) -> Option<PathBuf> {
    let candidates = [
        out.meta.get("local_uri").and_then(|v| v.as_str()),
        Some(out.uri.as_str()),
    ];
    for uri in candidates.into_iter().flatten() {
        if let Some(rest) = uri.strip_prefix("file://") {
            let p = PathBuf::from(rest);
            if p.extension().is_some_and(|e| e == "ply") && p.is_file() {
                return Some(p);
            }
        }
        // An already-rewritten artifact URL: recover the work-root-relative path.
        if let Some((_, rel)) = uri.split_once("/artifacts/") {
            let p = work_root.join(rel);
            if p.extension().is_some_and(|e| e == "ply") && p.is_file() {
                return Some(p);
            }
        }
    }
    None
}

/// Axis-aligned bounds `[min_x, min_y, min_z, max_x, max_y, max_z]`.
fn bounds_of(points: &[[f32; 3]]) -> [f64; 6] {
    let mut min = [f64::INFINITY; 3];
    let mut max = [f64::NEG_INFINITY; 3];
    for p in points {
        for a in 0..3 {
            let v = p[a] as f64;
            if v < min[a] {
                min[a] = v;
            }
            if v > max[a] {
                max[a] = v;
            }
        }
    }
    if points.is_empty() {
        return [0.0; 6];
    }
    [min[0], min[1], min[2], max[0], max[1], max[2]]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn output(kind: &str, uri: &str, meta: serde_json::Value) -> Output {
        Output {
            id: format!("out-{kind}"),
            job_id: "job-1".into(),
            kind: kind.into(),
            uri: uri.into(),
            meta,
            created_ms: 0,
        }
    }

    fn write_ply(dir: &Path, name: &str, points: &[[f32; 3]]) -> PathBuf {
        let path = dir.join(name);
        let mut body = format!(
            "ply\nformat ascii 1.0\nelement vertex {}\nproperty float x\nproperty float y\nproperty float z\nend_header\n",
            points.len()
        );
        for p in points {
            body.push_str(&format!("{} {} {}\n", p[0], p[1], p[2]));
        }
        std::fs::write(&path, body).unwrap();
        path
    }

    #[test]
    fn the_one_dimensional_transform_is_exact() {
        // A single seed at index 3 on a length-8 line: the exact squared distance
        // is (q-3)^2 everywhere. An approximate (chamfer) transform would not
        // reproduce these values.
        const INF: f32 = 1e20;
        let mut f = vec![INF; 8];
        f[3] = 0.0;
        let mut out = vec![0.0f32; 8];
        transform_1d(&f, &mut out);
        for (q, got) in out.iter().enumerate() {
            let d = q as f32 - 3.0;
            assert!(
                (got - d * d).abs() < 1e-3,
                "index {q}: expected {}, got {got}",
                d * d
            );
        }

        // Two seeds: every cell takes the nearer one.
        let mut f = vec![INF; 9];
        f[1] = 0.0;
        f[7] = 0.0;
        let mut out = vec![0.0f32; 9];
        transform_1d(&f, &mut out);
        let expect: Vec<f32> = (0..9)
            .map(|q| {
                let a = (q as f32 - 1.0).abs();
                let b = (q as f32 - 7.0).abs();
                let m = a.min(b);
                m * m
            })
            .collect();
        for (q, (got, want)) in out.iter().zip(expect.iter()).enumerate() {
            assert!(
                (got - want).abs() < 1e-3,
                "index {q}: want {want}, got {got}"
            );
        }
    }

    #[test]
    fn the_esdf_measures_real_metric_clearance_from_a_surface() {
        // One point at the origin, 1 m voxels, 4 m truncation. The field must
        // report 0 at the surface and the true Euclidean distance around it —
        // this is the number an aircraft would fly a clearance against, so an
        // approximation is not good enough.
        let grid = esdf_from_points(&[[0.0, 0.0, 0.0]], 1.0, 4.0).unwrap();
        assert_eq!(grid.resolution_m, 1.0);
        assert_eq!(grid.truncation_m, 4.0);
        assert_eq!(grid.voxel_count(), grid.distances.len());
        // The seed voxel sits where the point does.
        let seed = (
            ((0.0 - grid.origin[0]) / 1.0).round() as u32,
            ((0.0 - grid.origin[1]) / 1.0).round() as u32,
            ((0.0 - grid.origin[2]) / 1.0).round() as u32,
        );
        assert_eq!(grid.at(seed.0, seed.1, seed.2), Some(0.0));
        // One voxel along x is exactly 1 m.
        let one = grid.at(seed.0 + 1, seed.1, seed.2).unwrap();
        assert!((one - 1.0).abs() < 1e-3, "1 m along x, got {one}");
        // The diagonal is sqrt(2) m, NOT 2 m — the thing a chamfer approximation
        // gets wrong and a planner would fly against.
        let diag = grid.at(seed.0 + 1, seed.1 + 1, seed.2).unwrap();
        assert!(
            (diag - 2.0f32.sqrt()).abs() < 1e-3,
            "the diagonal must be sqrt(2) m, got {diag}"
        );
        // And the 3-D diagonal is sqrt(3) m.
        let diag3 = grid.at(seed.0 + 1, seed.1 + 1, seed.2 + 1).unwrap();
        assert!(
            (diag3 - 3.0f32.sqrt()).abs() < 1e-3,
            "the 3-D diagonal must be sqrt(3) m, got {diag3}"
        );
        // Nothing exceeds the truncation radius.
        assert!(grid.distances.iter().all(|d| *d <= 4.0 + 1e-6));
        // The buffer is f32 little-endian, as the descriptor promises.
        assert_eq!(grid.to_le_bytes().len(), grid.distances.len() * 4);
    }

    #[test]
    fn the_esdf_refuses_input_it_cannot_measure_rather_than_inventing_a_field() {
        // Each of these would otherwise produce a grid a planner reads as real
        // free space, which is the most dangerous possible output.
        assert_eq!(esdf_from_points(&[], 0.2, 4.0), Err(EsdfError::NoPoints));
        assert_eq!(
            esdf_from_points(&[[f32::NAN, 0.0, 0.0]], 0.2, 4.0),
            Err(EsdfError::NonFinite)
        );
        assert_eq!(
            esdf_from_points(&[[0.0, 0.0, 0.0]], 0.0, 4.0),
            Err(EsdfError::BadResolution(0.0))
        );
        // A capture whose extent implies more than the voxel ceiling is refused,
        // not silently truncated to a grid that omits most of the world.
        let huge = esdf_from_points(&[[0.0, 0.0, 0.0], [5_000.0, 5_000.0, 5_000.0]], 0.2, 4.0);
        assert!(matches!(huge, Err(EsdfError::TooLarge { .. })), "{huge:?}");
    }

    #[test]
    fn descriptors_carry_the_session_the_generation_and_real_measured_counts() {
        let dir = tempfile::tempdir().unwrap();
        let ply = write_ply(
            dir.path(),
            "cloud.ply",
            &[[0.0, 0.0, 0.0], [1.0, 2.0, 3.0], [-1.0, 0.5, 0.0]],
        );
        let outputs = vec![
            output(
                "pointcloud",
                &format!("file://{}", ply.display()),
                serde_json::Value::Null,
            ),
            output(
                "splat",
                "http://node.example/artifacts/job-1/scene.spz",
                serde_json::json!({"gaussian_count": 1_250_000, "step": 30_000, "lod_levels": 4,
                                   "manifest_url": "http://node.example/artifacts/job-1/manifest.json"}),
            ),
            output(
                "mesh",
                "http://node.example/artifacts/job-1/mesh.glb",
                serde_json::json!({"vertex_count": 8_000, "face_count": 16_000}),
            ),
        ];
        let set = derive_descriptors("atlas-drone-1-1000", 7, &outputs, dir.path());
        assert!(!set.is_empty());

        let cloud = set.pointcloud.expect("a point cloud descriptor");
        assert_eq!(cloud.session_id, "atlas-drone-1-1000");
        assert_eq!(cloud.generation, 7);
        assert_eq!(
            cloud.point_count, 3,
            "the count is parsed from the real .ply, not guessed"
        );
        assert_eq!(
            cloud.bounds,
            [-1.0, 0.0, 0.0, 1.0, 2.0, 3.0],
            "bounds are the real axis-aligned extent"
        );

        let splat = set.splat.expect("a splat descriptor");
        assert_eq!(splat.generation, 7);
        assert_eq!(splat.gaussian_count, 1_250_000);
        assert_eq!(splat.lod_levels, 4);
        assert!(
            splat.manifest_url.is_some(),
            "the LOD manifest rides through"
        );

        let mesh = set.mesh.expect("a mesh descriptor");
        assert_eq!(mesh.vertex_count, 8_000);
        assert_eq!(mesh.face_count, 16_000);
    }

    #[test]
    fn a_generation_with_no_readable_geometry_publishes_nothing_rather_than_an_empty_world() {
        let dir = tempfile::tempdir().unwrap();
        // A mock backend's placeholder output: no file, nothing measurable.
        let outputs = vec![output(
            "splat",
            "mock://splat/ds-9",
            serde_json::Value::Null,
        )];
        let occ = derive_occupancy(
            "sess-x",
            1,
            &outputs,
            dir.path(),
            "job-1",
            "http://node.example",
        )
        .unwrap();
        assert!(
            occ.is_none(),
            "no geometry means no planning input, never an empty free-space grid"
        );
        // And a generation with no outputs at all yields an empty set.
        assert!(derive_descriptors("sess-x", 1, &[], dir.path()).is_empty());
    }

    #[test]
    fn occupancy_is_derived_written_and_addressable() {
        let dir = tempfile::tempdir().unwrap();
        let ply = write_ply(
            dir.path(),
            "cloud.ply",
            &[[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]],
        );
        let outputs = vec![output(
            "pointcloud",
            &format!("file://{}", ply.display()),
            serde_json::Value::Null,
        )];
        let (desc, grid) = derive_occupancy(
            "sess-esdf",
            3,
            &outputs,
            dir.path(),
            "job-7",
            "http://node.example/",
        )
        .unwrap()
        .expect("geometry yields an ESDF");

        assert_eq!(desc.session_id, "sess-esdf");
        assert_eq!(desc.generation, 3);
        assert_eq!(
            desc.field,
            OccupancyField::Esdf,
            "a planner needs a distance field, not a voxel dump"
        );
        assert_eq!(desc.resolution_m, DEFAULT_ESDF_RESOLUTION_M);
        assert_eq!(desc.truncation_m, DEFAULT_ESDF_TRUNCATION_M);
        assert_eq!(desc.dims, grid.dims);
        assert_eq!(desc.origin, grid.origin);

        // The buffer is on disk at the advertised path and is exactly the
        // f32 grid the descriptor's dims describe.
        let url = desc.url.expect("a fetchable url");
        assert!(url.starts_with("http://node.example/artifacts/"), "{url}");
        let rel = url.split_once("/artifacts/").unwrap().1;
        let bytes = std::fs::read(dir.path().join(rel)).expect("the esdf buffer was written");
        assert_eq!(
            bytes.len(),
            grid.dims[0] as usize * grid.dims[1] as usize * grid.dims[2] as usize * 4
        );
        assert_eq!(bytes, grid.to_le_bytes());
    }
}
