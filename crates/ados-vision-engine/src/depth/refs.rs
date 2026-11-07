//! Ranging references for the depth fit: each LiDAR return and the downward
//! rangefinder spot, put in the camera's image, paired with the model's value
//! there.

use super::feed::Ranging;
use super::geometry::Projector;
use super::grid::cell_at;
use crate::backend::DenseMap;

/// LiDAR returns nearer than this are the airframe, not the scene.
const MIN_RANGE_M: f32 = 0.3;

/// The references one frame offers.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct References {
    /// `(model value, measured optical depth)` pairs.
    pub pairs: Vec<(f32, f32)>,
    /// The grid cells a reference landed in.
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
    if let Some(d) = ranging.down_m.filter(|d| d.is_finite() && *d > 0.0) {
        add([0.0, 0.0, d]);
    }
    out.cells.sort_unstable();
    out.cells.dedup();
    out
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
}
