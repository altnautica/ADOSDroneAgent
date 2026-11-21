//! Building one `vision.depth.v2` frame
//! ([`ados_protocol::vision_depth_frame`]) from the model's map and the
//! camera's metric fit.
//!
//! The model's map is sampled (nearest, so no pixel blends a near and a far
//! surface) at the centre of each of the published plane's pixels and put
//! through the fit for the optical-axis depth `Z`. Each pixel's standard
//! deviation is
//!
//! ```text
//! sigma = sqrt( (Z² · sigma_inv)² + (EDGE_GAIN · J)² + SIGMA_FLOOR_M² )
//! ```
//!
//! - `Z² · sigma_inv`: the fit's inverse-depth residual carried to depth
//!   (grows with distance squared; `sigma_inv` already grows while a fit is
//!   held, [`super::metric`]);
//! - `J`: the largest depth jump, metres, between the pixel and its eight
//!   neighbours, a neighbour without depth (sky) counting as a jump of the
//!   pixel's own depth. A pixel next to a depth discontinuity may belong to
//!   either side, and monocular models smear edges, so it carries part of
//!   the jump as uncertainty;
//! - [`SIGMA_FLOOR_M`]: the model's resolution floor.

use ados_protocol::vision_depth_frame::{DepthFit, DepthFrame, FRAME_COLS, FRAME_ROWS};

use super::metric::MetricFit;
use crate::backend::DenseMap;
use crate::depth::fit::Normaliser;
use crate::depth::geometry::CameraGeometry;

/// The share of a neighbouring depth jump a pixel carries as uncertainty.
pub const EDGE_GAIN: f32 = 0.5;

/// The least standard deviation a pixel has, metres.
pub const SIGMA_FLOOR_M: f32 = 0.05;

/// The nearest depth a pixel may hold, metres.
const MIN_DEPTH_M: f32 = 0.1;

/// The farthest depth a `u16` millimetre value holds.
const MAX_DEPTH_M: f32 = u16::MAX as f32 / 1000.0;

/// Metres to saturating millimetres.
fn mm(m: f32) -> u16 {
    (m * 1000.0).round().clamp(0.0, f32::from(u16::MAX)) as u16
}

/// The plane's pinhole intrinsics, pixels: the camera's horizontal field of
/// view across the plane's width, its vertical one (from the frame aspect)
/// across its height, the principal point at the centre.
pub fn intrinsics(geometry: &CameraGeometry, aspect: f32) -> [f32; 4] {
    let (w, h) = (f32::from(FRAME_COLS), f32::from(FRAME_ROWS));
    // The projector's focal lengths, in frame widths and heights.
    let fx_w = 0.5 / (geometry.hfov_deg.clamp(10.0, 170.0).to_radians() / 2.0).tan();
    let fy_h = fx_w * aspect.max(0.1);
    [fx_w * w, fy_h * h, w / 2.0, h / 2.0]
}

