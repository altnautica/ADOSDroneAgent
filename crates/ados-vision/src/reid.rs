//! Re-id (appearance) embedding helpers: crop a detection's box out of a frame,
//! resize it to the embedding model's input, and (for the in-process ONNX path)
//! normalize it into the model's expected tensor. The embedding the model
//! returns is L2-normalized so the tracker's cosine similarity is a clean dot
//! product, the standard re-id distance.
//!
//! Why the crop lives here (the engine), not in a backend: both the in-process
//! ONNX path and the Python RKNN sidecar consume the SAME model-input-sized
//! rgb24 crop, so cropping + resizing once in the engine keeps the two paths
//! byte-identical at the crop boundary. The ONNX path then applies the ImageNet
//! normalization in code (the detector path divides by 255 the same way — the
//! norm is applied outside the model, not folded into it); the RKNN path feeds
//! the raw uint8 crop and the converted `.rknn` folds the same normalization via
//! its `rknn.config(mean, std)`. The two must agree numerically — the SITL
//! distractor-survival check is the parity gate.

use ados_protocol::framebus::BoundingBox;

/// OSNet / ImageNet channel means (RGB order), in 0-255 pixel units.
pub const OSNET_MEAN: [f32; 3] = [123.675, 116.28, 103.53];
/// OSNet / ImageNet channel std-devs (RGB order), in 0-255 pixel units.
pub const OSNET_STD: [f32; 3] = [58.395, 57.12, 57.375];

/// Crop `bbox` out of an `rgb24` `frame` (`frame_w` x `frame_h`) and bilinearly
/// resize it to `out_w` x `out_h`, returning a packed rgb24 buffer
/// (`out_w*out_h*3` bytes). The box is in source-frame pixels (the same units a
/// `Detection` carries); it is clamped to the frame, and a degenerate (sub-pixel
/// or out-of-frame) box or an undersized frame returns `None`.
pub fn crop_resize_rgb24(
    frame: &[u8],
    frame_w: u32,
    frame_h: u32,
    bbox: &BoundingBox,
    out_w: u32,
    out_h: u32,
) -> Option<Vec<u8>> {
    if out_w == 0 || out_h == 0 || frame_w == 0 || frame_h == 0 {
        return None;
    }
    let expected = frame_w as usize * frame_h as usize * 3;
    if frame.len() < expected {
        return None;
    }
    let fw = frame_w as f32;
    let fh = frame_h as f32;
    // Clamp the box to the frame.
    let x0 = bbox.x.clamp(0.0, fw);
    let y0 = bbox.y.clamp(0.0, fh);
    let x1 = (bbox.x + bbox.width).clamp(0.0, fw);
    let y1 = (bbox.y + bbox.height).clamp(0.0, fh);
    let bw = x1 - x0;
    let bh = y1 - y0;
    if bw < 1.0 || bh < 1.0 {
        return None;
    }

    let mut out = vec![0u8; out_w as usize * out_h as usize * 3];
    let stride = frame_w as usize * 3;
    let max_x = frame_w as i64 - 1;
    let max_y = frame_h as i64 - 1;

    for oy in 0..out_h {
        // Map the output row centre to a source y inside the box.
        let sy = y0 + (oy as f32 + 0.5) * bh / out_h as f32 - 0.5;
        let sy = sy.clamp(0.0, fh - 1.0);
        let y_lo = sy.floor() as i64;
        let wy = sy - y_lo as f32;
        let y_lo = y_lo.clamp(0, max_y) as usize;
        let y_hi = (y_lo as i64 + 1).clamp(0, max_y) as usize;
        for ox in 0..out_w {
            let sx = x0 + (ox as f32 + 0.5) * bw / out_w as f32 - 0.5;
            let sx = sx.clamp(0.0, fw - 1.0);
            let x_lo = sx.floor() as i64;
            let wx = sx - x_lo as f32;
            let x_lo = x_lo.clamp(0, max_x) as usize;
            let x_hi = (x_lo as i64 + 1).clamp(0, max_x) as usize;

            let o = (oy as usize * out_w as usize + ox as usize) * 3;
            for c in 0..3usize {
                let p00 = frame[y_lo * stride + x_lo * 3 + c] as f32;
                let p01 = frame[y_lo * stride + x_hi * 3 + c] as f32;
                let p10 = frame[y_hi * stride + x_lo * 3 + c] as f32;
                let p11 = frame[y_hi * stride + x_hi * 3 + c] as f32;
                let top = p00 * (1.0 - wx) + p01 * wx;
                let bot = p10 * (1.0 - wx) + p11 * wx;
                out[o + c] = (top * (1.0 - wy) + bot * wy).round().clamp(0.0, 255.0) as u8;
            }
        }
    }
    Some(out)
}

