//! The interacting-multiple-model filter over the horizontal state: one
//! extended Kalman filter per motion model ([`super::models`]), mixed at every
//! step by the models' probabilities and a Markov switching prior, and
//! re-weighted by how well each predicted the measurement.

use super::mat::{
    add_outer, inv2, pos_block, propagate, quad2, update_position, Mat2, Mat5, Vec5, N,
};
use super::models::{constrain, jacobian, process_noise, step, Mode};
use super::SubjectConfig;

const MODES: usize = 3;

/// No model's probability falls below this, so the filter can always switch.
const MIN_PROB: f64 = 1e-3;

#[derive(Debug, Clone)]
pub(super) struct Imm {
    x: [Vec5; MODES],
    p: [Mat5; MODES],
    mu: [f64; MODES],
}

/// The probability of switching from model `i` to model `j` over `dt`, from
/// per-second switching rates: a walker turns, straightens and stops a few
/// times a minute.
fn transition(dt: f64, cfg: &SubjectConfig) -> [[f64; MODES]; MODES] {
    let rate = f64::from(cfg.switch_rate_hz);
    let leave = 1.0 - (-rate * dt).exp();
    let mut t = [[0.0; MODES]; MODES];
    for (i, row) in t.iter_mut().enumerate() {
        for (j, v) in row.iter_mut().enumerate() {
            *v = if i == j {
                1.0 - leave
            } else {
                leave / (MODES - 1) as f64
            };
        }
    }
    t
}

/// The moment-matched mean and covariance of a mixture.
fn mixture(weights: &[f64; MODES], x: &[Vec5; MODES], p: &[Mat5; MODES]) -> (Vec5, Mat5) {
    let mut mean = [0.0; N];
    for (w, xi) in weights.iter().zip(x) {
        for k in 0..N {
            mean[k] += w * xi[k];
        }
    }
    let mut cov = [[0.0; N]; N];
    for ((w, xi), pi) in weights.iter().zip(x).zip(p) {
        let mut d = [0.0; N];
        for k in 0..N {
            d[k] = xi[k] - mean[k];
        }
        for (r, row) in cov.iter_mut().enumerate() {
            for (c, v) in row.iter_mut().enumerate() {
                *v += w * pi[r][c];
            }
        }
        add_outer(&mut cov, &d, *w);
    }
    (mean, cov)
}

impl Imm {
    /// Start at a measured position with an unknown velocity.
    pub(super) fn new(pos: [f64; 2], pos_cov: &Mat2, cfg: &SubjectConfig) -> Self {
        let mut p = [[0.0; N]; N];
        for i in 0..2 {
            for j in 0..2 {
                p[i][j] = pos_cov[i][j];
            }
        }
        let v0 = f64::from(cfg.initial_speed_sigma).powi(2);
        p[2][2] = v0;
        p[3][3] = v0;
        p[4][4] = f64::from(cfg.max_turn_rate_rps / 3.0).powi(2);
        let x = [pos[0], pos[1], 0.0, 0.0, 0.0];
        let mut stop = p;
        stop[2][2] = 1e-4;
        stop[3][3] = 1e-4;
        stop[4][4] = 1e-4;
        Self {
            x: [x; MODES],
            p: [p, p, stop],
            mu: [0.5, 0.25, 0.25],
        }
    }

    pub(super) fn mode_probs(&self) -> [f64; MODES] {
        self.mu
    }

    /// The combined state and covariance.
    pub(super) fn combined(&self) -> (Vec5, Mat5) {
        mixture(&self.mu, &self.x, &self.p)
    }

    /// Mix, then move every model `dt` seconds on. The model probabilities
    /// become the predicted ones.
    pub(super) fn predict(&mut self, dt: f64, cfg: &SubjectConfig) {
        if dt <= 0.0 {
            return;
        }
        let t = transition(dt, cfg);
        let mut c = [0.0; MODES];
        for (j, cj) in c.iter_mut().enumerate() {
            *cj = (0..MODES)
                .map(|i| t[i][j] * self.mu[i])
                .sum::<f64>()
                .max(1e-12);
        }
        let mut x = self.x;
        let mut p = self.p;
        for (j, mode) in Mode::ALL.into_iter().enumerate() {
            let mut w = [0.0; MODES];
            for (i, wi) in w.iter_mut().enumerate() {
                *wi = t[i][j] * self.mu[i] / c[j];
            }
            let (x0, p0) = mixture(&w, &self.x, &self.p);
            let f = jacobian(mode, &x0, dt);
            x[j] = step(mode, &x0, dt);
            p[j] = propagate(&f, &p0, &process_noise(mode, dt, cfg));
        }
        self.x = x;
        self.p = p;
        self.mu = c;
    }

