//! The metric fit behind the `vision.depth.v2` frames: one scale and shift
//! taking the depth model's normalised relative inverse depth `x` to true
//! inverse depth, `1 / Z = s * x + t`, from the ranging hits in the frame.
//!
//! **Fit.** RANSAC on two-point hypotheses (every pair when there are few
//! hits, a fixed pseudo-random draw of [`RANSAC_DRAWS`] pairs otherwise, so a
//! frame always fits the same way), scoring the hits whose relative range
//! error `|(s * x + t) * Z - 1|` is within [`INLIER_REL`]. The best consensus
//! is refined by iteratively reweighted least squares with a Huber weight
//! ([`HUBER_REL`]) on that relative error, re-selecting the consensus after
//! each round. A fit needs [`MIN_INLIERS`] inliers, a positive scale, and
//! inliers spread over at least [`MIN_SUPPORT`] of the frame's normalised
//! value span (references that all read alike, one row of thin trunks, say
//! nothing about the rest of the frame).
//!
//! **Uncertainty.** `sigma_inv` is the inliers' root-mean-square residual in
//! inverse depth (1/m), floored at [`SIGMA_INV_FLOOR`]. In inverse depth the
//! error is close to constant over the frame, so the depth error grows with
//! the square of the depth: `sigma_Z ≈ Z² * sigma_inv` (the per-pixel model
//! is in [`super::frame`]).
//!
//! **Hold.** A frame with too few hits for a fit reuses the last good one for
//! up to [`HOLD_MAX_MS`], its `sigma_inv` multiplied by
//! `1 + held_ms / HOLD_DOUBLING_MS` (doubling over each second held). Past
//! that the camera has no fit and publishes no frames.

pub use crate::depth::fit::MIN_SUPPORT;

/// The fewest inliers a fit is accepted on.
pub const MIN_INLIERS: usize = 6;

/// The largest relative range error an inlier may have.
pub const INLIER_REL: f32 = 0.15;

/// The relative error beyond which the refinement's Huber weight falls off.
pub const HUBER_REL: f32 = 0.05;

/// Two-point hypotheses drawn when there are too many hits to try every pair.
pub const RANSAC_DRAWS: usize = 96;

/// The least inverse-depth residual a fit is credited with, 1/m.
pub const SIGMA_INV_FLOOR: f32 = 0.004;

/// The longest a fit is held for frames without one, milliseconds.
pub const HOLD_MAX_MS: u64 = 5_000;

/// The hold time over which a held fit's uncertainty doubles, milliseconds.
pub const HOLD_DOUBLING_MS: f32 = 1_000.0;

/// Refinement rounds.
const IRLS_ROUNDS: usize = 4;

/// The least normalised-value gap between a hypothesis' two points.
const MIN_PAIR_DX: f32 = 0.02;

/// Hits beyond this many are thinned evenly.
const MAX_HITS: usize = 256;

/// One accepted fit.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MetricFit {
    pub s: f32,
    pub t: f32,
    pub inliers: u32,
    /// Median `|Z_fit - Z|` over the inliers, metres.
    pub residual_m: f32,
    /// Inverse-depth residual, 1/m, at least [`SIGMA_INV_FLOOR`].
    pub sigma_inv: f32,
}

impl MetricFit {
    /// The optical depth for normalised value `x`, `None` at or beyond
    /// infinity.
    pub fn depth(&self, x: f32) -> Option<f32> {
        let inv = self.s * x + self.t;
        (inv > 1e-4).then(|| 1.0 / inv)
    }
}

/// `(normalised x, inverse depth)` of the usable hits.
fn usable(pairs: &[(f32, f32)]) -> Vec<(f32, f32)> {
    let all: Vec<(f32, f32)> = pairs
        .iter()
        .copied()
        .filter(|(x, z)| x.is_finite() && z.is_finite() && *z > 0.05)
        .map(|(x, z)| (x, 1.0 / z))
        .collect();
    let step = all.len().div_ceil(MAX_HITS).max(1);
    all.into_iter().step_by(step).collect()
}

/// The relative range error of hit `(x, y)` under `(s, t)`.
fn rel(s: f32, t: f32, (x, y): (f32, f32)) -> f32 {
    ((s * x + t) / y - 1.0).abs()
}

/// Inlier count and summed error of `(s, t)`.
fn score(s: f32, t: f32, pts: &[(f32, f32)]) -> (usize, f32) {
    pts.iter().fold((0, 0.0), |(n, sum), p| {
        let e = rel(s, t, *p);
        if e <= INLIER_REL {
            (n + 1, sum + e)
        } else {
            (n, sum)
        }
    })
}

