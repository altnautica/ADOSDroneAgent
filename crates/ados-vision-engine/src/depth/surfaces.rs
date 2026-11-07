//! Landing-zone candidates from a scaled depth map.
//!
//! Geometric only: the map is sampled on a lattice, each sample cast into the
//! body frame at its fitted depth and levelled with the vehicle's roll and
//! pitch. The levelled points are binned into [`BIN_M`]-square columns; a bin
//! whose points fit a plane tilted at most [`MAX_SLOPE_DEG`] with an RMS
//! residual at most [`MAX_RESIDUAL_M`] is flat, and, when the rangefinder
//! gives the ground height, lies within [`MAX_GROUND_OFFSET_M`] of it.
//! Neighbouring flat bins join into one zone, scored by area, and each zone
//! goes out as its bounding rectangle projected back into the image.

use ados_protocol::vision_depth::SurfaceZone;

use super::fit::ScaleShift;
use super::geometry::Projector;
use crate::backend::DenseMap;

/// The side of one ground bin, metres.
pub const BIN_M: f32 = 2.0;
/// The steepest plane a zone may have, degrees.
pub const MAX_SLOPE_DEG: f32 = 10.0;
/// The largest RMS plane residual a zone may have, metres.
pub const MAX_RESIDUAL_M: f32 = 0.1;
/// How far a zone's plane may sit from the rangefinder ground, metres.
pub const MAX_GROUND_OFFSET_M: f32 = 1.0;
/// A zone of this area or more scores 1.
const FULL_SCORE_M2: f32 = 16.0;
/// The most zones sent per frame.
const MAX_ZONES: usize = 8;
/// Samples farther than this are too coarse to judge, metres.
const MAX_DEPTH_M: f32 = 30.0;
/// The fewest samples a bin is judged on.
const MIN_BIN_POINTS: usize = 4;
/// The sampling lattice over the image.
const LATTICE_COLS: usize = 64;
const LATTICE_ROWS: usize = 36;

/// Rotate a body vector into the level frame (yaw left out).
fn level(p: [f32; 3], roll: f32, pitch: f32) -> [f32; 3] {
    let (sr, cr) = roll.sin_cos();
    let (sp, cp) = pitch.sin_cos();
    // Rx(roll)
    let y = cr * p[1] - sr * p[2];
    let z1 = sr * p[1] + cr * p[2];
    // Ry(pitch)
    [cp * p[0] + sp * z1, y, -sp * p[0] + cp * z1]
}

/// The inverse of [`level`].
fn unlevel(p: [f32; 3], roll: f32, pitch: f32) -> [f32; 3] {
    let (sr, cr) = roll.sin_cos();
    let (sp, cp) = pitch.sin_cos();
    let x = cp * p[0] - sp * p[2];
    let z1 = sp * p[0] + cp * p[2];
    [x, cr * p[1] + sr * z1, -sr * p[1] + cr * z1]
}

/// Least-squares plane `z = a x + b y + c` through `pts`, with its RMS
/// residual. `None` when the points do not span a plane.
fn plane(pts: &[[f32; 3]]) -> Option<(f32, f32, f32, f32)> {
    let n = pts.len() as f64;
    let (mut sx, mut sy, mut sz) = (0.0f64, 0.0, 0.0);
    for p in pts {
        sx += f64::from(p[0]);
        sy += f64::from(p[1]);
        sz += f64::from(p[2]);
    }
    let (mx, my, mz) = (sx / n, sy / n, sz / n);
    let (mut xx, mut xy, mut yy, mut xz, mut yz) = (0.0f64, 0.0, 0.0, 0.0, 0.0);
    for p in pts {
        let (dx, dy, dz) = (
            f64::from(p[0]) - mx,
            f64::from(p[1]) - my,
            f64::from(p[2]) - mz,
        );
        xx += dx * dx;
        xy += dx * dy;
        yy += dy * dy;
        xz += dx * dz;
        yz += dy * dz;
    }
    let det = xx * yy - xy * xy;
    if det.abs() < 1e-6 * (xx + yy).max(1e-9).powi(2) {
        return None;
    }
    let a = (xz * yy - yz * xy) / det;
    let b = (yz * xx - xz * xy) / det;
    let c = mz - a * mx - b * my;
    let ss: f64 = pts
        .iter()
        .map(|p| {
            let r = f64::from(p[2]) - (a * f64::from(p[0]) + b * f64::from(p[1]) + c);
            r * r
        })
        .sum();
    Some((a as f32, b as f32, c as f32, (ss / n).sqrt() as f32))
}