    /// The squared Mahalanobis distance of a position measurement from the
    /// combined prediction.
    pub(super) fn distance2(&self, z: [f64; 2], r: &Mat2) -> f64 {
        let (x, p) = self.combined();
        let mut s = pos_block(&p);
        for i in 0..2 {
            for j in 0..2 {
                s[i][j] += r[i][j];
            }
        }
        inv2(&s).map_or(f64::INFINITY, |(si, _)| {
            quad2(&si, [z[0] - x[0], z[1] - x[1]])
        })
    }

    /// Update every model on a position measurement and re-weigh the models
    /// by their likelihoods.
    pub(super) fn update(&mut self, z: [f64; 2], r: &Mat2, cfg: &SubjectConfig) {
        let mut log_l = [f64::NEG_INFINITY; MODES];
        for (j, mode) in Mode::ALL.into_iter().enumerate() {
            if let Some((nu, si, det)) = update_position(&mut self.x[j], &mut self.p[j], z, r) {
                log_l[j] = -0.5 * (quad2(&si, nu) + det.ln());
            }
            constrain(mode, &mut self.x[j], cfg);
        }
        let best = log_l.iter().copied().fold(f64::NEG_INFINITY, f64::max);
        if !best.is_finite() {
            return;
        }
        let mut mu = [0.0; MODES];
        for j in 0..MODES {
            mu[j] = self.mu[j] * (log_l[j] - best).exp();
        }
        self.set_probs(mu);
    }

    fn set_probs(&mut self, mu: [f64; MODES]) {
        let sum: f64 = mu.iter().sum();
        if !(sum > 0.0 && sum.is_finite()) {
            return;
        }
        let mut floored = mu.map(|m| (m / sum).max(MIN_PROB));
        let sum: f64 = floored.iter().sum();
        for m in &mut floored {
            *m /= sum;
        }
        self.mu = floored;
    }

    /// The combined state, covariance and model probabilities `dt` seconds
    /// ahead, without a measurement; the filter itself is left alone.
    pub(super) fn forecast(&self, dt: f64, cfg: &SubjectConfig) -> (Vec5, Mat5, [f64; MODES]) {
        let mut ahead = self.clone();
        ahead.predict_unmixed(dt, cfg);
        let (x, p) = ahead.combined();
        (x, p, ahead.mu)
    }

    /// The combined mean `dt` seconds ahead: each model carried on alone,
    /// weighted by the switching prior's probabilities. Cheaper than
    /// [`Self::forecast`] (no covariance).
    pub(super) fn forecast_mean(&self, dt: f64, cfg: &SubjectConfig) -> Vec5 {
        let mu = self.probs_after(dt, cfg);
        let mut mean = [0.0; N];
        for (j, mode) in Mode::ALL.into_iter().enumerate() {
            let x = step(mode, &self.x[j], dt);
            for k in 0..N {
                mean[k] += mu[j] * x[k];
            }
        }
        mean
    }

    fn probs_after(&self, dt: f64, cfg: &SubjectConfig) -> [f64; MODES] {
        let t = transition(dt, cfg);
        let mut mu = [0.0; MODES];
        for (j, m) in mu.iter_mut().enumerate() {
            *m = (0..MODES).map(|i| t[i][j] * self.mu[i]).sum();
        }
        mu
    }

    /// Carry every model on alone over a long horizon: mixing once per
    /// horizon would blend a turn into a straight line at the start.
    fn predict_unmixed(&mut self, dt: f64, cfg: &SubjectConfig) {
        let mu = self.probs_after(dt, cfg);
        for (j, mode) in Mode::ALL.into_iter().enumerate() {
            let f = jacobian(mode, &self.x[j], dt);
            self.p[j] = propagate(&f, &self.p[j], &process_noise(mode, dt, cfg));
            self.x[j] = step(mode, &self.x[j], dt);
        }
        self.mu = mu;
    }
}
