//! Dense model outputs: a per-pixel map a depth model produces, and the input
//! preparation such a model takes.

use anyhow::{anyhow, Result};

/// The ImageNet channel means and standard deviations a monocular depth
/// model (MiDaS-class) is trained on.
const IMAGENET_MEAN: [f32; 3] = [0.485, 0.456, 0.406];
const IMAGENET_STD: [f32; 3] = [0.229, 0.224, 0.225];

/// One dense model output: `width` x `height` values, row-major from the
/// top-left, covering the whole frame. For a depth model each value is a
/// relative inverse depth: larger is nearer, and the scale is unknown.
#[derive(Debug, Clone, PartialEq)]
pub struct DenseMap {
    pub width: u32,
    pub height: u32,
    pub data: Vec<f32>,
}

impl DenseMap {
    /// A map whose `data` length matches its size.
    pub fn new(width: u32, height: u32, data: Vec<f32>) -> Result<Self> {
        if width == 0 || height == 0 || data.len() != width as usize * height as usize {
            return Err(anyhow!(
                "dense map of {} values is not {width}x{height}",
                data.len()
            ));
        }
        Ok(Self {
            width,
            height,
            data,
        })
    }

    /// The value at normalised frame coordinates (`0..1`, y down), nearest
    /// sample.
    pub fn at(&self, u: f32, v: f32) -> f32 {
        let x = ((u * self.width as f32) as i64).clamp(0, self.width as i64 - 1) as usize;
        let y = ((v * self.height as f32) as i64).clamp(0, self.height as i64 - 1) as usize;
        self.data[y * self.width as usize + x]
    }
}

/// Resize an RGB24 frame to `iw` x `ih` (nearest sample, the whole frame
/// squeezed to the model's input, as a depth model is run), normalise each
/// channel with the ImageNet statistics, and lay it out NCHW. `None` when the
/// buffer is shorter than the frame it claims to be.
pub fn preprocess_depth_nchw(frame: &[u8], w: u32, h: u32, iw: u32, ih: u32) -> Option<Vec<f32>> {
    let (w, h, iw, ih) = (w as usize, h as usize, iw as usize, ih as usize);
    if w == 0 || h == 0 || iw == 0 || ih == 0 || frame.len() < w * h * 3 {
        return None;
    }
    let plane = iw * ih;
    let mut out = vec![0.0f32; plane * 3];
    for y in 0..ih {
        let sy = (y * h) / ih;
        for x in 0..iw {
            let sx = (x * w) / iw;
            let p = (sy * w + sx) * 3;
            for c in 0..3 {
                let v = f32::from(frame[p + c]) / 255.0;
                out[c * plane + y * iw + x] = (v - IMAGENET_MEAN[c]) / IMAGENET_STD[c];
            }
        }
    }
    Some(out)
}

/// The synthetic relative inverse depth a mock model returns: a level ground
/// plane below the image centre line (nearer toward the bottom) and a far
/// background above it, with a near vertical post at a quarter of the width.
/// Deterministic, so hardware-free runs and tests see a stable scene.
pub fn synthetic_depth(width: u32, height: u32) -> DenseMap {
    let (w, h) = (width as usize, height as usize);
    let mut data = vec![0.0f32; w * h];
    for y in 0..h {
        let v = (y as f32 + 0.5) / h as f32;
        let ground = if v > 0.5 { (v - 0.5) * 2.0 } else { 0.02 };
        for x in 0..w {
            let u = (x as f32 + 0.5) / w as f32;
            let post = (0.22..0.28).contains(&u) && (0.25..0.75).contains(&v);
            data[y * w + x] = if post { 0.8 } else { ground };
        }
    }
    DenseMap {
        width,
        height,
        data,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preprocessing_squeezes_the_frame_and_normalises_each_channel() {
        // A 4x2 frame: left half black, right half white.
        let mut frame = vec![0u8; 4 * 2 * 3];
        for y in 0..2 {
            for x in 2..4 {
                for c in 0..3 {
                    frame[(y * 4 + x) * 3 + c] = 255;
                }
            }
        }
        let out = preprocess_depth_nchw(&frame, 4, 2, 2, 2).unwrap();
        assert_eq!(out.len(), 12);
        // Channel 0: left column black, right column white.
        assert!((out[0] - (0.0 - 0.485) / 0.229).abs() < 1e-5);
        assert!((out[1] - (1.0 - 0.485) / 0.229).abs() < 1e-5);
        // Channel 2 starts at plane 2.
        assert!((out[8] - (0.0 - 0.406) / 0.225).abs() < 1e-5);
        assert!(preprocess_depth_nchw(&frame[..5], 4, 2, 2, 2).is_none());
    }

    #[test]
    fn a_map_samples_by_normalised_coordinate_and_checks_its_size() {
        let m = DenseMap::new(2, 2, vec![1.0, 2.0, 3.0, 4.0]).unwrap();
        assert_eq!(m.at(0.0, 0.0), 1.0);
        assert_eq!(m.at(0.99, 0.0), 2.0);
        assert_eq!(m.at(0.1, 0.9), 3.0);
        assert_eq!(m.at(1.5, 1.5), 4.0);
        assert!(DenseMap::new(2, 2, vec![1.0]).is_err());
    }

    #[test]
    fn the_synthetic_scene_has_a_near_post_and_a_ground_that_nears_downward() {
        let m = synthetic_depth(64, 36);
        assert!(m.at(0.25, 0.5) > m.at(0.75, 0.5));
        assert!(m.at(0.75, 0.95) > m.at(0.75, 0.6));
        assert!(m.at(0.75, 0.1) < 0.05);
    }
}