/// The pairs of hit indices hypotheses are drawn from.
fn draws(n: usize) -> Vec<(usize, usize)> {
    if n * (n - 1) / 2 <= RANSAC_DRAWS {
        return (0..n)
            .flat_map(|i| (i + 1..n).map(move |j| (i, j)))
            .collect();
    }
    // A fixed xorshift stream: the same frame always fits the same way.
    let mut state: u32 = 0x9E37_79B9;
    let mut next = || {
        state ^= state << 13;
        state ^= state >> 17;
        state ^= state << 5;
        state as usize % n
    };
    (0..RANSAC_DRAWS)
        .map(|_| (next(), next()))
        .filter(|(i, j)| i != j)
        .collect()
}

/// Weighted least squares of `y = s * x + t`, each hit weighted by its Huber
/// weight over `y²` (so the residual minimised is the relative one).
fn refine(s: f32, t: f32, pts: &[(f32, f32)]) -> Option<(f32, f32)> {
    let (mut sw, mut sx, mut sy, mut sxx, mut sxy) = (0.0f64, 0.0, 0.0, 0.0, 0.0);
    for p in pts {
        let e = rel(s, t, *p);
        if e > INLIER_REL {
            continue;
        }
        let huber = if e <= HUBER_REL { 1.0 } else { HUBER_REL / e };
        let w = f64::from(huber) / f64::from(p.1 * p.1);
        let (x, y) = (f64::from(p.0), f64::from(p.1));
        sw += w;
        sx += w * x;
        sy += w * y;
        sxx += w * x * x;
        sxy += w * x * y;
    }
    let det = sw * sxx - sx * sx;
    if det.abs() < 1e-12 {
        return None;
    }
    let s = (sw * sxy - sx * sy) / det;
    let t = (sy - s * sx) / sw;
    Some((s as f32, t as f32))
}

/// Fit `(normalised x, optical depth Z)` hits.
pub fn fit(pairs: &[(f32, f32)]) -> Option<MetricFit> {
    let pts = usable(pairs);
    if pts.len() < MIN_INLIERS {
        return None;
    }
    let mut best: Option<(f32, f32, usize, f32)> = None;
    for (i, j) in draws(pts.len()) {
        let ((xi, yi), (xj, yj)) = (pts[i], pts[j]);
        let dx = xj - xi;
        if dx.abs() < MIN_PAIR_DX {
            continue;
        }
        let s = (yj - yi) / dx;
        if !(s.is_finite() && s > 0.0) {
            continue;
        }
        let t = yi - s * xi;
        let (n, err) = score(s, t, &pts);
        if best.is_none_or(|(_, _, bn, be)| n > bn || (n == bn && err < be)) {
            best = Some((s, t, n, err));
        }
    }
    let (mut s, mut t, _, _) = best?;
    for _ in 0..IRLS_ROUNDS {
        (s, t) = refine(s, t, &pts)?;
    }
    if !(s.is_finite() && t.is_finite() && s > 0.0) {
        return None;
    }
    let inliers: Vec<(f32, f32)> = pts
        .iter()
        .copied()
        .filter(|p| rel(s, t, *p) <= INLIER_REL)
        .collect();
    if inliers.len() < MIN_INLIERS {
        return None;
    }
    let (lo, hi) = inliers
        .iter()
        .fold((f32::INFINITY, f32::NEG_INFINITY), |(lo, hi), (x, _)| {
            (lo.min(*x), hi.max(*x))
        });
    if hi - lo < MIN_SUPPORT {
        return None;
    }
    let ss: f32 = inliers.iter().map(|(x, y)| (s * x + t - y).powi(2)).sum();
    let dof = (inliers.len() - 2) as f32;
    let mut range_err: Vec<f32> = inliers
        .iter()
        .map(|(x, y)| (1.0 / (s * x + t) - 1.0 / y).abs())
        .collect();
    let mid = range_err.len() / 2;
    let (_, median, _) = range_err.select_nth_unstable_by(mid, f32::total_cmp);
    Some(MetricFit {
        s,
        t,
        inliers: inliers.len() as u32,
        residual_m: *median,
        sigma_inv: (ss / dof).sqrt().max(SIGMA_INV_FLOOR),
    })
}

/// One camera's last good fit, for frames without one.
#[derive(Debug, Clone, Copy, Default)]
pub struct FitHold {
    last: Option<(MetricFit, u64)>,
}

