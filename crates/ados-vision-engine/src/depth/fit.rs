//! Scaling a monocular depth model to metres.
//!
//! The model outputs relative inverse depth `x` with an unknown scale and
//! shift: true inverse depth is `1 / z = s * x + t`. Ranging hits projected
//! into the image give pairs `(x, 1 / z)`, and the fit is robust: `s` is the
//! median of the pairwise slopes (ratios of differences), `t` the median
//! residual intercept. A fit needs [`MIN_POINTS`] pairs, a positive scale, and
//! a median relative range error under [`MAX_MEDIAN_REL_ERROR`]. Accepted fits
//! are smoothed over time with an exponential moving average.

/// The fewest ranging pairs a fit is attempted on.
pub const MIN_POINTS: usize = 8;

/// The largest median relative range error an accepted fit may leave.
pub const MAX_MEDIAN_REL_ERROR: f32 = 0.3;

/// The smoothing weight of the newest fit.
pub const EMA_ALPHA: f32 = 0.3;

/// How long a smoothed fit stays usable without a fresh one, milliseconds.
pub const FIT_HOLD_MS: u64 = 2_000;

/// Pairs beyond this many are thinned evenly, bounding the pairwise slopes.
const MAX_PAIRS: usize = 48;

/// One fitted scale and shift: `1 / z = s * x + t`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ScaleShift {
    pub s: f32,
    pub t: f32,
}

impl ScaleShift {
    /// The optical depth for model value `x`, metres; `None` when the fit
    /// puts it at or beyond infinity.
    pub fn depth(&self, x: f32) -> Option<f32> {
        let inv = self.s * x + self.t;
        (inv > 1e-4).then(|| 1.0 / inv)
    }
}

fn median(v: &mut [f32]) -> Option<f32> {
    if v.is_empty() {
        return None;
    }
    let mid = v.len() / 2;
    let (_, m, _) = v.select_nth_unstable_by(mid, f32::total_cmp);
    Some(*m)
}

/// Fit `(x, z)` pairs: model value and measured optical depth (metres).
pub fn fit(pairs: &[(f32, f32)]) -> Option<ScaleShift> {
    let usable: Vec<(f32, f32)> = pairs
        .iter()
        .copied()
        .filter(|(x, z)| x.is_finite() && z.is_finite() && *z > 0.05)
        .map(|(x, z)| (x, 1.0 / z))
        .collect();
    if usable.len() < MIN_POINTS {
        return None;
    }
    let step = usable.len().div_ceil(MAX_PAIRS);
    let pts: Vec<(f32, f32)> = usable.into_iter().step_by(step).collect();
    let mut slopes = Vec::with_capacity(pts.len() * pts.len() / 2);
    for i in 0..pts.len() {
        for j in i + 1..pts.len() {
            let dx = pts[j].0 - pts[i].0;
            if dx.abs() > 1e-6 {
                slopes.push((pts[j].1 - pts[i].1) / dx);
            }
        }
    }
    let s = median(&mut slopes)?;
    if !(s.is_finite() && s > 0.0) {
        return None;
    }
    let mut intercepts: Vec<f32> = pts.iter().map(|(x, y)| y - s * x).collect();
    let t = median(&mut intercepts)?;
    let fitted = ScaleShift { s, t };
    let mut errors: Vec<f32> = pts
        .iter()
        .map(|(x, y)| match fitted.depth(*x) {
            Some(z) => (z * y - 1.0).abs(),
            None => f32::INFINITY,
        })
        .collect();
    let err = median(&mut errors)?;
    (err <= MAX_MEDIAN_REL_ERROR).then_some(fitted)
}

/// One camera's smoothed fit.
#[derive(Debug, Clone, Copy, Default)]
pub struct ScaleFilter {
    state: Option<(ScaleShift, u64)>,
}

impl ScaleFilter {
    /// Fold this frame's fit (if any) in at `now_ms` and return the fit to
    /// scale this frame with: the smoothed fit while one was accepted within
    /// [`FIT_HOLD_MS`], else `None` (and the filter restarts).
    pub fn update(&mut self, fresh: Option<ScaleShift>, now_ms: u64) -> Option<ScaleShift> {
        match (fresh, self.state) {
            (Some(f), Some((prev, at))) if now_ms.saturating_sub(at) <= FIT_HOLD_MS => {
                let s = prev.s + EMA_ALPHA * (f.s - prev.s);
                let t = prev.t + EMA_ALPHA * (f.t - prev.t);
                self.state = Some((ScaleShift { s, t }, now_ms));
            }
            (Some(f), _) => self.state = Some((f, now_ms)),
            (None, Some((_, at))) if now_ms.saturating_sub(at) <= FIT_HOLD_MS => {}
            (None, _) => self.state = None,
        }
        self.state.map(|(f, _)| f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pairs_for(s: f32, t: f32, n: usize) -> Vec<(f32, f32)> {
        (0..n)
            .map(|i| {
                let x = 0.1 + i as f32 * 0.05;
                (x, 1.0 / (s * x + t))
            })
            .collect()
    }

    #[test]
    fn an_exact_scale_and_shift_is_recovered() {
        let f = fit(&pairs_for(0.8, 0.05, 20)).unwrap();
        assert!((f.s - 0.8).abs() < 1e-3 && (f.t - 0.05).abs() < 1e-3);
        let z = f.depth(0.5).unwrap();
        assert!((z - 1.0 / 0.45).abs() < 1e-2);
    }

    #[test]
    fn a_fit_needs_eight_points_and_shrugs_off_a_few_outliers() {
        assert!(fit(&pairs_for(0.8, 0.05, MIN_POINTS - 1)).is_none());
        let mut p = pairs_for(0.8, 0.05, 20);
        // Three bad hits (a branch in front of the trunk the model saw).
        p[2].1 = 0.5;
        p[7].1 = 40.0;
        p[11].1 = 0.2;
        let f = fit(&p).unwrap();
        assert!((f.s - 0.8).abs() < 0.05, "{f:?}");
    }

    #[test]
    fn noise_with_no_relation_to_the_model_is_refused() {
        let p: Vec<(f32, f32)> = (0..20)
            .map(|i| (0.1 + i as f32 * 0.05, if i % 2 == 0 { 1.0 } else { 30.0 }))
            .collect();
        assert!(fit(&p).is_none());
        // An inverted relation (nearer reads farther) gives a negative scale.
        let inv: Vec<(f32, f32)> = (0..20)
            .map(|i| (0.1 + i as f32 * 0.05, 1.0 + i as f32))
            .collect();
        assert!(fit(&inv).is_none());
    }

    #[test]
    fn the_filter_smooths_holds_and_expires() {
        let mut f = ScaleFilter::default();
        let a = ScaleShift { s: 1.0, t: 0.0 };
        let b = ScaleShift { s: 2.0, t: 0.1 };
        assert_eq!(f.update(Some(a), 0), Some(a));
        let m = f.update(Some(b), 500).unwrap();
        assert!((m.s - 1.3).abs() < 1e-6 && (m.t - 0.03).abs() < 1e-6);
        // No fit this frame: the smoothed one holds for two seconds.
        assert_eq!(f.update(None, 2_400), Some(m));
        assert_eq!(f.update(None, 2_600), None);
        // After expiry the next fit starts fresh rather than averaging.
        assert_eq!(f.update(Some(b), 2_700), Some(b));
    }
}
