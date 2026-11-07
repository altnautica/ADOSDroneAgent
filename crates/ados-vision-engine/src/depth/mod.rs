//! The depth layer: a monocular depth model's output turned into the
//! per-camera depth grid and the landing-zone candidates.
//!
//! Per frame: the model's relative inverse depth ([`crate::backend::DenseMap`])
//! is pooled to the grid ([`grid`]); the ranging hits that land in the image
//! ([`refs`], fed by [`feed`]) give a scale-and-shift fit ([`fit`]); with a fit
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
        let refs = refs::collect(map, &proj, ranging);
        let scale = state.filter.update(fit::fit(&refs.pairs), now_ms);
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
}
