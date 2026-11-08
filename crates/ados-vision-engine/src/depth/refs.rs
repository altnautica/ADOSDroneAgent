//! Ranging references for the depth fit, paired with the model's value where
//! they fall in the camera's image:
//!
//! - each LiDAR return and the downward rangefinder spot, measured;
//! - the ground the rangefinder ranges, taken as level: with the vehicle's
//!   roll and pitch it gives the depth of every ray that meets it. The planar
//!   LiDAR's returns all lie on one image row, often on thin trunks the model
//!   barely resolves, so on their own they leave the rest of the frame
//!   unconstrained; the ground spans the lower image. A ray whose ground
//!   point lies beyond a LiDAR return on its bearing is left out (the
//!   obstacle stands in front of it), as are rays that graze the ground, where
//!   a small slope moves the hit far.

use super::feed::{Ranging, Scan};
use super::geometry::Projector;
use super::grid::cell_at;
use crate::backend::DenseMap;

/// LiDAR returns nearer than this are the airframe, not the scene.
const MIN_RANGE_M: f32 = 0.3;

/// Ground references are tried on this lattice of image points.
const GROUND_COLS: usize = 16;
const GROUND_ROWS: usize = 9;

/// The farthest ground reference, metres along the ray.
const GROUND_MAX_RANGE_M: f32 = 25.0;

/// The shallowest a ray may meet the ground and still be a reference, radians
/// below the horizon (10°).
const GROUND_MIN_DEPRESSION_RAD: f32 = 0.174_532_9;

/// The references one frame offers.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct References {
    /// `(model value, optical depth)` pairs.
    pub pairs: Vec<(f32, f32)>,
    /// The grid cells a measured reference (LiDAR return or rangefinder spot)
    /// landed in.
    pub cells: Vec<usize>,
}

/// Collect the references `ranging` offers in a camera seeing `map`.
pub fn collect(map: &DenseMap, proj: &Projector, ranging: &Ranging) -> References {
    let mut out = References::default();
    let mut add = |p: [f32; 3]| {
        if let Some((z, u, v)) = proj.project(p) {
            if let Some(cell) = cell_at(u, v) {
                out.pairs.push((map.at(u, v), z));
                out.cells.push(cell);
            }
        }
    };
    if let Some(scan) = &ranging.scan {
        for (k, r) in scan.ranges_m.iter().enumerate() {
            if !(r.is_finite() && *r >= MIN_RANGE_M) {
                continue;
            }
            let a = scan.angle_start_rad + k as f32 * scan.angle_increment_rad;
            add([r * a.cos(), r * a.sin(), 0.0]);
        }
    }
    let down = ranging.down_m.filter(|d| d.is_finite() && *d > 0.0);
    if let Some(d) = down {
        add([0.0, 0.0, d]);
    }
    if let (Some(d), Some((roll, pitch))) = (down, ranging.attitude) {
        add_ground(
            &mut out.pairs,
            map,
            proj,
            ranging.scan.as_ref(),
            d,
            roll,
            pitch,
        );
    }
    out.cells.sort_unstable();
    out.cells.dedup();
    out
}

/// Pair the lattice rays that meet the level ground `down_m` below the
/// rangefinder (measured along the body's down axis at `roll`, `pitch`).
fn add_ground(
    pairs: &mut Vec<(f32, f32)>,
    map: &DenseMap,
    proj: &Projector,
    scan: Option<&Scan>,
    down_m: f32,
    roll: f32,
    pitch: f32,
) {
    let (sr, cr) = roll.sin_cos();
    let (sp, cp) = pitch.sin_cos();
    // World down in the body frame, and the height above the ground.
    let g = [-sp, sr * cp, cr * cp];
    let height = down_m * g[2];
    if height <= 0.0 {
        return;
    }
    let min_sin = GROUND_MIN_DEPRESSION_RAD.sin();
    for r in 0..GROUND_ROWS {
        for c in 0..GROUND_COLS {
            let u = (c as f32 + 0.5) / GROUND_COLS as f32;
            let v = (r as f32 + 0.5) / GROUND_ROWS as f32;
            let ray = proj.ray(u, v);
            let norm = (ray[0] * ray[0] + ray[1] * ray[1] + ray[2] * ray[2]).sqrt();
            let fall = ray[0] * g[0] + ray[1] * g[1] + ray[2] * g[2];
            if fall < min_sin * norm {
                continue;
            }
            // The ray is scaled to unit optical depth, so the factor that
            // brings it to the ground is the optical depth there.
            let z = height / fall;
            if z * norm > GROUND_MAX_RANGE_M {
                continue;
            }
            let (x, y) = (ray[0] * z, ray[1] * z);
            if scan.is_some_and(|s| occluded(s, x, y)) {
                continue;
            }
            pairs.push((map.at(u, v), z));
        }
    }
}

