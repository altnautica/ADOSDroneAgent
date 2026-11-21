//! One sighting's range cues fused into a single position fix.
//!
//! The subject is taken to stand upright: a vertical segment from the feet
//! to the top of the head. Every cue gives the horizontal distance `d` from
//! the camera to that segment:
//!
//! - **box**: the box's top and bottom edges are lines of sight to the head
//!   and the feet. With `t` the depression slope of a line of sight (down
//!   over horizontal), the segment of height `H` at distance `d` spans
//!   `t_foot - t_top = H / d`, so `d = H / (t_foot - t_top)`. This holds for
//!   any camera pitch. `H` is the subject's estimated height (the prior
//!   1.70 ± 0.12 m until another cue measures it), and its uncertainty is a
//!   bias shared by every frame, kept apart from the per-frame edge jitter;
//! - **ground**: the line of sight to the feet meets the ground under the
//!   subject at `d = (ground_down - camera_down) / t_foot`;
//! - **depth**: metric depth sampled inside the box ([`super::depth_cue`]).
//!
//! The cues are fused by inverse variance. When the box and another cue both
//! measure, the other cue also measures the subject's height (`H = d *
//! (t_foot - t_top)`), so the box ranges well after the other cue is lost.

use super::mat::Mat2;
use super::scalar::Scalar;
use ados_protocol::vision_subject::SubjectSources;

/// One sighting of the subject in a camera frame, in local NED.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Sighting {
    /// The frame's time, ns (the clock every update uses).
    pub stamp_ns: u64,
    /// The camera's centre, NED metres.
    pub camera_ned: [f32; 3],
    /// Lines of sight (any length) through the middle of the box's top edge
    /// and of its bottom edge, NED.
    pub top_ray: [f32; 3],
    pub foot_ray: [f32; 3],
    /// Both box edges lie inside the frame, so the box spans the whole
    /// subject. A box cut by the frame border does not range by its height.
    pub full_height: bool,
    /// One sigma of a box edge's angular jitter, radians.
    pub edge_sigma_rad: f32,
    pub depth: Option<DepthCue>,
    pub ground: Option<GroundCue>,
}

/// Metric depth on the subject: the horizontal distance from the camera.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DepthCue {
    pub horizontal_m: f32,
    pub sigma_m: f32,
}

impl DepthCue {
    /// The cue for an optical-axis depth `z_m` (one sigma `sigma_m`) seen
    /// along `ray_ned`, a line of sight scaled so its optical-axis component
    /// is 1 (the point at depth `z` is `z * ray`). `None` for a depth that is
    /// not positive or a ray with no horizontal extent.
    pub fn along_ray(z_m: f32, sigma_m: f32, ray_ned: [f32; 3]) -> Option<Self> {
        let h = ray_ned[0].hypot(ray_ned[1]);
        (z_m > 0.0 && z_m.is_finite() && sigma_m > 0.0 && h > 1e-3).then_some(Self {
            horizontal_m: z_m * h,
            sigma_m: sigma_m * h,
        })
    }
}

/// The ground under the subject: its NED down coordinate.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GroundCue {
    pub down_m: f32,
    pub sigma_m: f32,
}

/// A sighting reduced to a horizontal position measurement and a vertical
/// one.
#[derive(Debug, Clone, Copy)]
pub(super) struct Fix {
    pub ne: [f64; 2],
    pub r: Mat2,
    /// The feet's down coordinate and its variance.
    pub down: f64,
    pub down_var: f64,
    /// The share of the range's uncertainty that is the height prior's bias
    /// (one sigma, metres, along the bearing).
    pub bias_sigma: f64,
    pub bearing: [f64; 2],
    pub sources: SubjectSources,
    /// The subject's height measured by the other cues, and its variance.
    pub height_obs: Option<(f64, f64)>,
}

/// The depression slope of a line of sight (down over horizontal) and its
/// horizontal unit direction; `None` for a vertical line of sight.
fn slope(ray: [f32; 3]) -> Option<(f64, [f64; 2])> {
    let [n, e, d] = ray.map(f64::from);
    let h = n.hypot(e);
    (h > 1e-6 && d.is_finite()).then(|| (d / h, [n / h, e / h]))
}

