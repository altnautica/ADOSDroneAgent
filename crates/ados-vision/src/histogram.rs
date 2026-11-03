//! A model-free appearance descriptor computed from a detection's pixels.
//!
//! The fallback when no learned re-id model is loaded: a coarse colour
//! histogram of the box, split into an upper and a lower half (torso and legs
//! for a standing person), so two people in differently coloured clothing score
//! low and the same person across frames scores high. Each half contributes
//! [`HALF_BINS`] bins:
//!
//! - six hue sectors (red, yellow, green, cyan, blue, magenta), each weighted
//!   by the pixel's chroma, so a saturated colour lands in its sector;
//! - two achromatic bins, dark and light, weighted by the pixel's lack of
//!   chroma and split by its luma, so black, grey and white clothing still
//!   separate.
//!
//! Each half is normalized to sum 1 (exposure changes shift every bin alike),
//! and the whole descriptor is L2-normalized so the tracker's cosine similarity
//! is a plain dot product. Its length is [`crate::tracker::APPEARANCE_DIM`].
//! A learned embedding is far more discriminative; this exists so appearance
//! gating degrades rather than disappears when the model file is absent.

use ados_protocol::framebus::BoundingBox;

use crate::reid::{crop_resize_rgb24, l2_normalize};
use crate::tracker::APPEARANCE_DIM;

/// Bins per vertical half: six hue sectors plus dark and light achromatic.
pub const HALF_BINS: usize = APPEARANCE_DIM / 2;
/// The box is resampled to this size before binning, so the descriptor's cost
/// is fixed whatever the box size.
const SAMPLE_W: u32 = 16;
const SAMPLE_H: u32 = 32;

/// The histogram descriptor of `bbox` in an `rgb24` `frame` (`frame_w` x
/// `frame_h`), L2-normalized, of length [`crate::tracker::APPEARANCE_DIM`].
/// `None` when the box is degenerate or outside the frame.
pub fn histogram_descriptor(
    frame: &[u8],
    frame_w: u32,
    frame_h: u32,
    bbox: &BoundingBox,
) -> Option<Vec<f32>> {
    let crop = crop_resize_rgb24(frame, frame_w, frame_h, bbox, SAMPLE_W, SAMPLE_H)?;
    let mut out = vec![0f32; APPEARANCE_DIM];
    let row_len = SAMPLE_W as usize * 3;
    for (row, line) in crop.chunks_exact(row_len).enumerate() {
        let half = if row < SAMPLE_H as usize / 2 { 0 } else { 1 };
        let bins = &mut out[half * HALF_BINS..(half + 1) * HALF_BINS];
        for &[r, g, b] in line.as_chunks::<3>().0 {
            add_pixel(bins, r, g, b);
        }
    }
    for half in out.as_chunks_mut::<HALF_BINS>().0 {
        let sum: f32 = half.iter().sum();
        if sum > 0.0 {
            for v in half.iter_mut() {
                *v /= sum;
            }
        }
    }
    l2_normalize(&mut out);
    Some(out)
}

/// Accumulate one pixel into a half's eight bins.
fn add_pixel(bins: &mut [f32], r: u8, g: u8, b: u8) {
    let (r, g, b) = (r as f32, g as f32, b as f32);
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let chroma = (max - min) / 255.0;
    if chroma > 0.0 {
        bins[hue_sector(r, g, b, max, max - min)] += chroma;
    }
    let luma = (0.299 * r + 0.587 * g + 0.114 * b) / 255.0;
    let achromatic = 1.0 - chroma;
    bins[6] += achromatic * (1.0 - luma);
    bins[7] += achromatic * luma;
}