/// Whether the scan has a return on the bearing of body point `(x, y)`
/// nearer than it.
fn occluded(scan: &Scan, x: f32, y: f32) -> bool {
    let n = scan.ranges_m.len();
    if n == 0 || !(scan.angle_increment_rad.is_finite() && scan.angle_increment_rad > 0.0) {
        return false;
    }
    let bearing = (y.atan2(x) - scan.angle_start_rad).rem_euclid(std::f32::consts::TAU);
    let k = ((bearing / scan.angle_increment_rad) as usize).min(n - 1);
    let r = scan.ranges_m[k];
    r.is_finite() && r >= MIN_RANGE_M && r < x.hypot(y)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::depth::feed::Scan;
    use crate::depth::geometry::CameraGeometry;

    #[test]
    fn returns_in_the_field_of_view_become_references_and_the_rest_do_not() {
        let map = DenseMap::new(4, 4, (0..16).map(|i| i as f32).collect()).unwrap();
        let proj = CameraGeometry::default().projector(1.0);
        // Bins every 10° all round: only those within ±35° of the nose are seen.
        let ranges: Vec<f32> = (0..36).map(|k| if k == 1 { 0.1 } else { 5.0 }).collect();
        let r = collect(
            &map,
            &proj,
            &Ranging {
                scan: Some(Scan {
                    angle_start_rad: 0.0,
                    angle_increment_rad: 10f32.to_radians(),
                    ranges_m: ranges,
                }),
                down_m: Some(4.0),
                attitude: None,
            },
        );
        // Bins 0, 2, 3, 33, 34, 35 (bin 1 is the airframe); the rangefinder
        // spot below is out of a level forward camera's view.
        assert_eq!(r.pairs.len(), 6);
        // Bin 0 is dead ahead: optical depth 5 m, on the centre row.
        assert!(r.pairs.iter().any(|(_, z)| (z - 5.0).abs() < 1e-4));
        assert!(r
            .cells
            .iter()
            .all(|c| c / ados_protocol::vision_depth::GRID_COLS == 9));
    }

    #[test]
    fn a_belly_camera_takes_the_rangefinder_spot() {
        let map = DenseMap::new(2, 2, vec![0.1, 0.2, 0.3, 0.4]).unwrap();
        let proj = CameraGeometry {
            hfov_deg: 90.0,
            pitch_down_deg: 90.0,
        }
        .projector(1.0);
        let r = collect(
            &map,
            &proj,
            &Ranging {
                scan: None,
                down_m: Some(3.5),
                attitude: None,
            },
        );
        assert_eq!(r.pairs.len(), 1);
        assert!((r.pairs[0].1 - 3.5).abs() < 1e-4);
    }

    #[test]
    fn the_ground_below_gives_a_forward_camera_references_down_its_lower_image() {
        let map = DenseMap::new(4, 4, vec![1.0; 16]).unwrap();
        let proj = CameraGeometry {
            hfov_deg: 87.0,
            pitch_down_deg: 15.0,
        }
        .projector(16.0 / 9.0);
        let level = Ranging {
            scan: None,
            down_m: Some(4.0),
            attitude: Some((0.0, 0.0)),
        };
        let r = collect(&map, &proj, &level);
        assert!(r.pairs.len() > 30, "{}", r.pairs.len());
        // Inferred, not measured: no cell is marked.
        assert!(r.cells.is_empty());
        // Every pair lies on the plane 4 m below: depth times the ray's
        // fall is the height.
        let nearest = r.pairs.iter().map(|p| p.1).fold(f32::INFINITY, f32::min);
        // The bottom lattice row looks about 40° down: about 5.6 m of
        // optical depth, never nearer than the height.
        assert!((4.0..6.5).contains(&nearest), "{nearest}");
        // Nosed up 20°, less ground is in view and it is farther.
        let nose_up = collect(
            &map,
            &proj,
            &Ranging {
                attitude: Some((0.0, 20f32.to_radians())),
                ..level.clone()
            },
        );
        assert!(nose_up.pairs.len() < r.pairs.len());
        assert!(nose_up.pairs.iter().all(|p| p.1 > nearest));
        // A wall the LiDAR sees 3 m ahead across the view hides the ground
        // beyond it.
        let wall = Scan {
            angle_start_rad: 0.0,
            angle_increment_rad: 1f32.to_radians(),
            ranges_m: (0..360)
                .map(|k| if !(60..300).contains(&k) { 3.0 } else { 0.0 })
                .collect(),
        };
        let walled = collect(
            &map,
            &proj,
            &Ranging {
                scan: Some(wall),
                ..level.clone()
            },
        );
        let ground: Vec<_> = walled.pairs.iter().filter(|p| p.1 > 3.5).collect();
        assert!(ground.is_empty(), "{ground:?}");
        // No rangefinder reading or no attitude: no ground.
        let none = collect(
            &map,
            &proj,
            &Ranging {
                attitude: None,
                ..level
            },
        );
        assert!(none.pairs.is_empty());
    }
}
