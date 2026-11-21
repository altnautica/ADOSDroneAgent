//! Where the subject will be: the filter's forecast at each horizon, walked
//! through a map of free space so a predicted path bends round what blocks
//! it (a person walking at a tree goes round it, along a corridor follows
//! it) instead of running through.

use ados_protocol::vision_subject::{SubjectPrediction, PREDICTION_HORIZONS_S};

use super::imm::Imm;
use super::mat::{Mat2, Mat5};
use super::scalar::Scalar;
use super::SubjectConfig;

/// What a map knows about the space around the subject.
pub trait FreeSpace {
    /// Whether a person could be at `p` (NED metres): not inside anything
    /// the map holds as occupied. Unknown space counts as free.
    fn is_free(&self, p: [f32; 3]) -> bool;
}

/// No map: everywhere is free, and predictions are the filter's own.
#[derive(Debug, Clone, Copy, Default)]
pub struct OpenSpace;

impl FreeSpace for OpenSpace {
    fn is_free(&self, _: [f32; 3]) -> bool {
        true
    }
}

/// The predicted path is walked in steps this long, seconds.
const STEP_S: f64 = 0.25;

/// The turns tried, degrees, when the next step runs into occupied space:
/// the smallest that clears is kept for the rest of the path.
const BENDS_DEG: [f64; 12] = [
    15.0, -15.0, 30.0, -30.0, 45.0, -45.0, 60.0, -60.0, 75.0, -75.0, 90.0, -90.0,
];

/// The predictions at every [`PREDICTION_HORIZONS_S`] offset.
pub(super) fn predictions(
    imm: &Imm,
    down: &Scalar,
    height_m: f64,
    bias: &Mat2,
    cfg: &SubjectConfig,
    free: &impl FreeSpace,
) -> Vec<SubjectPrediction> {
    let last = PREDICTION_HORIZONS_S[PREDICTION_HORIZONS_S.len() - 1];
    let steps = (f64::from(last) / STEP_S).round() as usize;
    // Query a person's middle, not the ground under their feet.
    let probe_down = down.value - 0.5 * height_m;
    let probe = |ne: [f64; 2]| [ne[0] as f32, ne[1] as f32, probe_down as f32];
    let start = imm.forecast_mean(0.0, cfg);
    let mut prev = [start[0], start[1]];
    let mut pos = prev;
    let mut heading = 0.0_f64;
    let mut path = Vec::with_capacity(steps + 1);
    path.push(pos);
    for k in 1..=steps {
        let m = imm.forecast_mean(k as f64 * STEP_S, cfg);
        let disp = [m[0] - prev[0], m[1] - prev[1]];
        prev = [m[0], m[1]];
        if disp[0].hypot(disp[1]) > 1e-3 {
            let turned = |a: f64| {
                let (s, c) = a.sin_cos();
                [
                    pos[0] + c * disp[0] - s * disp[1],
                    pos[1] + s * disp[0] + c * disp[1],
                ]
            };
            let ahead = turned(heading);
            if free.is_free(probe(ahead)) {
                pos = ahead;
            } else if let Some((bend, next)) = BENDS_DEG
                .iter()
                .map(|d| heading + d.to_radians())
                .map(|a| (a, turned(a)))
                .find(|(_, p)| free.is_free(probe(*p)))
            {
                heading = bend;
                pos = next;
            }
            // Nowhere to go: the subject is taken to stop where they are.
        }
        path.push(pos);
    }
    PREDICTION_HORIZONS_S
        .iter()
        .map(|&dt_s| {
            let dt = f64::from(dt_s);
            let k = ((dt / STEP_S).round() as usize).min(steps);
            let (_, p, _) = imm.forecast(dt, cfg);
            let down_var = down.var + f64::from(cfg.ground_drift_mps).powi(2) * dt;
            SubjectPrediction {
                dt_s,
                pos_ned: [path[k][0] as f32, path[k][1] as f32, down.value as f32],
                cov: cov3(&p, bias, down_var),
            }
        })
        .collect()
}

/// The 3 x 3 position covariance, row-major: the horizontal block of `p`
/// plus the range bias `bias`, and the vertical variance, uncorrelated.
pub(super) fn cov3(p: &Mat5, bias: &Mat2, down_var: f64) -> [f32; 9] {
    let h = |i: usize, j: usize| (p[i][j] + bias[i][j]) as f32;
    [
        h(0, 0),
        h(0, 1),
        0.0,
        h(1, 0),
        h(1, 1),
        0.0,
        0.0,
        0.0,
        down_var as f32,
    ]
}