/// Inverse-variance accumulator.
#[derive(Default)]
struct Fusion {
    w: f64,
    wd: f64,
}

impl Fusion {
    fn add(&mut self, d: f64, var: f64) {
        if d.is_finite() && var > 0.0 && var.is_finite() {
            self.w += 1.0 / var;
            self.wd += d / var;
        }
    }

    fn mean(&self) -> Option<(f64, f64)> {
        (self.w > 0.0).then(|| (self.wd / self.w, 1.0 / self.w))
    }
}

/// The nearest and farthest horizontal distances a fix may claim, metres.
const MIN_RANGE_M: f64 = 0.5;
const MAX_RANGE_M: f64 = 250.0;

/// Fuse the sighting's cues. `height` is the subject's height estimate;
/// `prior_range` the predicted horizontal distance, used (loosely) when no
/// cue ranges, so the bearing still corrects the track. `None` when the
/// sighting cannot place the subject.
pub(super) fn fix(s: &Sighting, height: &Scalar, prior_range: Option<f64>) -> Option<Fix> {
    let (tf, foot_u) = slope(s.foot_ray)?;
    let (tt, top_u) = slope(s.top_ray)?;
    let cam = s.camera_ned.map(f64::from);
    let se = f64::from(s.edge_sigma_rad).max(1e-4);
    let sig_tf = se * (1.0 + tf * tf);
    let sig_tt = se * (1.0 + tt * tt);
    let delta = tf - tt;
    let span_var = sig_tf * sig_tf + sig_tt * sig_tt;
    let spans = s.full_height && delta > 1e-3;
    let mut sources = SubjectSources::default();

    let mut other = Fusion::default();
    if let Some(g) = s.ground {
        let h = f64::from(g.down_m) - cam[2];
        if h > 0.3 && tf > 0.02 {
            let sg = f64::from(g.sigma_m);
            other.add(h / tf, (sg / tf).powi(2) + (h * sig_tf / (tf * tf)).powi(2));
            sources.ground = true;
        }
    }
    if let Some(dc) = s.depth {
        if dc.horizontal_m > 0.2 {
            other.add(f64::from(dc.horizontal_m), f64::from(dc.sigma_m).powi(2));
            sources.depth = true;
        }
    }
    let other_mean = other.mean();
    let height_obs = other_mean
        .filter(|_| spans)
        .map(|(d, var)| (d * delta, delta * delta * var + d * d * span_var));

    let mut all = Fusion {
        w: other.w,
        wd: other.wd,
    };
    let mut box_cue = None;
    if spans {
        let h = height.value;
        let d = h / delta;
        let white = (h * span_var.sqrt() / (delta * delta)).powi(2);
        let bias = d * height.sigma() / h;
        all.add(d, white + bias * bias);
        box_cue = Some((1.0 / (white + bias * bias), bias));
        sources.box_height = true;
    }
    let (range, var, bias_sigma) = match all.mean() {
        Some((d, var)) => {
            let bias = box_cue.map_or(0.0, |(w, b)| w / all.w * b);
            (d, var, bias)
        }
        None => {
            let d = prior_range?;
            (d, (0.5 * d).max(2.0).powi(2), 0.0)
        }
    };
    if !(MIN_RANGE_M..=MAX_RANGE_M).contains(&range) {
        return None;
    }

    let sum = [foot_u[0] + top_u[0], foot_u[1] + top_u[1]];
    let norm = sum[0].hypot(sum[1]);
    let u = if norm > 1e-6 {
        [sum[0] / norm, sum[1] / norm]
    } else {
        foot_u
    };
    let tangential = (range * se).powi(2);
    let r = [
        [
            var * u[0] * u[0] + tangential * u[1] * u[1],
            (var - tangential) * u[0] * u[1],
        ],
        [
            (var - tangential) * u[0] * u[1],
            var * u[1] * u[1] + tangential * u[0] * u[0],
        ],
    ];
    let (down, down_var) = match s.ground.filter(|_| sources.ground) {
        Some(g) => (f64::from(g.down_m), f64::from(g.sigma_m).powi(2)),
        None => (
            cam[2] + range * tf,
            tf * tf * var + (range * sig_tf).powi(2),
        ),
    };
    Some(Fix {
        ne: [cam[0] + range * u[0], cam[1] + range * u[1]],
        r,
        down,
        down_var,
        bias_sigma,
        bearing: u,
        sources,
        height_obs,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Lines of sight from a camera at `cam` to a person of height `h` with
    /// feet at `feet`.
    fn sighting(cam: [f32; 3], feet: [f32; 3], h: f32) -> Sighting {
        let ray = |p: [f32; 3]| [p[0] - cam[0], p[1] - cam[1], p[2] - cam[2]];
        Sighting {
            stamp_ns: 0,
            camera_ned: cam,
            top_ray: ray([feet[0], feet[1], feet[2] - h]),
            foot_ray: ray(feet),
            full_height: true,
            edge_sigma_rad: 0.003,
            depth: None,
            ground: None,
        }
    }

    #[test]
    fn the_box_alone_ranges_a_prior_height_person_exactly() {
        let s = sighting([0.0, 0.0, -6.0], [6.0, 8.0, 0.0], 1.7);
        let f = fix(&s, &Scalar::new(1.7, 0.12), None).unwrap();
        assert!(
            (f.ne[0] - 6.0).abs() < 1e-4 && (f.ne[1] - 8.0).abs() < 1e-4,
            "{:?}",
            f.ne
        );
        assert!(f.down.abs() < 1e-3);
        assert!(f.sources.box_height && !f.sources.depth);
        // The bias of a 0.12 m height sigma at 10 m.
        assert!((f.bias_sigma - 10.0 * 0.12 / 1.7).abs() < 0.05);
        assert!(f.height_obs.is_none());
    }

    #[test]
    fn the_ground_measures_range_and_height() {
        let s = GroundCue {
            down_m: 0.0,
            sigma_m: 0.1,
        };
        let mut sight = sighting([0.0, 0.0, -6.0], [8.0, 0.0, 0.0], 1.85);
        sight.ground = Some(s);
        let f = fix(&sight, &Scalar::new(1.7, 0.12), None).unwrap();
        assert!(f.sources.ground && f.sources.box_height);
        let (h, var) = f.height_obs.unwrap();
        assert!((h - 1.85).abs() < 1e-3 && var > 0.0, "{h}");
        // The ground cue dominates the biased box: closer to the truth than
        // the box alone (1.7 / 1.85 * 8 = 7.35 m).
        assert!((f.ne[0] - 8.0).abs() < 0.3, "{:?}", f.ne);
    }

    #[test]
    fn a_cut_box_without_another_cue_ranges_on_the_prior_only() {
        let mut s = sighting([0.0, 0.0, -6.0], [8.0, 0.0, 0.0], 1.7);
        s.full_height = false;
        assert!(fix(&s, &Scalar::new(1.7, 0.12), None).is_none());
        let f = fix(&s, &Scalar::new(1.7, 0.12), Some(9.0)).unwrap();
        assert!((f.ne[0] - 9.0).abs() < 1e-6 && !f.sources.any());
    }

    #[test]
    fn a_depth_cue_converts_optical_depth_to_horizontal() {
        // A ray 30 degrees below the horizon, optical component 1 along it.
        let (s, c) = 30f32.to_radians().sin_cos();
        let cue = DepthCue::along_ray(10.0, 0.5, [c, 0.0, s]).unwrap();
        assert!((cue.horizontal_m - 10.0 * c).abs() < 1e-5);
        assert!(DepthCue::along_ray(0.0, 0.5, [1.0, 0.0, 0.0]).is_none());
        assert!(DepthCue::along_ray(5.0, 0.5, [0.0, 0.0, 1.0]).is_none());
    }
}
