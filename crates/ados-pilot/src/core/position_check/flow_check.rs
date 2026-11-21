//! The FC's own optical flow (rate × range) against the EKF's velocity: the
//! displacement each says the vehicle made over [`WINDOW_MS`]. A frozen
//! external position pulls the EKF velocity to rest while the aircraft flies
//! on; the flow still measures the ground going by, whatever its quality.
//!
//! A dusty or dark floor makes the flow noisy: white noise of about 1 m/s
//! per axis and sample at 2 m. Summed over the window at 20 Hz that alone
//! moves the gap by about 0.4 m per axis, enough to cross a fixed
//! [`TRIP_M`] on a flight whose estimate is right. The trip and clear
//! thresholds therefore sit [`NOISE_SIGMAS`] standard deviations of that
//! summed noise above [`TRIP_M`] and [`CLEAR_M`]. The noise is measured in
//! the window itself, from the sample-to-sample differences of flow minus
//! estimate: white noise doubles their variance, while a steady or slowly
//! growing gap (the frozen estimate this check is for) adds almost nothing.
//! Clean flow keeps the bare thresholds.

use std::collections::VecDeque;

use crate::geo::{self, V2};

/// The window the flow and the estimate are compared over.
pub const WINDOW_MS: u64 = 3_000;
/// The displacement gap over the window that marks the position unverified,
/// on noise-free flow.
pub const TRIP_M: f32 = 1.5;
/// The gap under which a tripped check clears, on noise-free flow.
pub const CLEAR_M: f32 = 0.75;
/// How many standard deviations of the window's summed flow noise the trip
/// and clear thresholds sit above [`TRIP_M`] and [`CLEAR_M`]. Two keep 20 Hz
/// flow at a dusty quality from tripping on a correct estimate, and catch a
/// 1 m/s frozen-estimate gap within about 3 s.
pub const NOISE_SIGMAS: f32 = 2.0;
/// A flow gap longer than this is not bridged (the comparison restarts).
const MAX_STEP_MS: u64 = 200;
/// A flow check whose last flow is older than this no longer speaks.
const FLOW_STALE_MS: u64 = 1_000;
/// The flow-against-estimate gap a tripped check brakes on is averaged over
/// this, ms: five 10 Hz flow samples smooth a dusty floor's flow, and the
/// lag stays short of the FC's velocity response (a 1 s average let the
/// brake overshoot into a 0.3-0.9 m/s backward slide in simulation).
const BRAKE_WINDOW_MS: u64 = 500;
/// The most the flow brake asks, m/s: the frozen-pose drifts it stops run
/// to 3 m/s, and an ask held under the gap leaves the vehicle sliding at
/// the rest.
pub const FLOW_BRAKE_MAX_MPS: f32 = 3.0;

/// One flow sample in the window.
#[derive(Debug, Clone, Copy)]
struct Step {
    at_ms: u64,
    /// Flow minus estimate, north-east m/s.
    gap: V2,
    /// Time since the sample before, ms (0 for the first of a run).
    dt_ms: u64,
}

impl Step {
    fn moved(&self) -> V2 {
        geo::scale(self.gap, self.dt_ms as f32 / 1000.0)
    }
}

/// The flow-against-estimate comparison.
#[derive(Debug, Clone, Default)]
pub struct FlowCheck {
    steps: VecDeque<Step>,
    last_ms: Option<u64>,
    tripped: bool,
    flow_mps: f32,
    estimate_mps: f32,
}

impl FlowCheck {
    /// Fold one flow sample in: the ground velocity it measures and the
    /// EKF's, both north-east, m/s.
    pub fn step(&mut self, flow_ne: V2, est_ne: V2, now_ms: u64) {
        let dt_ms = match self.last_ms {
            Some(t) if now_ms.saturating_sub(t) <= MAX_STEP_MS => now_ms.saturating_sub(t),
            Some(_) => {
                self.steps.clear();
                0
            }
            None => 0,
        };
        self.last_ms = Some(now_ms);
        self.steps.push_back(Step {
            at_ms: now_ms,
            gap: geo::sub(flow_ne, est_ne),
            dt_ms,
        });
        while self
            .steps
            .front()
            .is_some_and(|s| now_ms.saturating_sub(s.at_ms) >= WINDOW_MS)
        {
            self.steps.pop_front();
        }
        self.flow_mps = geo::norm(flow_ne);
        self.estimate_mps = geo::norm(est_ne);
        let gap = self.gap_m();
        let noise = NOISE_SIGMAS * self.noise_m();
        if gap > TRIP_M + noise {
            self.tripped = true;
        } else if gap < CLEAR_M + noise {
            self.tripped = false;
        }
    }

