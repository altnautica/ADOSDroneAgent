//! Scaling a monocular depth model to metres.
//!
//! The model outputs relative inverse depth `x` with an unknown scale and
//! shift, and that scale and shift change from frame to frame. Each frame's
//! values are first normalised by the frame's own spread ([`Normaliser`]), so
//! a fit carried from one frame to the next means the same thing in both.
//! True inverse depth is `1 / z = s * x + t` on the normalised values.
//! Ranging references projected into the image give pairs `(x, 1 / z)`, and
//! the fit is robust: `s` is the median of the pairwise slopes (ratios of
//! differences), `t` the median residual intercept. A fit needs
//! [`MIN_POINTS`] pairs, a positive scale, a median relative range error under
//! [`MAX_MEDIAN_REL_ERROR`], and references that agree with it across at least
//! [`MIN_SUPPORT`] of the frame's value spread: a fit on references that all
//! read alike (one row of thin trunks the model barely resolves) says nothing
//! about the rest of the image and is refused rather than extrapolated.
//! Accepted fits are smoothed over time with an exponential moving average.

/// The fewest ranging pairs a fit is attempted on.
pub const MIN_POINTS: usize = 8;

/// The largest median relative range error an accepted fit may leave.
pub const MAX_MEDIAN_REL_ERROR: f32 = 0.3;

/// The least share of the frame's normalised value spread the references
/// the fit agrees with must cover.
pub const MIN_SUPPORT: f32 = 0.25;

/// The smoothing weight of the newest fit.
pub const EMA_ALPHA: f32 = 0.3;

/// How long a smoothed fit stays usable without a fresh one, milliseconds.
pub const FIT_HOLD_MS: u64 = 2_000;

/// Pairs beyond this many are thinned evenly, bounding the pairwise slopes.
const MAX_PAIRS: usize = 48;

/// The quantiles of a frame's cell values that span it.
const SPAN_QUANTILES: (f32, f32) = (0.05, 0.95);

/// One frame's value normalisation: `(x - lo) / span`, the frame's 5th
/// percentile cell value going to 0 and its 95th to 1.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Normaliser {
    lo: f32,
    span: f32,
}

impl Normaliser {
    /// The identity (values already normalised).
    pub const IDENTITY: Self = Self { lo: 0.0, span: 1.0 };

    /// The normalisation of a frame whose cells hold `values`, using
    /// `scratch` for the sort; `None` for a frame with no spread.
    pub fn of(values: &[f32], scratch: &mut Vec<f32>) -> Option<Self> {
        scratch.clear();
        scratch.extend(values.iter().copied().filter(|v| v.is_finite()));
        if scratch.len() < 2 {
            return None;
        }
        scratch.sort_unstable_by(f32::total_cmp);
        let at = |q: f32| scratch[((scratch.len() - 1) as f32 * q).round() as usize];
        let (lo, hi) = (at(SPAN_QUANTILES.0), at(SPAN_QUANTILES.1));
        let span = hi - lo;
        (span.is_finite() && span > 1e-6).then_some(Self { lo, span })
    }

    /// `x` normalised.
    pub fn apply(&self, x: f32) -> f32 {
        (x - self.lo) / self.span
    }

    /// A fit on normalised values as one on this frame's raw values.
    pub fn to_raw(&self, f: ScaleShift) -> ScaleShift {
        ScaleShift {
            s: f.s / self.span,
            t: f.t - f.s * self.lo / self.span,
        }
    }
}

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

/// Fit `(x, z)` pairs: raw model value and measured optical depth (metres),
/// the values normalised by `norm`. The fit is on normalised values.
pub fn fit(pairs: &[(f32, f32)], norm: &Normaliser) -> Option<ScaleShift> {
    let usable: Vec<(f32, f32)> = pairs
        .iter()
        .copied()
        .filter(|(x, z)| x.is_finite() && z.is_finite() && *z > 0.05)
        .map(|(x, z)| (norm.apply(x), 1.0 / z))
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
    // The references the fit agrees with must span the frame, or the fit is
    // an extrapolation from a sliver of it.
    let (lo, hi) = pts
        .iter()
        .zip(&errors)
        .filter(|(_, e)| **e <= MAX_MEDIAN_REL_ERROR)
        .fold((f32::INFINITY, f32::NEG_INFINITY), |(lo, hi), ((x, _), _)| {
            (lo.min(*x), hi.max(*x))
        });
    let err = median(&mut errors)?;
    (err <= MAX_MEDIAN_REL_ERROR && hi - lo >= MIN_SUPPORT).then_some(fitted)
}