/// One frame from `camera_id` captured at `stamp_ns`: `map` normalised by
/// `norm`, scaled by `fit` (held for `held_ms`), in a camera of `geometry`
/// whose frames are `aspect` wide.
#[allow(clippy::too_many_arguments)]
pub fn build(
    camera_id: &str,
    stamp_ns: u64,
    map: &DenseMap,
    norm: &Normaliser,
    fit: &MetricFit,
    held_ms: u64,
    geometry: &CameraGeometry,
    aspect: f32,
) -> DepthFrame {
    let (w, h) = (usize::from(FRAME_COLS), usize::from(FRAME_ROWS));
    let depth: Vec<f32> = (0..w * h)
        .map(|i| {
            let u = ((i % w) as f32 + 0.5) / w as f32;
            let v = ((i / w) as f32 + 0.5) / h as f32;
            fit.depth(norm.apply(map.at(u, v)))
                .filter(|z| z.is_finite() && (MIN_DEPTH_M..=MAX_DEPTH_M).contains(z))
                .unwrap_or(0.0)
        })
        .collect();
    let sigma_mm = (0..w * h)
        .map(|i| {
            let z = depth[i];
            if z <= 0.0 {
                return u16::MAX;
            }
            let (c, r) = ((i % w) as isize, (i / w) as isize);
            let mut jump = 0.0f32;
            for dr in -1..=1isize {
                for dc in -1..=1isize {
                    let (nc, nr) = (c + dc, r + dr);
                    if (dr, dc) == (0, 0)
                        || nc < 0
                        || nr < 0
                        || nc >= w as isize
                        || nr >= h as isize
                    {
                        continue;
                    }
                    let n = depth[nr as usize * w + nc as usize];
                    jump = jump.max(if n > 0.0 { (n - z).abs() } else { z });
                }
            }
            let fit_m = z * z * fit.sigma_inv;
            let edge_m = EDGE_GAIN * jump;
            mm((fit_m * fit_m + edge_m * edge_m + SIGMA_FLOOR_M * SIGMA_FLOOR_M).sqrt())
        })
        .collect();
    let [fx, fy, cx, cy] = intrinsics(geometry, aspect);
    DepthFrame {
        camera_id: camera_id.to_string(),
        stamp_ns,
        w: FRAME_COLS,
        h: FRAME_ROWS,
        fx,
        fy,
        cx,
        cy,
        pitch_down_deg: geometry.pitch_down_deg,
        depth_mm: depth.iter().map(|z| mm(*z)).collect(),
        sigma_mm,
        fit: DepthFit {
            scale: fit.s,
            shift: fit.t,
            inliers: fit.inliers,
            residual_m: fit.residual_m,
            held_ms: u32::try_from(held_ms).unwrap_or(u32::MAX),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ados_protocol::vision_depth_frame::FRAME_MAX_BYTES;

    const FIT: MetricFit = MetricFit {
        s: 1.0,
        t: 0.0,
        inliers: 12,
        residual_m: 0.1,
        sigma_inv: 0.01,
    };

    /// A scene in normalised inverse depth: the left half a wall at
    /// `1 / left` metres, the right half one at `1 / right`.
    fn two_walls(left: f32, right: f32) -> DenseMap {
        let (w, h) = (252u32, 196u32);
        let data = (0..w * h)
            .map(|i| if i % w < w / 2 { left } else { right })
            .collect();
        DenseMap::new(w, h, data).unwrap()
    }

    fn px(f: &DepthFrame, c: usize, r: usize) -> (f32, f32) {
        let i = r * usize::from(f.w) + c;
        (
            f32::from(f.depth_mm[i]) / 1000.0,
            f32::from(f.sigma_mm[i]) / 1000.0,
        )
    }

    fn frame(map: &DenseMap, fit: &MetricFit) -> DepthFrame {
        build(
            "main",
            7,
            map,
            &Normaliser::IDENTITY,
            fit,
            0,
            &CameraGeometry::default(),
            16.0 / 9.0,
        )
    }

    #[test]
    fn a_pixel_reads_its_depth_and_its_sigma_grows_with_the_square_of_it() {
        // Walls at 2 m and 8 m.
        let f = frame(&two_walls(0.5, 0.125), &FIT);
        assert_eq!((f.w, f.h), (FRAME_COLS, FRAME_ROWS));
        let (near, s_near) = px(&f, 10, 40);
        let (far, s_far) = px(&f, 110, 40);
        assert!((near - 2.0).abs() < 1e-3 && (far - 8.0).abs() < 1e-3);
        let want = |z: f32| ((z * z * 0.01).powi(2) + SIGMA_FLOOR_M.powi(2)).sqrt();
        assert!((s_near - want(2.0)).abs() < 2e-3, "{s_near}");
        assert!((s_far - want(8.0)).abs() < 2e-3, "{s_far}");
        // Four times the distance, about sixteen times the fit term.
        assert!(s_far > 10.0 * s_near);
    }

    #[test]
    fn a_pixel_beside_a_depth_edge_is_less_certain_than_one_off_it() {
        let f = frame(&two_walls(0.5, 0.125), &FIT);
        let edge = usize::from(f.w) / 2 - 1;
        let (z_edge, s_edge) = px(&f, edge, 40);
        let (z_off, s_off) = px(&f, edge - 5, 40);
        assert_eq!(z_edge, z_off);
        // Half the 6 m jump.
        assert!(s_edge > 2.9 && s_off < 0.1, "{s_edge} {s_off}");
    }

    #[test]
    fn a_held_fit_widens_every_pixel_and_sky_has_no_depth() {
        let held = MetricFit {
            sigma_inv: FIT.sigma_inv * 3.0,
            ..FIT
        };
        let fresh = frame(&two_walls(0.5, 0.0), &FIT);
        let stale = frame(&two_walls(0.5, 0.0), &held);
        assert!(px(&stale, 10, 40).1 > 2.0 * px(&fresh, 10, 40).1);
        // The right half is at infinity.
        assert_eq!(px(&fresh, 110, 40), (0.0, f32::from(u16::MAX) / 1000.0));
    }

    #[test]
    fn the_intrinsics_agree_with_the_engine_projector() {
        let g = CameraGeometry {
            hfov_deg: 87.0,
            pitch_down_deg: 0.0,
        };
        let aspect = 16.0 / 9.0;
        let [fx, fy, cx, cy] = intrinsics(&g, aspect);
        let proj = g.projector(aspect);
        // A body point (forward, right, down) in a level camera.
        let p = [6.0f32, 1.5, 0.8];
        let (z, u, v) = proj.project(p).unwrap();
        assert!((fx * p[1] / z + cx - u * f32::from(FRAME_COLS)).abs() < 1e-3);
        assert!((fy * p[2] / z + cy - v * f32::from(FRAME_ROWS)).abs() < 1e-3);
    }

    #[test]
    fn a_frame_fits_one_event() {
        let f = frame(&two_walls(0.5, 0.125), &FIT);
        assert!(f.to_msgpack().len() <= FRAME_MAX_BYTES);
    }
}