/// Normalize a packed rgb24 crop (`w` x `h`) into the OSNet input tensor: NCHW
/// f32 with `(pixel - mean) / std` per channel (ImageNet stats). This is the
/// in-process ONNX path; the RKNN path sends the raw uint8 crop and lets the
/// converted model fold the same normalization.
pub fn preprocess_osnet_nchw(crop: &[u8], w: u32, h: u32) -> Option<Vec<f32>> {
    let plane = w as usize * h as usize;
    if crop.len() < plane * 3 {
        return None;
    }
    let mut out = vec![0f32; 3 * plane];
    for i in 0..plane {
        for c in 0..3usize {
            let v = crop[i * 3 + c] as f32;
            out[c * plane + i] = (v - OSNET_MEAN[c]) / OSNET_STD[c];
        }
    }
    Some(out)
}

/// L2-normalize a feature vector in place (a zero vector is left untouched). The
/// tracker scores appearances with cosine similarity, so a unit-norm embedding
/// makes that a plain dot product, the canonical re-id distance.
pub fn l2_normalize(v: &mut [f32]) {
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        for x in v.iter_mut() {
            *x /= norm;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid_frame(w: u32, h: u32, rgb: [u8; 3]) -> Vec<u8> {
        let mut f = vec![0u8; (w * h * 3) as usize];
        for px in f.chunks_exact_mut(3) {
            px.copy_from_slice(&rgb);
        }
        f
    }

    #[test]
    fn crop_of_a_solid_frame_is_that_solid_colour() {
        let frame = solid_frame(64, 64, [10, 20, 30]);
        let bbox = BoundingBox {
            x: 8.0,
            y: 8.0,
            width: 32.0,
            height: 32.0,
        };
        let crop = crop_resize_rgb24(&frame, 64, 64, &bbox, 256, 128).expect("crop");
        assert_eq!(crop.len(), 256 * 128 * 3);
        // Every pixel of a solid-colour crop is that colour (within rounding).
        for px in crop.chunks_exact(3) {
            assert_eq!(px, &[10, 20, 30]);
        }
    }

    #[test]
    fn degenerate_or_out_of_frame_box_is_none() {
        let frame = solid_frame(32, 32, [0, 0, 0]);
        let zero = BoundingBox {
            x: 5.0,
            y: 5.0,
            width: 0.0,
            height: 10.0,
        };
        assert!(crop_resize_rgb24(&frame, 32, 32, &zero, 256, 128).is_none());
        let off = BoundingBox {
            x: 100.0,
            y: 100.0,
            width: 10.0,
            height: 10.0,
        };
        assert!(crop_resize_rgb24(&frame, 32, 32, &off, 256, 128).is_none());
    }

    #[test]
    fn osnet_preprocess_applies_imagenet_norm_in_nchw() {
        // A 1x1 white pixel: each channel becomes (255 - mean)/std.
        let crop = vec![255u8, 255, 255];
        let t = preprocess_osnet_nchw(&crop, 1, 1).expect("preprocess");
        assert_eq!(t.len(), 3);
        for c in 0..3 {
            let expect = (255.0 - OSNET_MEAN[c]) / OSNET_STD[c];
            assert!(
                (t[c] - expect).abs() < 1e-4,
                "channel {c}: {} vs {expect}",
                t[c]
            );
        }
    }

    #[test]
    fn l2_normalize_makes_unit_norm() {
        let mut v = vec![3.0f32, 4.0];
        l2_normalize(&mut v);
        assert!((v[0] - 0.6).abs() < 1e-6);
        assert!((v[1] - 0.8).abs() < 1e-6);
        let mut z = vec![0.0f32, 0.0];
        l2_normalize(&mut z);
        assert_eq!(z, vec![0.0, 0.0]);
    }
}