/// The hue sector `0..6` of a pixel with non-zero chroma `delta` (the standard
/// HSV hue, in sixths of the circle, centred on the primaries and secondaries).
fn hue_sector(r: f32, g: f32, b: f32, max: f32, delta: f32) -> usize {
    let h = if max == r {
        ((g - b) / delta).rem_euclid(6.0)
    } else if max == g {
        (b - r) / delta + 2.0
    } else {
        (r - g) / delta + 4.0
    };
    // Shift by half a sector so each bin is centred on its colour.
    ((h + 0.5).floor() as usize) % 6
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A frame whose upper half is `top` and lower half is `bottom`.
    fn two_tone(w: u32, h: u32, top: [u8; 3], bottom: [u8; 3]) -> Vec<u8> {
        let mut f = Vec::with_capacity((w * h * 3) as usize);
        for y in 0..h {
            let c = if y < h / 2 { top } else { bottom };
            for _ in 0..w {
                f.extend_from_slice(&c);
            }
        }
        f
    }

    fn whole(w: u32, h: u32) -> BoundingBox {
        BoundingBox {
            x: 0.0,
            y: 0.0,
            width: w as f32,
            height: h as f32,
        }
    }

    fn dot(a: &[f32], b: &[f32]) -> f32 {
        a.iter().zip(b).map(|(x, y)| x * y).sum()
    }

    #[test]
    fn the_descriptor_is_unit_length_and_fixed_size() {
        let f = two_tone(40, 80, [200, 30, 30], [30, 30, 200]);
        let d = histogram_descriptor(&f, 40, 80, &whole(40, 80)).unwrap();
        assert_eq!(d.len(), APPEARANCE_DIM);
        assert!((dot(&d, &d) - 1.0).abs() < 1e-4);
    }

    #[test]
    fn the_same_clothing_matches_across_exposure_and_box_size() {
        let a = two_tone(40, 80, [200, 30, 30], [30, 30, 200]);
        let b = two_tone(20, 60, [170, 26, 26], [26, 26, 170]);
        let da = histogram_descriptor(&a, 40, 80, &whole(40, 80)).unwrap();
        let db = histogram_descriptor(&b, 20, 60, &whole(20, 60)).unwrap();
        assert!(dot(&da, &db) > 0.95, "same colours: {}", dot(&da, &db));
    }

    #[test]
    fn different_clothing_scores_low() {
        let red_blue = two_tone(40, 80, [200, 30, 30], [30, 30, 200]);
        let green_white = two_tone(40, 80, [30, 200, 30], [240, 240, 240]);
        let da = histogram_descriptor(&red_blue, 40, 80, &whole(40, 80)).unwrap();
        let db = histogram_descriptor(&green_white, 40, 80, &whole(40, 80)).unwrap();
        assert!(dot(&da, &db) < 0.3, "different colours: {}", dot(&da, &db));
    }

    #[test]
    fn black_and_white_separate() {
        let black = two_tone(20, 40, [10, 10, 10], [10, 10, 10]);
        let white = two_tone(20, 40, [245, 245, 245], [245, 245, 245]);
        let db = histogram_descriptor(&black, 20, 40, &whole(20, 40)).unwrap();
        let dw = histogram_descriptor(&white, 20, 40, &whole(20, 40)).unwrap();
        assert!(dot(&db, &dw) < 0.2, "black vs white: {}", dot(&db, &dw));
    }

    #[test]
    fn hue_sectors_land_on_their_primaries() {
        assert_eq!(hue_sector(255.0, 0.0, 0.0, 255.0, 255.0), 0);
        assert_eq!(hue_sector(255.0, 255.0, 0.0, 255.0, 255.0), 1);
        assert_eq!(hue_sector(0.0, 255.0, 0.0, 255.0, 255.0), 2);
        assert_eq!(hue_sector(0.0, 255.0, 255.0, 255.0, 255.0), 3);
        assert_eq!(hue_sector(0.0, 0.0, 255.0, 255.0, 255.0), 4);
        assert_eq!(hue_sector(255.0, 0.0, 255.0, 255.0, 255.0), 5);
    }

    #[test]
    fn a_box_outside_the_frame_has_no_descriptor() {
        let f = two_tone(10, 10, [1, 2, 3], [4, 5, 6]);
        let off = BoundingBox {
            x: 50.0,
            y: 50.0,
            width: 5.0,
            height: 5.0,
        };
        assert!(histogram_descriptor(&f, 10, 10, &off).is_none());
    }
}
