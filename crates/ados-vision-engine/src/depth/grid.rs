//! Pooling a dense depth map into the per-camera grid and encoding it.

use ados_protocol::vision_depth::{
    encode_range_m, GRID_CELLS, GRID_COLS, GRID_MASK_BYTES, GRID_ROWS, UNSCALED_FAR_M,
    UNSCALED_MID_M, UNSCALED_NEAR_M,
};

use super::fit::ScaleShift;
use super::geometry::Projector;
use crate::backend::DenseMap;

/// The share of a cell's samples nearer than its "near" value: the cell
/// reports its nearest tenth, so a thin obstacle in a cell still shows.
const NEAR_QUANTILE: f32 = 0.9;

/// One frame's map pooled to the grid: per cell the near value (the
/// [`NEAR_QUANTILE`] of relative inverse depth) and the median.
#[derive(Debug, Clone)]
pub struct Pooled {
    pub near: Vec<f32>,
    pub median: Vec<f32>,
}

/// The centre of cell `i` in normalised image coordinates.
pub fn cell_centre(i: usize) -> (f32, f32) {
    let (r, c) = (i / GRID_COLS, i % GRID_COLS);
    (
        (c as f32 + 0.5) / GRID_COLS as f32,
        (r as f32 + 0.5) / GRID_ROWS as f32,
    )
}

/// The cell a normalised image point falls in, `None` outside the image.
pub fn cell_at(u: f32, v: f32) -> Option<usize> {
    if !(0.0..1.0).contains(&u) || !(0.0..1.0).contains(&v) {
        return None;
    }
    let c = ((u * GRID_COLS as f32) as usize).min(GRID_COLS - 1);
    let r = ((v * GRID_ROWS as f32) as usize).min(GRID_ROWS - 1);
    Some(r * GRID_COLS + c)
}

/// Pool `map` into the grid. Every cell gets at least the sample at its
/// centre, so a map coarser than the grid still fills it.
pub fn pool(map: &DenseMap) -> Pooled {
    let (w, h) = (map.width as usize, map.height as usize);
    let mut near = vec![0.0; GRID_CELLS];
    let mut median = vec![0.0; GRID_CELLS];
    let mut samples: Vec<f32> = Vec::new();
    for i in 0..GRID_CELLS {
        let (r, c) = (i / GRID_COLS, i % GRID_COLS);
        let x0 = c * w / GRID_COLS;
        let x1 = ((c + 1) * w / GRID_COLS).max(x0 + 1).min(w);
        let y0 = r * h / GRID_ROWS;
        let y1 = ((r + 1) * h / GRID_ROWS).max(y0 + 1).min(h);
        samples.clear();
        for y in y0..y1 {
            samples.extend(
                map.data[y * w + x0..y * w + x1]
                    .iter()
                    .copied()
                    .filter(|v| v.is_finite()),
            );
        }
        if samples.is_empty() {
            let (u, v) = cell_centre(i);
            samples.push(map.at(u, v));
        }
        samples.sort_unstable_by(f32::total_cmp);
        let q = ((samples.len() - 1) as f32 * NEAR_QUANTILE).round() as usize;
        near[i] = samples[q];
        median[i] = samples[samples.len() / 2];
    }
    Pooled { near, median }
}

/// The range along cell `i`'s centre ray for optical depth `z`.
fn ray_range(proj: &Projector, i: usize, z: f32) -> f32 {
    let (u, v) = cell_centre(i);
    let r = proj.ray(u, v);
    z * (r[0] * r[0] + r[1] * r[1] + r[2] * r[2]).sqrt()
}

/// Encode a scaled grid: each cell's near value through the fit, as a range
/// in metres along its centre ray.
pub fn encode_scaled(pooled: &Pooled, fit: ScaleShift, proj: &Projector) -> Vec<u8> {
    (0..GRID_CELLS)
        .map(|i| match fit.depth(pooled.near[i]) {
            Some(z) => encode_range_m(ray_range(proj, i, z)),
            None => 255,
        })
        .collect()
}