impl FitHold {
    /// Take this frame's fit (if any) at `now_ms` and return the fit to scale
    /// it with, its `sigma_inv` inflated by the hold, and how long it was
    /// held (`0` for a fresh fit). `None` once the last good fit is older
    /// than [`HOLD_MAX_MS`].
    pub fn update(&mut self, fresh: Option<MetricFit>, now_ms: u64) -> Option<(MetricFit, u64)> {
        if let Some(f) = fresh {
            self.last = Some((f, now_ms));
            return Some((f, 0));
        }
        let (f, at) = self.last?;
        let held = now_ms.saturating_sub(at);
        if held > HOLD_MAX_MS {
            self.last = None;
            return None;
        }
        let grown = 1.0 + held as f32 / HOLD_DOUBLING_MS;
        Some((
            MetricFit {
                sigma_inv: f.sigma_inv * grown,
                ..f
            },
            held,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `n` hits on `1 / Z = s * x + t` over `x` in `0.05..0.95`.
    fn hits(s: f32, t: f32, n: usize) -> Vec<(f32, f32)> {
        (0..n)
            .map(|i| {
                let x = 0.05 + 0.9 * i as f32 / (n - 1) as f32;
                (x, 1.0 / (s * x + t))
            })
            .collect()
    }

    #[test]
    fn a_known_scale_and_shift_is_recovered_through_thirty_percent_outliers() {
        let (s, t) = (0.42, 0.035);
        let mut p = hits(s, t, 40);
        // 12 of 40 hits wrong: branches in front of what the model saw,
        // returns through gaps, a rangefinder on a bush.
        for (k, i) in (0..40).step_by(3).take(12).enumerate() {
            let z = p[i].1;
            p[i].1 = if k % 2 == 0 { z * 0.4 } else { z * 2.5 + 3.0 };
        }
        // Mild noise on the rest.
        for (i, hit) in p.iter_mut().enumerate() {
            hit.1 *= 1.0 + 0.01 * ((i * 7 % 5) as f32 - 2.0) / 2.0;
        }
        let f = fit(&p).expect("a fit");
        assert!((f.s - s).abs() / s < 0.03, "{f:?}");
        assert!((f.t - t).abs() < 0.01, "{f:?}");
        assert_eq!(f.inliers, 28, "{f:?}");
        assert!(f.residual_m < 0.2, "{f:?}");
    }

    #[test]
    fn six_inliers_fit_and_five_do_not() {
        assert!(fit(&hits(0.3, 0.05, MIN_INLIERS - 1)).is_none());
        let f = fit(&hits(0.3, 0.05, MIN_INLIERS)).unwrap();
        assert_eq!(f.inliers as usize, MIN_INLIERS);
        assert_eq!(f.sigma_inv, SIGMA_INV_FLOOR);
        assert!(f.residual_m < 1e-3);
    }

    #[test]
    fn hits_that_all_read_alike_or_disagree_are_refused() {
        let sliver: Vec<(f32, f32)> = (0..20)
            .map(|i| (0.40 + 0.005 * (i % 4) as f32, 6.6 + 0.2 * i as f32))
            .collect();
        assert!(fit(&sliver).is_none());
        let noise: Vec<(f32, f32)> = (0..20)
            .map(|i| (0.05 * i as f32, if i % 2 == 0 { 1.0 } else { 30.0 }))
            .collect();
        assert!(fit(&noise).is_none());
        // Nearer reads farther: a negative scale.
        let inverted: Vec<(f32, f32)> =
            (0..20).map(|i| (0.05 * i as f32, 1.0 + i as f32)).collect();
        assert!(fit(&inverted).is_none());
    }

    #[test]
    fn a_held_fit_grows_its_uncertainty_and_expires() {
        let f = fit(&hits(0.3, 0.05, 12)).unwrap();
        let mut hold = FitHold::default();
        assert!(hold.update(None, 0).is_none());
        assert_eq!(hold.update(Some(f), 1_000), Some((f, 0)));
        let (h1, held1) = hold.update(None, 1_500).unwrap();
        let (h2, held2) = hold.update(None, 3_000).unwrap();
        assert_eq!((held1, held2), (500, 2_000));
        assert_eq!((h1.s, h1.t), (f.s, f.t));
        assert!((h1.sigma_inv / f.sigma_inv - 1.5).abs() < 1e-5);
        assert!((h2.sigma_inv / f.sigma_inv - 3.0).abs() < 1e-5);
        assert!(hold.update(None, 1_000 + HOLD_MAX_MS + 1).is_none());
        // Expired for good: no resurrection later.
        assert!(hold.update(None, 1_000 + HOLD_MAX_MS + 2).is_none());
    }
}
