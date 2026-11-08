//! The depth layer: a monocular depth model's output turned into the
//! per-camera depth grid and the landing-zone candidates.
//!
//! Per frame: the model's relative inverse depth ([`crate::backend::DenseMap`])
//! is pooled to the grid ([`grid`]); the ranging references in the image (LiDAR
//! hits, the rangefinder spot and the level ground the rangefinder ranges;
//! [`refs`], fed by [`feed`]) give a scale-and-shift fit on the frame's
//! normalised values ([`fit`]); with a fit
//! the grid is in metres, without one it is relative bands. While surfaces are
//! wanted, the scaled map is searched for flat ground ([`surfaces`]). The
//! camera's mount and field of view come from [`geometry`].

pub mod feed;
pub mod fit;
pub mod geometry;
pub mod grid;
pub mod refs;
pub mod surfaces;

use std::collections::HashMap;

use ados_protocol::vision_depth::{DepthGrid, ScaleSource, SurfacesFrame, GRID_COLS, GRID_ROWS};

use crate::backend::DenseMap;
use feed::Ranging;
use fit::ScaleFilter;
use geometry::CameraGeometry;

/// The shortest gap between two landing-zone searches on one camera.
pub const SURFACES_INTERVAL_MS: u64 = 1_000;

/// What one frame's depth run produced.
#[derive(Debug, Clone, PartialEq)]
pub struct DepthOutput {
    pub grid: DepthGrid,
    /// Present when surfaces were asked for and the grid was scaled; an empty
    /// zone list is a real answer (no flat ground in view).
    pub surfaces: Option<SurfacesFrame>,
}

#[derive(Debug, Default)]
struct CameraState {
    filter: ScaleFilter,
    last_surfaces_ms: Option<u64>,
    runs: u64,
    scale_source: Option<ScaleSource>,
}

/// The per-camera state of the depth layer: each camera's fit filter, its
/// surfaces pace and its counters.
#[derive(Debug, Default)]
pub struct DepthProcessor {
    geometries: HashMap<String, CameraGeometry>,
    cameras: HashMap<String, CameraState>,
    /// Reused for each frame's value spread.
    scratch: Vec<f32>,
}

/// One camera's counters for the status sidecar.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CameraCounters {
    pub runs: u64,
    pub scale_source: Option<ScaleSource>,
}

impl DepthProcessor {
    /// A processor with the cameras' geometry (an absent camera uses the
    /// default).
    pub fn new(geometries: HashMap<String, CameraGeometry>) -> Self {
        Self {
            geometries,
            cameras: HashMap::new(),
            scratch: Vec::new(),
        }
    }

    /// Process one frame's map from `camera_id`, captured at `mono_ns`, in a
    /// frame of `aspect` (width / height), at `now_ms`. Surfaces are searched
    /// when `want_surfaces` and the camera's last search is a pace old.
    #[allow(clippy::too_many_arguments)]
    pub fn process(
        &mut self,
        camera_id: &str,
        map: &DenseMap,
        aspect: f32,
        mono_ns: u64,
        now_ms: u64,
        ranging: &Ranging,
        want_surfaces: bool,
    ) -> DepthOutput {
        let geometry = self.geometries.get(camera_id).copied().unwrap_or_default();
        let proj = geometry.projector(aspect);
        let state = self.cameras.entry(camera_id.to_string()).or_default();
        let pooled = grid::pool(map);
        let norm = fit::Normaliser::of(&pooled.median, &mut self.scratch);
        let refs = refs::collect(map, &proj, ranging);
        let fresh = norm.and_then(|n| fit::fit(&refs.pairs, &n));
        let scale = state
            .filter
            .update(fresh, now_ms)
            .zip(norm)
            .map(|(f, n)| n.to_raw(f));
        let (near, scale_source) = match scale {
            Some(f) => (grid::encode_scaled(&pooled, f, &proj), ScaleSource::Lidar),
            None => (grid::encode_unscaled(&pooled), ScaleSource::None),
        };
        state.runs += 1;
        state.scale_source = Some(scale_source);
        let due = state
            .last_surfaces_ms
            .is_none_or(|t| now_ms.saturating_sub(t) >= SURFACES_INTERVAL_MS);
        let surfaces = if want_surfaces && due {
            state.last_surfaces_ms = Some(now_ms);
            let zones = match (scale, ranging.attitude) {
                (Some(f), Some(att)) => surfaces::find_zones(map, f, &proj, att, ranging.down_m),
                // Without metres or a level reference there is nothing to
                // judge flatness against: say so with no zones.
                _ => Vec::new(),
            };
            Some(SurfacesFrame {
                camera_id: camera_id.to_string(),
                mono_ns,
                zones,
            })
        } else {
            None
        };
        DepthOutput {
            grid: DepthGrid {
                camera_id: camera_id.to_string(),
                mono_ns,
                scale_source,
                cols: GRID_COLS as u16,
                rows: GRID_ROWS as u16,
                near,
                measured: grid::mask(refs.cells),
            },
            surfaces,
        }
    }

