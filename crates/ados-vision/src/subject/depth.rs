//! Metric depth on the subject, read out of a `vision.depth.v2` frame.
//!
//! The box's lower-central region (the middle 40 % of its width, from 45 %
//! to 85 % of its height: hips and legs) is mostly the subject: the head
//! region often mixes in the background behind it and the feet the ground.
//! The cue is the median depth of that region's pixels, its sigma the
//! median of their sigma plane combined with the spread of their depths. A
//! region whose depths spread widely (the box straddles the subject and the
//! scene behind) gives no cue.

use ados_protocol::vision_depth_frame::DepthFrame;

use super::range::DepthCue;

/// The region sampled, as fractions of the box: `(x0, x1)` across its width,
/// `(y0, y1)` down its height.
const REGION_X: (f32, f32) = (0.3, 0.7);
const REGION_Y: (f32, f32) = (0.45, 0.85);

/// The largest robust spread (1.4826 * median absolute deviation) over the
/// median depth taken as one surface.
const MAX_SPREAD: f32 = 0.15;

/// The depth cue for a subject boxed at `bbox` (normalised image
/// coordinates `[x0, y0, x1, y1]`, `0..1`, y down) in `frame`. `ray_at`
/// gives the NED line of sight through a normalised image point, scaled so
/// its optical-axis component is 1. `None` when the region holds no depth or
/// does not look like one surface.
pub fn depth_cue(
    frame: &DepthFrame,
    bbox: [f32; 4],
    ray_at: impl Fn(f32, f32) -> Option<[f32; 3]>,
) -> Option<DepthCue> {
    let (w, h) = (usize::from(frame.w), usize::from(frame.h));
    if w == 0 || h == 0 || frame.depth_mm.len() != w * h || frame.sigma_mm.len() != w * h {
        return None;
    }
    let [bx0, by0, bx1, by1] = bbox;
    let (bw, bh) = (bx1 - bx0, by1 - by0);
    if !(bw > 0.0 && bh > 0.0) {
        return None;
    }
    let (u0, u1) = (bx0 + REGION_X.0 * bw, bx0 + REGION_X.1 * bw);
    let (v0, v1) = (by0 + REGION_Y.0 * bh, by0 + REGION_Y.1 * bh);
    // Pixel (i, j) has its centre at ((i + 0.5) / w, (j + 0.5) / h). A
    // region narrower than a pixel still takes the pixel under its centre.
    let span = |lo: f32, hi: f32, n: usize| {
        let n_f = n as f32;
        let first = (lo * n_f - 0.5).ceil().max(0.0) as usize;
        let last = ((hi * n_f - 0.5).floor().max(0.0) as usize).min(n - 1);
        if first <= last {
            (first, last)
        } else {
            let mid = (((lo + hi) * 0.5 * n_f) as usize).min(n - 1);
            (mid, mid)
        }
    };
    let (i0, i1) = span(u0.max(0.0), u1.min(1.0), w);
    let (j0, j1) = span(v0.max(0.0), v1.min(1.0), h);
    let mut depths = Vec::with_capacity((i1 - i0 + 1) * (j1 - j0 + 1));
    let mut sigmas = Vec::with_capacity(depths.capacity());
    for j in j0..=j1 {
        for i in i0..=i1 {
            let k = j * w + i;
            if frame.depth_mm[k] > 0 {
                depths.push(f32::from(frame.depth_mm[k]) / 1000.0);
                sigmas.push(f32::from(frame.sigma_mm[k]) / 1000.0);
            }
        }
    }
    let n = depths.len();
    if n == 0 {
        return None;
    }
    let z = median(&mut depths);
    let sigma = median(&mut sigmas);
    for d in &mut depths {
        *d = (*d - z).abs();
    }
    let spread = 1.4826 * median(&mut depths);
    if spread > MAX_SPREAD * z {
        return None;
    }
    let sigma = (sigma * sigma + spread * spread / n as f32).sqrt();
    let ray = ray_at(0.5 * (u0 + u1), 0.5 * (v0 + v1))?;
    DepthCue::along_ray(z, sigma.max(0.01), ray)
}

fn median(v: &mut [f32]) -> f32 {
    let mid = v.len() / 2;
    *v.select_nth_unstable_by(mid, f32::total_cmp).1
}

#[cfg(test)]
mod tests {
    use super::*;
    use ados_protocol::vision_depth_frame::{DepthFit, FRAME_COLS, FRAME_ROWS};

    fn frame(background_mm: u16) -> DepthFrame {
        let (w, h) = (usize::from(FRAME_COLS), usize::from(FRAME_ROWS));
        DepthFrame {
            camera_id: "main".into(),
            stamp_ns: 0,
            w: FRAME_COLS,
            h: FRAME_ROWS,
            fx: 100.0,
            fy: 75.0,
            cx: 64.0,
            cy: 48.0,
            pitch_down_deg: 0.0,
            depth_mm: vec![background_mm; w * h],
            sigma_mm: vec![400; w * h],
            fit: DepthFit {
                scale: 1.0,
                shift: 0.0,
                inliers: 5,
                residual_m: 0.2,
                held_ms: 0,
            },
        }
    }

    /// Paint a person at `z_mm` over the box `[x0, y0, x1, y1]`.
    fn paint(f: &mut DepthFrame, b: [f32; 4], z_mm: u16) {
        let (w, h) = (usize::from(f.w), usize::from(f.h));
        for j in 0..h {
            for i in 0..w {
                let (u, v) = ((i as f32 + 0.5) / w as f32, (j as f32 + 0.5) / h as f32);
                if u >= b[0] && u <= b[2] && v >= b[1] && v <= b[3] {
                    f.depth_mm[j * w + i] = z_mm;
                }
            }
        }
    }

    const LEVEL: fn(f32, f32) -> Option<[f32; 3]> = |_, _| Some([1.0, 0.0, 0.0]);

    #[test]
    fn the_subject_depth_is_read_over_the_background() {
        let mut f = frame(30_000);
        let b = [0.45, 0.3, 0.55, 0.7];
        paint(&mut f, b, 8_000);
        let cue = depth_cue(&f, b, LEVEL).unwrap();
        assert!((cue.horizontal_m - 8.0).abs() < 1e-3, "{cue:?}");
        assert!((cue.sigma_m - 0.4).abs() < 0.01);
    }

    #[test]
    fn a_region_straddling_two_surfaces_gives_no_cue() {
        let mut f = frame(30_000);
        let b = [0.45, 0.3, 0.55, 0.7];
        // Only the left half of the box is the subject.
        paint(&mut f, [0.45, 0.3, 0.5, 0.7], 8_000);
        assert!(depth_cue(&f, b, LEVEL).is_none());
    }

    #[test]
    fn a_tiny_box_and_an_unknown_region_are_handled() {
        let mut f = frame(0);
        let tiny = [0.5, 0.5, 0.502, 0.503];
        assert!(depth_cue(&f, tiny, LEVEL).is_none());
        paint(&mut f, [0.48, 0.48, 0.52, 0.52], 12_000);
        let cue = depth_cue(&f, tiny, LEVEL).unwrap();
        assert!((cue.horizontal_m - 12.0).abs() < 1e-3);
    }
}