/// One camera's smoothed fit, on normalised values.
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

    const ID: Normaliser = Normaliser::IDENTITY;

    #[test]
    fn an_exact_scale_and_shift_is_recovered() {
        let f = fit(&pairs_for(0.8, 0.05, 20), &ID).unwrap();
        assert!((f.s - 0.8).abs() < 1e-3 && (f.t - 0.05).abs() < 1e-3);
        let z = f.depth(0.5).unwrap();
        assert!((z - 1.0 / 0.45).abs() < 1e-2);
    }

    #[test]
    fn a_fit_needs_eight_points_and_shrugs_off_a_few_outliers() {
        assert!(fit(&pairs_for(0.8, 0.05, MIN_POINTS - 1), &ID).is_none());
        let mut p = pairs_for(0.8, 0.05, 20);
        // Three bad hits (a branch in front of the trunk the model saw).
        p[2].1 = 0.5;
        p[7].1 = 40.0;
        p[11].1 = 0.2;
        let f = fit(&p, &ID).unwrap();
        assert!((f.s - 0.8).abs() < 0.05, "{f:?}");
    }

    #[test]
    fn noise_with_no_relation_to_the_model_is_refused() {
        let p: Vec<(f32, f32)> = (0..20)
            .map(|i| (0.1 + i as f32 * 0.05, if i % 2 == 0 { 1.0 } else { 30.0 }))
            .collect();
        assert!(fit(&p, &ID).is_none());
        // An inverted relation (nearer reads farther) gives a negative scale.
        let inv: Vec<(f32, f32)> = (0..20)
            .map(|i| (0.1 + i as f32 * 0.05, 1.0 + i as f32))
            .collect();
        assert!(fit(&inv, &ID).is_none());
    }

    #[test]
    fn references_that_all_read_alike_are_not_extrapolated_to_the_frame() {
        // Trunks 6.6-10.3 m away on one row, where the model reads nearly
        // the same value: the residuals are small, but the fit is a guess
        // for every other part of the frame.
        let sliver: Vec<(f32, f32)> = (0..20)
            .map(|i| (0.40 + 0.005 * (i % 4) as f32, 6.6 + 0.2 * i as f32))
            .collect();
        assert!(fit(&sliver, &ID).is_none());
    }

    #[test]
    fn the_normaliser_spans_the_frame_and_maps_a_fit_back_to_raw_values() {
        let cells: Vec<f32> = (0..=100).map(|i| 200.0 + 6.0 * i as f32).collect();
        let mut scratch = Vec::new();
        let n = Normaliser::of(&cells, &mut scratch).unwrap();
        assert!((n.apply(230.0)).abs() < 1e-5 && (n.apply(770.0) - 1.0).abs() < 1e-5);
        let f = ScaleShift { s: 0.2, t: 0.05 };
        let raw = n.to_raw(f);
        for x in [230.0f32, 500.0, 770.0] {
            let (a, b) = (f.depth(n.apply(x)).unwrap(), raw.depth(x).unwrap());
            assert!((a - b).abs() < 1e-3 * a, "{a} {b}");
        }
        assert!(Normaliser::of(&[3.0; 10], &mut scratch).is_none());
        assert!(Normaliser::of(&[f32::NAN; 10], &mut scratch).is_none());
    }

    #[test]
    fn a_held_fit_follows_the_model_rescaling_its_output_between_frames() {
        // The same scene twice; the model's raw output is three times larger
        // and shifted the second time. Carried on normalised values, the fit
        // from the first frame gives the second the same depths.
        let a: Vec<f32> = (0..64).map(|i| 1.0 + i as f32).collect();
        let b: Vec<f32> = a.iter().map(|x| 3.0 * x + 40.0).collect();
        let mut scratch = Vec::new();
        let (na, nb) = (
            Normaliser::of(&a, &mut scratch).unwrap(),
            Normaliser::of(&b, &mut scratch).unwrap(),
        );
        let pairs: Vec<(f32, f32)> = a.iter().map(|x| (*x, 50.0 / x)).collect();
        let mut filter = ScaleFilter::default();
        let fa = filter.update(fit(&pairs, &na), 0).unwrap();
        let held = filter.update(None, 500).unwrap();
        for (xa, xb) in a.iter().zip(&b).step_by(9) {
            let za = na.to_raw(fa).depth(*xa).unwrap();
            let zb = nb.to_raw(held).depth(*xb).unwrap();
            assert!((za - zb).abs() < 1e-2 * za, "{za} {zb}");
            assert!((za - 50.0 / xa).abs() < 0.05 * za);
        }
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