    /// `camera_id`'s counters.
    pub fn counters(&self, camera_id: &str) -> CameraCounters {
        self.cameras
            .get(camera_id)
            .map(|s| CameraCounters {
                runs: s.runs,
                scale_source: s.scale_source,
            })
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::synthetic_depth;
    use ados_protocol::vision_depth::{decode_range_code, GRID_CELLS};
    use feed::Scan;

    /// A scan whose returns agree with the synthetic scene under `1/z = x`:
    /// the post (inverse depth 0.8) ahead-left is 1.25 m deep.
    fn ranging_for_post() -> Ranging {
        // Every bin in the camera's view gets the range the synthetic scene
        // has along that bearing on the scan plane (the image's centre row).
        let mut ranges = vec![0.0f32; 360];
        let proj = CameraGeometry::default().projector(16.0 / 9.0);
        let map = synthetic_depth(256, 256);
        for (k, r) in ranges.iter_mut().enumerate() {
            let a = (k as f32).to_radians();
            if let Some((_, u, v)) = proj.project([a.cos(), a.sin(), 0.0]) {
                if (0.0..1.0).contains(&u) {
                    // The optical depth the scene has at that pixel, as a
                    // horizontal range along the bearing.
                    let z = 1.0 / map.at(u, v);
                    let ray = proj.ray(u, v);
                    *r = z * (ray[0] * ray[0] + ray[1] * ray[1]).sqrt();
                }
            }
        }
        Ranging {
            scan: Some(Scan {
                angle_start_rad: 0.0,
                angle_increment_rad: 1f32.to_radians(),
                ranges_m: ranges,
            }),
            down_m: None,
            attitude: Some((0.0, 0.0)),
        }
    }

    #[test]
    fn with_ranging_the_grid_is_scaled_and_the_hit_cells_are_measured() {
        let mut p = DepthProcessor::default();
        let map = synthetic_depth(256, 256);
        let out = p.process("main", &map, 16.0 / 9.0, 9, 0, &ranging_for_post(), false);
        assert_eq!(out.grid.scale_source, ScaleSource::Lidar);
        assert_eq!(out.grid.near.len(), GRID_CELLS);
        let post = decode_range_code(out.grid.near[9 * GRID_COLS + 8]).unwrap();
        assert!((1.1..1.5).contains(&post), "{post}");
        assert!(out.grid.measured.iter().any(|b| *b != 0));
        assert!(out.surfaces.is_none());
        assert_eq!(p.counters("main").runs, 1);
    }

    #[test]
    fn without_ranging_the_grid_is_unscaled_and_surfaces_say_none_found() {
        let mut p = DepthProcessor::default();
        let map = synthetic_depth(64, 64);
        let out = p.process("main", &map, 16.0 / 9.0, 9, 0, &Ranging::default(), true);
        assert_eq!(out.grid.scale_source, ScaleSource::None);
        assert!(out.grid.measured.iter().all(|b| *b == 0));
        assert_eq!(out.surfaces.unwrap().zones, vec![]);
        // Surfaces are paced at 1 Hz per camera.
        let again = p.process("main", &map, 16.0 / 9.0, 10, 500, &Ranging::default(), true);
        assert!(again.surfaces.is_none());
        let later = p.process(
            "main",
            &map,
            16.0 / 9.0,
            11,
            1_000,
            &Ranging::default(),
            true,
        );
        assert!(later.surfaces.is_some());
        assert_eq!(p.counters("main").scale_source, Some(ScaleSource::None));
    }

    /// A recorded forest frame: the depth model's real output for the front
    /// camera 3.99 m above level ground (HUD overlays of the screenshot it
    /// was taken from are `null`), with the planar scan and rangefinder of
    /// the same moment.
    struct Recorded {
        map: DenseMap,
        geometry: CameraGeometry,
        aspect: f32,
        ranging: Ranging,
    }

    fn recorded_forest() -> Recorded {
        #[derive(serde::Deserialize)]
        struct Fixture {
            width: u32,
            height: u32,
            aspect: f32,
            hfov_deg: f32,
            pitch_down_deg: f32,
            down_m: f32,
            roll_rad: f32,
            pitch_rad: f32,
            scan: Scan,
            map: Vec<Option<f32>>,
        }
        let f: Fixture = serde_json::from_str(include_str!(
            "../../tests/fixtures/depth-forest-frame.json"
        ))
        .unwrap();
        let data = f.map.iter().map(|v| v.unwrap_or(f32::NAN)).collect();
        Recorded {
            map: DenseMap::new(f.width, f.height, data).unwrap(),
            geometry: CameraGeometry {
                hfov_deg: f.hfov_deg,
                pitch_down_deg: f.pitch_down_deg,
            },
            aspect: f.aspect,
            ranging: Ranging {
                scan: Some(f.scan),
                down_m: Some(f.down_m),
                attitude: Some((f.roll_rad, f.pitch_rad)),
            },
        }
    }

    /// The range to level ground `height` below along cell `i`'s ray, metres.
    fn ground_range(proj: &geometry::Projector, i: usize, height: f32) -> Option<f32> {
        let (u, v) = grid::cell_centre(i);
        let ray = proj.ray(u, v);
        (ray[2] > 0.05).then(|| height / ray[2] * (ray[0].hypot(ray[1]).hypot(ray[2])))
    }

    #[test]
    fn a_recorded_forest_frame_reads_the_ground_at_its_range() {
        let rec = recorded_forest();
        let mut p = DepthProcessor::new(HashMap::from([("main".to_string(), rec.geometry)]));
        let out = p.process("main", &rec.map, rec.aspect, 1, 0, &rec.ranging, false);
        assert_eq!(out.grid.scale_source, ScaleSource::Lidar);
        let proj = rec.geometry.projector(rec.aspect);
        let height = rec.ranging.down_m.unwrap();
        // Nothing in this frame is within 5 m of the camera (the nearest
        // ground is 5.3 m down the bottom row, the nearest trunk 6.6 m).
        let mut within_2m = 0;
        // Row by row below the horizon, the cells read the ground's range
        // (the bottom row lay under the screenshot's readout bar).
        for r in 9..GRID_ROWS - 1 {
            let mut errors: Vec<f32> = (r * GRID_COLS..(r + 1) * GRID_COLS)
                .filter_map(|i| {
                    let got = decode_range_code(out.grid.near[i])?;
                    within_2m += usize::from(got <= 2.0);
                    let truth = ground_range(&proj, i, height)?;
                    // Cells the screenshot's overlays hid have no model value.
                    let (u, v) = grid::cell_centre(i);
                    rec.map
                        .at(u, v)
                        .is_finite()
                        .then_some((got / truth - 1.0).abs())
                })
                .collect();
            assert!(errors.len() >= 8, "row {r}: {} cells", errors.len());
            errors.sort_unstable_by(f32::total_cmp);
            let median = errors[errors.len() / 2];
            assert!(median <= 0.25, "row {r}: median relative error {median}");
        }
        assert_eq!(within_2m, 0);
    }

    #[test]
    fn a_recorded_forest_frame_with_only_its_scan_row_stays_unscaled() {
        // Without the rangefinder the references are the scan's trunk hits
        // on one image row, which the model reads alike from 6.6 to 10.3 m:
        // no metres can be read off that for the rest of the frame.
        let rec = recorded_forest();
        let mut p = DepthProcessor::new(HashMap::from([("main".to_string(), rec.geometry)]));
        let scan_only = Ranging {
            down_m: None,
            ..rec.ranging
        };
        let out = p.process("main", &rec.map, rec.aspect, 1, 0, &scan_only, false);
        assert_eq!(out.grid.scale_source, ScaleSource::None);
        // The trunk hits are still marked as measured cells.
        assert!(out.grid.measured.iter().any(|b| *b != 0));
    }
}