/// Encode an unscaled grid: relative bands only. The nearest fifth of the
/// frame's inverse-depth span is the near band, the next fifth the middle
/// band, the rest far.
pub fn encode_unscaled(pooled: &Pooled) -> Vec<u8> {
    let (lo, hi) = pooled
        .near
        .iter()
        .fold((f32::INFINITY, f32::NEG_INFINITY), |(lo, hi), v| {
            (lo.min(*v), hi.max(*v))
        });
    let span = hi - lo;
    pooled
        .near
        .iter()
        .map(|v| {
            let rel = if span > 1e-6 { (v - lo) / span } else { 0.0 };
            encode_range_m(if rel >= 0.8 {
                UNSCALED_NEAR_M
            } else if rel >= 0.6 {
                UNSCALED_MID_M
            } else {
                UNSCALED_FAR_M
            })
        })
        .collect()
}

/// A `measured` bitmask with the given cells set.
pub fn mask(cells: impl IntoIterator<Item = usize>) -> Vec<u8> {
    let mut out = vec![0u8; GRID_MASK_BYTES];
    for i in cells {
        if i < GRID_CELLS {
            out[i / 8] |= 1 << (i % 8);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::synthetic_depth;
    use crate::depth::geometry::CameraGeometry;
    use ados_protocol::vision_depth::decode_range_code;

    #[test]
    fn cells_map_to_and_from_image_points() {
        assert_eq!(cell_at(0.0, 0.0), Some(0));
        assert_eq!(cell_at(0.999, 0.999), Some(GRID_CELLS - 1));
        assert_eq!(cell_at(1.0, 0.5), None);
        for i in [0, 37, GRID_CELLS - 1] {
            let (u, v) = cell_centre(i);
            assert_eq!(cell_at(u, v), Some(i));
        }
    }

    #[test]
    fn pooling_keeps_a_thin_near_post_in_its_cells() {
        let p = pool(&synthetic_depth(256, 256));
        // The post spans u 0.22..0.28: column 7 (0.219..0.25) and 8.
        let post = 9 * GRID_COLS + 8;
        let beside = 9 * GRID_COLS + 20;
        assert!(p.near[post] > 0.7, "{}", p.near[post]);
        assert!(p.near[beside] < 0.1);
        // A map coarser than the grid still fills every cell.
        let coarse = pool(&synthetic_depth(8, 8));
        assert!(coarse.near.iter().all(|v| v.is_finite()));
    }

    #[test]
    fn a_scaled_grid_reports_metres_along_each_ray() {
        let p = pool(&synthetic_depth(256, 256));
        let proj = CameraGeometry::default().projector(16.0 / 9.0);
        // 1/z = x: the post at x 0.8 is 1.25 m deep.
        let codes = encode_scaled(&p, ScaleShift { s: 1.0, t: 0.0 }, &proj);
        let post = decode_range_code(codes[9 * GRID_COLS + 8]).unwrap();
        assert!((1.2..1.45).contains(&post), "{post}");
        // Sky (x 0.02) is 50 m: still in range, far.
        let sky = decode_range_code(codes[GRID_COLS + 20]).unwrap();
        assert!(sky > 40.0);
        // A fit that puts a value past infinity encodes the far end.
        let codes = encode_scaled(&p, ScaleShift { s: 1.0, t: -0.5 }, &proj);
        assert_eq!(codes[GRID_COLS + 20], 255);
    }

    #[test]
    fn an_unscaled_grid_is_three_bands() {
        let p = pool(&synthetic_depth(256, 256));
        let codes = encode_unscaled(&p);
        let near = encode_range_m(UNSCALED_NEAR_M);
        let far = encode_range_m(UNSCALED_FAR_M);
        assert_eq!(codes[9 * GRID_COLS + 8], near);
        assert_eq!(codes[GRID_COLS + 20], far);
        assert!(codes
            .iter()
            .all(|c| *c == near || *c == far || *c == encode_range_m(UNSCALED_MID_M)));
    }

    #[test]
    fn the_mask_sets_one_bit_per_cell_lsb_first() {
        let m = mask([0, 9, GRID_CELLS - 1, GRID_CELLS + 5]);
        assert_eq!(m.len(), GRID_MASK_BYTES);
        assert_eq!(m[0], 0b1);
        assert_eq!(m[1], 0b10);
        assert_eq!(m[GRID_MASK_BYTES - 1], 0b1000_0000);
    }
}
