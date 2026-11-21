//! The three motion models the filter weighs, on the shared state
//! `[north, east, v_north, v_east, turn_rate]`:
//!
//! - **constant velocity**: straight walking, with white acceleration noise;
//!   its turn rate is held at zero;
//! - **coordinated turn**: walking round a bend at a nearly constant turn
//!   rate (the turn rate is part of the state and drifts with white noise);
//! - **stop**: standing still; velocity and turn rate are zero and the
//!   position only drifts.

use super::mat::{identity, Mat5, Vec5, N};
use super::SubjectConfig;

/// A motion model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    ConstantVelocity,
    CoordinatedTurn,
    Stop,
}

impl Mode {
    /// Every model, in the order mode probabilities are reported.
    pub const ALL: [Mode; 3] = [Mode::ConstantVelocity, Mode::CoordinatedTurn, Mode::Stop];
}

/// The variance a zeroed state element keeps, so covariances stay positive.
const FLOOR_VAR: f64 = 1e-4;

/// The state `dt` seconds on under `mode`.
pub(super) fn step(mode: Mode, x: &Vec5, dt: f64) -> Vec5 {
    let [n, e, vn, ve, w] = *x;
    match mode {
        Mode::ConstantVelocity => [n + vn * dt, e + ve * dt, vn, ve, 0.0],
        Mode::Stop => [n, e, 0.0, 0.0, 0.0],
        Mode::CoordinatedTurn => {
            let (s, c) = (w * dt).sin_cos();
            let (sw, cw) = turn_terms(w, dt);
            [
                n + sw * vn - cw * ve,
                e + cw * vn + sw * ve,
                c * vn - s * ve,
                s * vn + c * ve,
                w,
            ]
        }
    }
}

/// `sin(w dt) / w` and `(1 - cos(w dt)) / w`, with their limits at `w = 0`.
fn turn_terms(w: f64, dt: f64) -> (f64, f64) {
    if w.abs() < 1e-6 {
        (dt, 0.5 * w * dt * dt)
    } else {
        let (s, c) = (w * dt).sin_cos();
        (s / w, (1.0 - c) / w)
    }
}

/// The Jacobian of [`step`] at `x`.
pub(super) fn jacobian(mode: Mode, x: &Vec5, dt: f64) -> Mat5 {
    let mut f = identity();
    match mode {
        Mode::ConstantVelocity => {
            f[0][2] = dt;
            f[1][3] = dt;
            f[4][4] = 0.0;
        }
        Mode::Stop => {
            f[2][2] = 0.0;
            f[3][3] = 0.0;
            f[4][4] = 0.0;
        }
        Mode::CoordinatedTurn => {
            let w = x[4];
            let (s, c) = (w * dt).sin_cos();
            let (sw, cw) = turn_terms(w, dt);
            f[0][2] = sw;
            f[0][3] = -cw;
            f[1][2] = cw;
            f[1][3] = sw;
            f[2][2] = c;
            f[2][3] = -s;
            f[3][2] = s;
            f[3][3] = c;
            // The turn-rate column, by central difference: the closed form is
            // long and this is exact to well under the noise.
            let h = 1e-4;
            let (mut lo, mut hi) = (*x, *x);
            lo[4] -= h;
            hi[4] += h;
            let (a, b) = (step(mode, &lo, dt), step(mode, &hi, dt));
            for (i, row) in f.iter_mut().enumerate().take(4) {
                row[4] = (b[i] - a[i]) / (2.0 * h);
            }
        }
    }
    f
}

/// The process noise of `dt` seconds under `mode`.
pub(super) fn process_noise(mode: Mode, dt: f64, cfg: &SubjectConfig) -> Mat5 {
    let mut q = [[0.0; N]; N];
    let accel = |q: &mut Mat5, sigma: f32| {
        let s2 = f64::from(sigma).powi(2);
        for (p, v) in [(0, 2), (1, 3)] {
            q[p][p] = s2 * dt.powi(3) / 3.0;
            q[p][v] = s2 * dt.powi(2) / 2.0;
            q[v][p] = q[p][v];
            q[v][v] = s2 * dt;
        }
    };
    match mode {
        Mode::ConstantVelocity => {
            accel(&mut q, cfg.cv_accel_sigma);
            q[4][4] = FLOOR_VAR;
        }
        Mode::CoordinatedTurn => {
            accel(&mut q, cfg.ct_accel_sigma);
            q[4][4] = f64::from(cfg.turn_accel_sigma).powi(2) * dt;
        }
        Mode::Stop => {
            let drift = f64::from(cfg.stop_drift_sigma).powi(2) * dt;
            q[0][0] = drift;
            q[1][1] = drift;
            q[2][2] = FLOOR_VAR;
            q[3][3] = FLOOR_VAR;
            q[4][4] = FLOOR_VAR;
        }
    }
    q
}

/// Hold a model's state to what a person can do: speed at most
/// `max_speed_mps`, turn rate at most `max_turn_rate_rps`; a standing model
/// has no velocity.
pub(super) fn constrain(mode: Mode, x: &mut Vec5, cfg: &SubjectConfig) {
    match mode {
        Mode::Stop => {
            x[2] = 0.0;
            x[3] = 0.0;
            x[4] = 0.0;
        }
        Mode::ConstantVelocity | Mode::CoordinatedTurn => {
            let speed = x[2].hypot(x[3]);
            let max = f64::from(cfg.max_speed_mps);
            if speed > max {
                x[2] *= max / speed;
                x[3] *= max / speed;
            }
            let wmax = f64::from(cfg.max_turn_rate_rps);
            x[4] = if mode == Mode::ConstantVelocity {
                0.0
            } else {
                x[4].clamp(-wmax, wmax)
            };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_quarter_turn_lands_on_the_circle() {
        // 2 m/s north, turning right at 0.4 rad/s: radius 5 m.
        let x = [0.0, 0.0, 2.0, 0.0, 0.4];
        let t = std::f64::consts::FRAC_PI_2 / 0.4;
        let y = step(Mode::CoordinatedTurn, &x, t);
        assert!(
            (y[0] - 5.0).abs() < 1e-9 && (y[1] - 5.0).abs() < 1e-9,
            "{y:?}"
        );
        assert!(y[2].abs() < 1e-9 && (y[3] - 2.0).abs() < 1e-9);
        // The zero-rate limit is straight flight.
        let s = step(Mode::CoordinatedTurn, &[0.0, 0.0, 2.0, 1.0, 0.0], 1.5);
        assert!((s[0] - 3.0).abs() < 1e-9 && (s[1] - 1.5).abs() < 1e-9);
    }

    #[test]
    fn the_turn_jacobian_matches_a_numerical_one() {
        let x = [1.0, -2.0, 1.5, 0.7, 0.3];
        let f = jacobian(Mode::CoordinatedTurn, &x, 0.4);
        for j in 0..N {
            let h = 1e-6;
            let (mut lo, mut hi) = (x, x);
            lo[j] -= h;
            hi[j] += h;
            let (a, b) = (
                step(Mode::CoordinatedTurn, &lo, 0.4),
                step(Mode::CoordinatedTurn, &hi, 0.4),
            );
            for i in 0..N {
                let d = (b[i] - a[i]) / (2.0 * h);
                assert!((f[i][j] - d).abs() < 1e-5, "[{i}][{j}] {} vs {d}", f[i][j]);
            }
        }
    }
}
