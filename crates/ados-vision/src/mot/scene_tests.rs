//! Deterministic synthetic scenes for the tracker tests: walkers on a straight
//! line, optional hidden spells, occlusion by nearer walkers, detector jitter
//! and misses, a panning camera, and an optional rendered frame so appearance
//! descriptors come from real pixels.

use ados_protocol::framebus::{BoundingBox, Detection};

use super::{describe, Features, Homography, MultiObjectTracker};

/// Small deterministic generator (xorshift64*).
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self(seed.max(1))
    }

    /// Uniform in `0..1`.
    pub fn unit(&mut self) -> f32 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        (self.0.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 40) as f32 / (1u64 << 24) as f32
    }

    pub fn range(&mut self, lo: f32, hi: f32) -> f32 {
        lo + (hi - lo) * self.unit()
    }
}

#[derive(Debug, Clone)]
pub struct Walker {
    /// Centre at `t = 0`, world pixels (before the camera pan).
    pub x: f32,
    pub y: f32,
    /// Velocity, pixels per second.
    pub vx: f32,
    pub vy: f32,
    pub w: f32,
    pub h: f32,
    /// Upper and lower clothing colours, for rendered scenes.
    pub top: [u8; 3],
    pub bottom: [u8; 3],
    /// Spells `[from, to)` in seconds when the detector does not see it.
    pub hidden: Vec<(f64, f64)>,
    /// Spells when it is seen partly hidden: low score, top part only.
    pub partial: Vec<(f64, f64)>,
    /// Position jumps `(at, x, y)`: from `at` on it walks from `(x, y)`.
    pub jumps: Vec<(f64, f32, f32)>,
}

impl Walker {
    pub fn new(x: f32, y: f32, vx: f32, w: f32, h: f32) -> Self {
        Self {
            x,
            y,
            vx,
            vy: 0.0,
            w,
            h,
            top: [200, 30, 30],
            bottom: [30, 30, 200],
            hidden: Vec::new(),
            partial: Vec::new(),
            jumps: Vec::new(),
        }
    }

    pub fn colours(mut self, top: [u8; 3], bottom: [u8; 3]) -> Self {
        self.top = top;
        self.bottom = bottom;
        self
    }

    fn center(&self, t: f64) -> (f32, f32) {
        let (mut x0, mut y0, mut t0) = (self.x, self.y, 0.0);
        for &(at, x, y) in &self.jumps {
            if t >= at {
                (x0, y0, t0) = (x, y, at);
            }
        }
        let dt = (t - t0) as f32;
        (x0 + self.vx * dt, y0 + self.vy * dt)
    }

    fn in_spell(spells: &[(f64, f64)], t: f64) -> bool {
        spells.iter().any(|&(a, b)| t >= a && t < b)
    }
}

pub struct Scene {
    pub walkers: Vec<Walker>,
    pub width: f32,
    pub height: f32,
    pub dt: f64,
    /// Camera pan as an image shift in pixels at time `t`.
    pub pan: fn(f64) -> f32,
    pub jitter_px: f32,
    pub miss_rate: f32,
    /// Hide walkers mostly covered by nearer ones (nearer = lower in image).
    pub occlusion: bool,
    /// Render frames and describe every box from its pixels.
    pub render: bool,
}

pub struct Frame {
    pub t_ns: u64,
    pub dets: Vec<Detection>,
    pub truth: Vec<usize>,
    pub features: Vec<Option<Features>>,
    /// Previous frame's pixels to this frame's.
    pub warp: Homography,
}

pub fn no_pan(_: f64) -> f32 {
    0.0
}

pub fn person(x: f32, y: f32, w: f32, h: f32, score: f32) -> Detection {
    Detection {
        bbox: Some(BoundingBox {
            x: x - w / 2.0,
            y: y - h / 2.0,
            width: w,
            height: h,
        }),
        class_label: "person".into(),
        confidence: score,
        track_id: None,
        assoc_confidence: None,
        lock_state: None,
        attributes: None,
        mask: None,
        keypoints: None,
        depth: None,
        world_pos: None,
        track_state: None,
        appearance_id: None,
    }
}

impl Scene {
    pub fn new(walkers: Vec<Walker>) -> Self {
        Self {
            walkers,
            width: 1280.0,
            height: 720.0,
            dt: 0.1,
            pan: no_pan,
            jitter_px: 0.0,
            miss_rate: 0.0,
            occlusion: false,
            render: false,
        }
    }

    /// Frames from `t = 0` up to `duration` seconds.
    pub fn frames(&self, duration: f64, seed: u64) -> Vec<Frame> {
        let mut rng = Rng::new(seed);
        let n = (duration / self.dt).round() as usize;
        (0..n)
            .map(|k| self.frame(k as f64 * self.dt, &mut rng))
            .collect()
    }

