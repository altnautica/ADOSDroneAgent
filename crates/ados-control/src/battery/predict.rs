//! Time-to-reserve projection.
//!
//! The average percent-drop rate over a sliding window, projected forward to
//! the reserve percentage. Deliberately simple: it is good to within about ten
//! percent over the linear part of a LiPo discharge curve, and it never claims
//! a projection it lacks the data for.

use std::collections::VecDeque;

use serde::Serialize;

/// A drop rate above this (percent per second) is reported as `high`.
const HIGH_DROP_PCT_PER_S: f64 = 0.5;

/// The fields of one sample the projection reads.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Point {
    pub t_ms: i64,
    pub remaining_pct: Option<i64>,
    pub current_a: Option<f64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PredictionState {
    /// Not discharging, or not enough data to say.
    Idle,
    Normal,
    /// Discharging faster than [`HIGH_DROP_PCT_PER_S`].
    High,
    /// Already at or below the reserve.
    Past,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Prediction {
    pub state: PredictionState,
    /// Seconds until the reserve, `null` when idle.
    pub eta_s: Option<i64>,
    pub drop_pct_per_s: Option<f64>,
    /// Mean absolute current over the window.
    pub mean_current_a: Option<f64>,
}

impl Prediction {
    /// No projection: fewer than two readings, or a window under a second.
    pub const NONE: Self = Self {
        state: PredictionState::Idle,
        eta_s: None,
        drop_pct_per_s: None,
        mean_current_a: None,
    };
}

impl Default for Prediction {
    fn default() -> Self {
        Self::NONE
    }
}

/// Project the time to `reserve_percent` from the readings within `window_s`
/// of the newest one that reports a remaining percentage. Needs at least two
/// such readings spanning at least one second.
pub fn predict(points: &VecDeque<Point>, window_s: u32, reserve_percent: u32) -> Prediction {
    let Some((newest, newest_pct)) = points
        .iter()
        .rev()
        .find_map(|p| p.remaining_pct.map(|pct| (p, pct as f64)))
    else {
        return Prediction::NONE;
    };
    let window_start = newest.t_ms - i64::from(window_s) * 1000;
    let mut first: Option<(i64, f64)> = None;
    let mut count = 0usize;
    let mut current_sum = 0.0;
    let mut current_n = 0usize;
    for p in points
        .iter()
        .filter(|p| p.t_ms >= window_start && p.t_ms <= newest.t_ms)
    {
        if let Some(pct) = p.remaining_pct {
            first.get_or_insert((p.t_ms, pct as f64));
            count += 1;
        }
        if let Some(a) = p.current_a {
            current_sum += a.abs();
            current_n += 1;
        }
    }
    let Some((first_t, first_pct)) = first else {
        return Prediction::NONE;
    };
    let elapsed_s = (newest.t_ms - first_t) as f64 / 1000.0;
    if count < 2 || elapsed_s < 1.0 {
        return Prediction::NONE;
    }

    let mean_current_a = (current_n > 0).then(|| current_sum / current_n as f64);
    let drop = (first_pct - newest_pct) / elapsed_s;
    let prediction = |state, eta_s| Prediction {
        state,
        eta_s,
        drop_pct_per_s: Some(drop),
        mean_current_a,
    };
    if drop <= 0.0 {
        return prediction(PredictionState::Idle, None);
    }
    let to_reserve = newest_pct - f64::from(reserve_percent);
    if to_reserve <= 0.0 {
        return prediction(PredictionState::Past, Some(0));
    }
    let eta_s = (to_reserve / drop).round() as i64;
    let state = if drop > HIGH_DROP_PCT_PER_S {
        PredictionState::High
    } else {
        PredictionState::Normal
    };
    prediction(state, Some(eta_s))
}

#[cfg(test)]
mod tests {
    use super::*;

    const WINDOW_S: u32 = 30;
    const RESERVE: u32 = 25;

    fn at(t_ms: i64, pct: i64, current_a: f64) -> Point {
        Point {
            t_ms,
            remaining_pct: Some(pct),
            current_a: Some(current_a),
        }
    }

    fn run(points: Vec<Point>) -> Prediction {
        predict(&points.into(), WINDOW_S, RESERVE)
    }

    #[test]
    fn no_projection_without_two_readings() {
        assert_eq!(run(vec![]), Prediction::NONE);
        assert_eq!(run(vec![at(0, 90, -10.0)]), Prediction::NONE);
    }

    #[test]
    fn no_projection_on_a_sub_second_window() {
        assert_eq!(
            run(vec![at(0, 90, -10.0), at(500, 89, -10.0)]),
            Prediction::NONE
        );
    }

    #[test]
    fn a_flat_percentage_is_idle() {
        let p = run(vec![at(0, 90, 0.0), at(2000, 90, 0.0), at(4000, 90, 0.0)]);
        assert_eq!(p.state, PredictionState::Idle);
        assert_eq!(p.eta_s, None);
    }

    #[test]
    fn at_or_below_the_reserve_is_past_with_zero_eta() {
        let p = run(vec![at(0, 30, -10.0), at(5000, 24, -20.0)]);
        assert_eq!(p.state, PredictionState::Past);
        assert_eq!(p.eta_s, Some(0));
        let at_reserve = run(vec![at(0, 30, -10.0), at(5000, 25, -20.0)]);
        assert_eq!(at_reserve.state, PredictionState::Past);
    }

    #[test]
    fn eta_is_the_linear_projection_to_the_reserve() {
        // 90 -> 80 over 10 s is 1 %/s; 55 points above the reserve is 55 s.
        let p = run((0..=5).map(|i| at(i * 2000, 90 - 2 * i, -10.0)).collect());
        assert_eq!(p.eta_s, Some(55));
        assert_eq!(p.state, PredictionState::High);
        assert_eq!(p.drop_pct_per_s, Some(1.0));
        assert_eq!(p.mean_current_a, Some(10.0));
    }

    #[test]
    fn half_a_percent_per_second_is_still_normal() {
        // Exactly 0.5 %/s: the high band is strictly above it.
        let p = run(vec![at(0, 80, -5.0), at(10_000, 75, -5.0)]);
        assert_eq!(p.state, PredictionState::Normal);
        assert_eq!(p.eta_s, Some(100));
    }

    #[test]
    fn readings_older_than_the_window_do_not_skew_the_rate() {
        // 1 %/s for 30 s, then 0.1 %/s for 30 s (one integer step every 10 s).
        // The 30 s window sees only the slow tail, so the projection is near
        // (67 - 25) / 0.1 = 420 s.
        let mut points: Vec<Point> = (0..=30).map(|s| at(s * 1000, 100 - s, -10.0)).collect();
        points.extend((1..=3).map(|step| at(30_000 + step * 10_000, 70 - step, -10.0)));
        let eta = run(points).eta_s.unwrap();
        assert!((380..460).contains(&eta), "eta {eta}");
    }

    #[test]
    fn readings_without_a_percentage_are_skipped() {
        let mut points = vec![at(0, 90, -10.0), at(10_000, 80, -10.0)];
        points.push(Point {
            t_ms: 11_000,
            remaining_pct: None,
            current_a: Some(-30.0),
        });
        let p = run(points);
        assert_eq!(p.drop_pct_per_s, Some(1.0));
        assert_eq!(p.eta_s, Some(55));
    }
}