    /// A sample with no flow measurement: the comparison restarts.
    pub fn gap(&mut self) {
        self.last_ms = None;
        self.steps.clear();
    }

    /// The displacement gap over the window, metres.
    pub fn gap_m(&self) -> f32 {
        let s = self
            .steps
            .iter()
            .fold([0.0f32; 2], |a, s| geo::add(a, s.moved()));
        geo::norm(s)
    }

    /// One standard deviation of the flow noise summed into
    /// [`FlowCheck::gap_m`], metres: the per-sample variance of flow minus
    /// estimate (both axes), from half the mean square of successive
    /// differences, times the sum of the squared sample periods.
    pub fn noise_m(&self) -> f32 {
        let n = self.steps.len();
        if n < 2 {
            return 0.0;
        }
        let (mut diff_sq, mut dt_sq) = (0.0f32, 0.0f32);
        for (a, b) in self.steps.iter().zip(self.steps.iter().skip(1)) {
            let d = geo::norm(geo::sub(b.gap, a.gap));
            diff_sq += d * d;
        }
        for s in &self.steps {
            let dt = s.dt_ms as f32 / 1000.0;
            dt_sq += dt * dt;
        }
        (diff_sq / (2.0 * (n - 1) as f32) * dt_sq).sqrt()
    }

    /// Tripped, with the speeds it last compared, while the flow is fresh.
    pub(super) fn verdict(&self, now_ms: u64) -> Option<(f32, f32)> {
        let fresh = self
            .last_ms
            .is_some_and(|t| now_ms.saturating_sub(t) <= FLOW_STALE_MS);
        (self.tripped && fresh).then_some((self.flow_mps, self.estimate_mps))
    }

    /// While tripped: the velocity, north-east m/s, to ask the flight
    /// controller for so that the ground velocity the flow measures goes to
    /// zero. The FC drives its EKF velocity to the ask, and the ground
    /// velocity is the EKF's plus the gap (flow − estimate), averaged over
    /// [`BRAKE_WINDOW_MS`]; so the ask is minus that gap, held to
    /// [`FLOW_BRAKE_MAX_MPS`]. `None` while not tripped (or the flow is
    /// stale) or with under 0.2 s of flow in the window: a quality-0 sample
    /// or a missing range restarts the comparison, and the hold then asks
    /// zero on the estimate as before.
    pub(super) fn brake_ne(&self, now_ms: u64) -> Option<V2> {
        self.verdict(now_ms)?;
        let (mut sum, mut span_ms) = ([0.0f32; 2], 0u64);
        for s in self
            .steps
            .iter()
            .filter(|s| now_ms.saturating_sub(s.at_ms) < BRAKE_WINDOW_MS)
        {
            sum = geo::add(sum, s.moved());
            span_ms += s.dt_ms;
        }
        if span_ms < 200 {
            return None;
        }
        let s = span_ms as f32 / 1000.0;
        Some(geo::clamp_len(
            [-sum[0] / s, -sum[1] / s],
            FLOW_BRAKE_MAX_MPS,
        ))
    }
}

/// The body-frame (FRD) ground velocity of a flow rate over `range_m`:
/// forward motion reads as +y flow, rightward as −x (ArduPilot's
/// OPTICAL_FLOW convention).
pub fn flow_body_velocity(comp: [f32; 2], range_m: f32) -> V2 {
    [comp[1] * range_m, -comp[0] * range_m]
}

/// Rotate a body-frame horizontal velocity by `yaw` into north-east.
pub fn body_to_ne(v: V2, yaw: f32) -> V2 {
    let (s, c) = yaw.sin_cos();
    [v[0] * c - v[1] * s, v[0] * s + v[1] * c]
}