struct FlatBin {
    ix: i32,
    iy: i32,
    slope_deg: f32,
    /// Plane height at the bin centre, level frame (down positive).
    z: f32,
}

/// Find the landing-zone candidates in `map`, scaled by `fit`, seen through
/// `proj` with the vehicle at `roll`/`pitch` and, when known, `agl_m` above
/// the ground.
pub fn find_zones(
    map: &DenseMap,
    fit: ScaleShift,
    proj: &Projector,
    (roll, pitch): (f32, f32),
    agl_m: Option<f32>,
) -> Vec<SurfaceZone> {
    use std::collections::BTreeMap;
    let mut bins: BTreeMap<(i32, i32), Vec<[f32; 3]>> = BTreeMap::new();
    for r in 0..LATTICE_ROWS {
        for c in 0..LATTICE_COLS {
            let u = (c as f32 + 0.5) / LATTICE_COLS as f32;
            let v = (r as f32 + 0.5) / LATTICE_ROWS as f32;
            let Some(z) = fit.depth(map.at(u, v)).filter(|z| *z <= MAX_DEPTH_M) else {
                continue;
            };
            let ray = proj.ray(u, v);
            let p = level([ray[0] * z, ray[1] * z, ray[2] * z], roll, pitch);
            // Only points below the vehicle can be ground.
            if p[2] <= 0.0 {
                continue;
            }
            let key = ((p[0] / BIN_M).floor() as i32, (p[1] / BIN_M).floor() as i32);
            bins.entry(key).or_default().push(p);
        }
    }
    let mut flat: Vec<FlatBin> = Vec::new();
    for ((ix, iy), pts) in &bins {
        if pts.len() < MIN_BIN_POINTS {
            continue;
        }
        let Some((a, b, c, residual)) = plane(pts) else {
            continue;
        };
        let slope_deg = (a * a + b * b).sqrt().atan().to_degrees();
        let cx = (*ix as f32 + 0.5) * BIN_M;
        let cy = (*iy as f32 + 0.5) * BIN_M;
        let z = a * cx + b * cy + c;
        let on_ground = agl_m.is_none_or(|g| (z - g).abs() <= MAX_GROUND_OFFSET_M);
        if slope_deg <= MAX_SLOPE_DEG && residual <= MAX_RESIDUAL_M && on_ground {
            flat.push(FlatBin {
                ix: *ix,
                iy: *iy,
                slope_deg,
                z,
            });
        }
    }
    // Join 4-neighbours into zones.
    let mut seen = vec![false; flat.len()];
    let mut zones: Vec<SurfaceZone> = Vec::new();
    for start in 0..flat.len() {
        if seen[start] {
            continue;
        }
        seen[start] = true;
        let mut members = vec![start];
        let mut k = 0;
        while k < members.len() {
            let (x, y) = (flat[members[k]].ix, flat[members[k]].iy);
            for (j, b) in flat.iter().enumerate() {
                if !seen[j] && (b.ix - x).abs() + (b.iy - y).abs() == 1 {
                    seen[j] = true;
                    members.push(j);
                }
            }
            k += 1;
        }
        let n = members.len() as f32;
        let (mut x0, mut x1, mut y0, mut y1) = (i32::MAX, i32::MIN, i32::MAX, i32::MIN);
        let (mut slope, mut z) = (0.0f32, 0.0f32);
        for &m in &members {
            let b = &flat[m];
            x0 = x0.min(b.ix);
            x1 = x1.max(b.ix);
            y0 = y0.min(b.iy);
            y1 = y1.max(b.iy);
            slope += b.slope_deg;
            z += b.z;
        }
        let z = z / n;
        let corners = [(x1 + 1, y0), (x1 + 1, y1 + 1), (x0, y1 + 1), (x0, y0)];
        let pts: Option<Vec<[f32; 2]>> = corners
            .iter()
            .map(|&(ix, iy)| {
                let body = unlevel([ix as f32 * BIN_M, iy as f32 * BIN_M, z], roll, pitch);
                proj.project(body)
                    .map(|(_, u, v)| [u.clamp(0.0, 1.0), v.clamp(0.0, 1.0)])
            })
            .collect();
        if let Some(pts) = pts {
            zones.push(SurfaceZone {
                pts,
                score: (n * BIN_M * BIN_M / FULL_SCORE_M2).min(1.0),
                slope_deg: slope / n,
            });
        }
    }
    zones.sort_by(|a, b| b.score.total_cmp(&a.score));
    zones.truncate(MAX_ZONES);
    zones
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::depth::geometry::CameraGeometry;

    /// The inverse depth a belly camera `agl` above `ground(x, y)` sees, with
    /// the fit `1/z = x` exact.
    fn belly_map(agl: f32, ground: impl Fn(f32, f32) -> f32) -> (DenseMap, Projector) {
        let g = CameraGeometry {
            hfov_deg: 90.0,
            pitch_down_deg: 90.0,
        };
        let proj = g.projector(1.0);
        let (w, h) = (128u32, 128u32);
        let mut data = Vec::with_capacity((w * h) as usize);
        for y in 0..h {
            for x in 0..w {
                let ray = proj.ray((x as f32 + 0.5) / w as f32, (y as f32 + 0.5) / h as f32);
                // March down the ray to the ground surface.
                let mut z = 0.1;
                while z < 60.0 {
                    let p = [ray[0] * z, ray[1] * z, ray[2] * z];
                    if p[2] >= agl - ground(p[0], p[1]) {
                        break;
                    }
                    z += 0.01;
                }
                data.push(1.0 / z);
            }
        }
        (DenseMap::new(w, h, data).unwrap(), proj)
    }

    const EXACT: ScaleShift = ScaleShift { s: 1.0, t: 0.0 };

    #[test]
    fn level_ground_below_a_belly_camera_is_one_large_zone() {
        let (map, proj) = belly_map(8.0, |_, _| 0.0);
        let zones = find_zones(&map, EXACT, &proj, (0.0, 0.0), Some(8.0));
        assert!(!zones.is_empty());
        assert_eq!(zones[0].score, 1.0);
        assert!(zones[0].slope_deg < 2.0, "{}", zones[0].slope_deg);
        assert!(zones[0].pts.iter().all(|p| (0.0..=1.0).contains(&p[0])));
    }

    #[test]
    fn a_steep_slope_has_no_zone_and_a_ground_far_from_the_rangefinder_is_refused() {
        // 30° slope across x.
        let (map, proj) = belly_map(8.0, |x, _| x * 30f32.to_radians().tan());
        assert!(find_zones(&map, EXACT, &proj, (0.0, 0.0), None).is_empty());
        // Level ground, but the rangefinder says 3 m: that plane is a roof.
        let (map, proj) = belly_map(8.0, |_, _| 0.0);
        assert!(find_zones(&map, EXACT, &proj, (0.0, 0.0), Some(3.0)).is_empty());
    }

    #[test]
    fn levelling_round_trips() {
        let p = [1.0, -2.0, 3.0];
        let q = unlevel(level(p, 0.2, -0.3), 0.2, -0.3);
        for i in 0..3 {
            assert!((p[i] - q[i]).abs() < 1e-5);
        }
        // Pitched nose-up 90°, the body nose points straight up (level -z).
        let up = level([1.0, 0.0, 0.0], 0.0, std::f32::consts::FRAC_PI_2);
        assert!((up[2] + 1.0).abs() < 1e-5);
    }
}
