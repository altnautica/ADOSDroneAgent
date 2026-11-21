//! A licence-clean appearance descriptor and the online classifier the
//! designated subject's re-identification learns with.
//!
//! The descriptor is computed in this crate from the box's own pixels, with no
//! learned weights: the colour histogram of [`crate::histogram`] (upper and
//! lower half, so clothing separates) beside an oriented-gradient histogram
//! over a 2 x 2 grid of cells (silhouette and texture). Each block is
//! L2-normalised and weighted equally, so the whole vector has unit length and
//! a dot product is its cosine similarity.

use ados_protocol::framebus::BoundingBox;

use crate::histogram::{add_pixel, HALF_BINS};

/// Colour bins: two halves of [`HALF_BINS`].
const COLOR_DIM: usize = 2 * HALF_BINS;
/// Unsigned gradient orientation bins per cell.
const ORIENT_BINS: usize = 8;
/// Gradient cells across and down the box.
const CELLS_X: usize = 2;
const CELLS_Y: usize = 2;
const GRAD_DIM: usize = CELLS_X * CELLS_Y * ORIENT_BINS;
/// Length of a [`Features`] vector.
pub const FEAT_DIM: usize = COLOR_DIM + GRAD_DIM;
/// The box is sampled on this grid, whatever its size.
const SAMPLE_W: usize = 16;
const SAMPLE_H: usize = 32;

/// A unit-length appearance descriptor.
pub type Features = [f32; FEAT_DIM];

/// Describe the box `bbox` of an `rgb24` frame (`frame_w` x `frame_h`) into
/// `out`. Returns false (leaving `out` unspecified) when the box falls outside
/// the frame or the frame is short.
pub fn describe(
    frame: &[u8],
    frame_w: u32,
    frame_h: u32,
    bbox: &BoundingBox,
    out: &mut Features,
) -> bool {
    let (fw, fh) = (frame_w as usize, frame_h as usize);
    if fw == 0 || fh == 0 || frame.len() < fw * fh * 3 {
        return false;
    }
    let x0 = bbox.x.max(0.0);
    let y0 = bbox.y.max(0.0);
    let x1 = (bbox.x + bbox.width).min(fw as f32);
    let y1 = (bbox.y + bbox.height).min(fh as f32);
    if x1 - x0 < 2.0 || y1 - y0 < 2.0 {
        return false;
    }
    out.fill(0.0);
    let mut luma = [0f32; SAMPLE_W * SAMPLE_H];
    let (sx, sy) = ((x1 - x0) / SAMPLE_W as f32, (y1 - y0) / SAMPLE_H as f32);
    for row in 0..SAMPLE_H {
        let py = ((y0 + (row as f32 + 0.5) * sy) as usize).min(fh - 1);
        let half = if row < SAMPLE_H / 2 { 0 } else { HALF_BINS };
        for col in 0..SAMPLE_W {
            let px = ((x0 + (col as f32 + 0.5) * sx) as usize).min(fw - 1);
            let o = (py * fw + px) * 3;
            let (r, g, b) = (frame[o], frame[o + 1], frame[o + 2]);
            add_pixel(&mut out[half..half + HALF_BINS], r, g, b);
            luma[row * SAMPLE_W + col] =
                (0.299 * f32::from(r) + 0.587 * f32::from(g) + 0.114 * f32::from(b)) / 255.0;
        }
    }
    for half in out[..COLOR_DIM].as_chunks_mut::<HALF_BINS>().0 {
        let sum: f32 = half.iter().sum();
        if sum > 0.0 {
            half.iter_mut().for_each(|v| *v /= sum);
        }
    }
    let grad = &mut out[COLOR_DIM..];
    for row in 1..SAMPLE_H - 1 {
        for col in 1..SAMPLE_W - 1 {
            let i = row * SAMPLE_W + col;
            let gx = luma[i + 1] - luma[i - 1];
            let gy = luma[i + SAMPLE_W] - luma[i - SAMPLE_W];
            let mag = (gx * gx + gy * gy).sqrt();
            if mag <= 1e-6 {
                continue;
            }
            let theta = gy.atan2(gx).rem_euclid(std::f32::consts::PI);
            let bin =
                ((theta / std::f32::consts::PI * ORIENT_BINS as f32) as usize).min(ORIENT_BINS - 1);
            let cell = (row * CELLS_Y / SAMPLE_H) * CELLS_X + col * CELLS_X / SAMPLE_W;
            grad[cell * ORIENT_BINS + bin] += mag;
        }
    }
    normalize_block(&mut out[..COLOR_DIM]);
    normalize_block(&mut out[COLOR_DIM..]);
    true
}

/// Scale a block to length `1/sqrt(2)`, so two full blocks make a unit vector.
/// An all-zero block stays zero.
fn normalize_block(block: &mut [f32]) {
    let n = block.iter().map(|v| v * v).sum::<f32>().sqrt();
    if n > 0.0 {
        let s = std::f32::consts::FRAC_1_SQRT_2 / n;
        block.iter_mut().for_each(|v| *v *= s);
    }
}

/// Cosine similarity of two descriptors, clamped to `0..=1`.
pub fn similarity(a: &Features, b: &Features) -> f32 {
    let (mut dot, mut na, mut nb) = (0f32, 0f32, 0f32);
    for (x, y) in a.iter().zip(b) {
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    if na <= 0.0 || nb <= 0.0 {
        return 0.0;
    }
    (dot / (na.sqrt() * nb.sqrt())).clamp(0.0, 1.0)
}

/// Blend `x` into the running mean `mean` at `rate`, then renormalise.
pub fn blend(mean: &mut Features, x: &Features, rate: f32) {
    for (m, v) in mean.iter_mut().zip(x) {
        *m += rate * (v - *m);
    }
    let n = mean.iter().map(|v| v * v).sum::<f32>().sqrt();
    if n > 0.0 {
        mean.iter_mut().for_each(|v| *v /= n);
    }
}

/// Online logistic regression: "is this the subject?" over a descriptor.
#[derive(Debug, Clone)]
pub struct Classifier {
    w: Features,
    b: f32,
}

impl Default for Classifier {
    fn default() -> Self {
        Self {
            w: [0.0; FEAT_DIM],
            b: 0.0,
        }
    }
}

impl Classifier {
    /// Probability that `x` is the subject.
    pub fn score(&self, x: &Features) -> f32 {
        let z: f32 = self.w.iter().zip(x).map(|(w, v)| w * v).sum::<f32>() + self.b;
        1.0 / (1.0 + (-z).exp())
    }

    /// One stochastic gradient step on `x` labelled `positive`.
    pub fn train(&mut self, x: &Features, positive: bool, lr: f32, l2: f32) {
        let y = if positive { 1.0 } else { 0.0 };
        let g = y - self.score(x);
        let decay = 1.0 - lr * l2;
        for (w, v) in self.w.iter_mut().zip(x) {
            *w = *w * decay + lr * g * v;
        }
        self.b += lr * g;
    }
}