    fn frame(&self, t: f64, rng: &mut Rng) -> Frame {
        let pan = (self.pan)(t);
        let shift = pan - (self.pan)((t - self.dt).max(0.0));
        let boxes: Vec<(usize, f32, f32, f32, f32)> = self
            .walkers
            .iter()
            .enumerate()
            .map(|(i, w)| {
                let (x, y) = w.center(t);
                (i, x - pan, y, w.w, w.h)
            })
            .collect();
        let mut pixels = self.render.then(|| self.paint(&boxes));
        let mut frame = Frame {
            t_ns: (t * 1e9).round() as u64,
            dets: Vec::new(),
            truth: Vec::new(),
            features: Vec::new(),
            warp: Homography::translation(-shift, 0.0),
        };
        for &(i, x, y, w, h) in &boxes {
            let walker = &self.walkers[i];
            if Walker::in_spell(&walker.hidden, t)
                || x + w / 2.0 < 0.0
                || x - w / 2.0 > self.width
                || (self.occlusion && covered(&boxes, i) > 0.7)
            {
                continue;
            }
            if self.miss_rate > 0.0 && rng.unit() < self.miss_rate {
                continue;
            }
            let (jx, jy) = (
                rng.range(-self.jitter_px, self.jitter_px),
                rng.range(-self.jitter_px, self.jitter_px),
            );
            let partial = Walker::in_spell(&walker.partial, t)
                || (self.occlusion && covered(&boxes, i) > 0.35);
            let det = if partial {
                let ph = h * 0.6;
                person(x + jx, y - (h - ph) / 2.0 + jy, w, ph, 0.3)
            } else {
                person(x + jx, y + jy, w, h, 0.9)
            };
            let feat = pixels.as_mut().and_then(|p| {
                let mut f = [0.0; super::FEAT_DIM];
                let b = det.bbox.as_ref()?;
                describe(p, self.width as u32, self.height as u32, b, &mut f).then_some(f)
            });
            frame.features.push(feat);
            frame.dets.push(det);
            frame.truth.push(i);
        }
        frame
    }

    /// A grey frame with every walker painted, far ones first.
    fn paint(&self, boxes: &[(usize, f32, f32, f32, f32)]) -> Vec<u8> {
        let (fw, fh) = (self.width as usize, self.height as usize);
        let mut p = vec![110u8; fw * fh * 3];
        let mut order: Vec<_> = boxes.to_vec();
        order.sort_by(|a, b| (a.2 + a.4 / 2.0).total_cmp(&(b.2 + b.4 / 2.0)));
        for (i, x, y, w, h) in order {
            let wk = &self.walkers[i];
            let (x0, x1) = (
                (x - w / 2.0).max(0.0) as usize,
                ((x + w / 2.0) as usize).min(fw),
            );
            let (y0, y1) = (
                (y - h / 2.0).max(0.0) as usize,
                ((y + h / 2.0) as usize).min(fh),
            );
            for row in y0..y1 {
                let c = if (row as f32) < y { wk.top } else { wk.bottom };
                for col in x0..x1 {
                    p[(row * fw + col) * 3..][..3].copy_from_slice(&c);
                }
            }
        }
        p
    }
}

/// Fraction of walker `i`'s box covered by nearer walkers' boxes (the largest
/// single cover).
fn covered(boxes: &[(usize, f32, f32, f32, f32)], i: usize) -> f32 {
    let (_, x, y, w, h) = boxes[i];
    let bottom = y + h / 2.0;
    boxes
        .iter()
        .filter(|o| o.0 != i && o.2 + o.4 / 2.0 > bottom)
        .map(|&(_, ox, oy, ow, oh)| {
            let iw = ((x + w / 2.0).min(ox + ow / 2.0) - (x - w / 2.0).max(ox - ow / 2.0)).max(0.0);
            let ih = ((y + h / 2.0).min(oy + oh / 2.0) - (y - h / 2.0).max(oy - oh / 2.0)).max(0.0);
            iw * ih / (w * h)
        })
        .fold(0.0, f32::max)
}

/// The id each detection got, per frame, after running `frames` through
/// `mot` (warping by each frame's camera motion when `warp`).
pub fn run(mot: &mut MultiObjectTracker, frames: &[Frame], warp: bool) -> Vec<Vec<Option<u32>>> {
    frames
        .iter()
        .map(|f| {
            if warp {
                mot.warp_homography(&f.warp);
            }
            mot.update(&f.dets, &f.features, Some(f.t_ns));
            (0..f.dets.len())
                .map(|i| mot.output(i).and_then(|o| o.track_id))
                .collect()
        })
        .collect()
}

/// Identity metrics of a run against the scene's truth.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct IdMetrics {
    pub gt_dets: usize,
    pub id_switches: usize,
    pub idf1: f32,
}

pub fn score(frames: &[Frame], ids: &[Vec<Option<u32>>]) -> IdMetrics {
    use std::collections::BTreeMap;
    let mut last: BTreeMap<usize, u32> = BTreeMap::new();
    let mut pairs: BTreeMap<(usize, u32), usize> = BTreeMap::new();
    let (mut gt_dets, mut id_dets, mut id_switches) = (0, 0, 0);
    for (f, frame_ids) in frames.iter().zip(ids) {
        for (&gt, id) in f.truth.iter().zip(frame_ids) {
            gt_dets += 1;
            let Some(id) = *id else { continue };
            id_dets += 1;
            *pairs.entry((gt, id)).or_default() += 1;
            if last.insert(gt, id).is_some_and(|prev| prev != id) {
                id_switches += 1;
            }
        }
    }
    // One-to-one id to truth mapping, largest overlap first.
    let mut ranked: Vec<_> = pairs.into_iter().collect();
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    let (mut used_gt, mut used_id) = (Vec::new(), Vec::new());
    let mut idtp = 0;
    for ((gt, id), n) in ranked {
        if !used_gt.contains(&gt) && !used_id.contains(&id) {
            used_gt.push(gt);
            used_id.push(id);
            idtp += n;
        }
    }
    let denom = (gt_dets + id_dets).max(1) as f32;
    IdMetrics {
        gt_dets,
        id_switches,
        idf1: 2.0 * idtp as f32 / denom,
    }
}
