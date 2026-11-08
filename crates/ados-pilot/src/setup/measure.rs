//! In-flight measurements: vibration, attitude tracking error, rate
//! oscillation, step-response overshoot.

use std::f32::consts::PI;

use super::fcstate::{FcState, RateSample};

/// Vibration verdict thresholds (m/s²), contract C9.
pub const VIBE_PASS: f32 = 30.0;
pub const VIBE_WARN: f32 = 60.0;
/// Attitude error RMS pass threshold (deg).
pub const ATT_ERR_PASS_DEG: f32 = 3.0;
/// Band (3–15 Hz) RMS of a body rate above which the vehicle oscillates (rad/s).
pub const OSC_RMS_RAD_S: f32 = 0.30;
/// Window for the oscillation check (ms).
pub const OSC_WINDOW_MS: u64 = 2000;

pub fn vibe_verdict(max: f32) -> &'static str {
    if max < VIBE_PASS {
        "pass"
    } else if max < VIBE_WARN {
        "warn"
    } else {
        "fail"
    }
}

/// Running statistics a hover/flight phase accumulates.
#[derive(Debug, Clone, Default)]
pub struct Acc {
    pub vibe_max: [f32; 3],
    pub vibe_sum: [f32; 3],
    pub vibe_n: u32,
    pub err_sq: [f32; 2],
    pub err_n: u32,
    pub last_att_ms: u64,
    pub throttle_sum: f32,
    pub throttle_n: u32,
    pub climb_sum: f32,
    pub climb_n: u32,
    pub airspeed_err_sum: f32,
    pub airspeed_err_n: u32,
    pub last_vibe_ms: u64,
    pub last_vfr_ms: u64,
}

impl Acc {
    /// Folds in the samples that are new since the last call.
    pub fn sample(&mut self, fc: &FcState, now_ms: u64) {
        if let Some((v, t)) = fc.vibration {
            if t > self.last_vibe_ms {
                self.last_vibe_ms = t;
                for ((max, sum), x) in self
                    .vibe_max
                    .iter_mut()
                    .zip(self.vibe_sum.iter_mut())
                    .zip(v)
                {
                    *max = max.max(x);
                    *sum += x;
                }
                self.vibe_n += 1;
            }
        }
        if let (Some((a, ta)), Some((tg, tt))) = (fc.attitude, fc.att_target) {
            if ta > self.last_att_ms && now_ms.saturating_sub(tt) < 500 {
                self.last_att_ms = ta;
                for i in 0..2 {
                    let e = (a[i] - tg[i]).to_degrees();
                    self.err_sq[i] += e * e;
                }
                self.err_n += 1;
            }
        }
        if now_ms > self.last_vfr_ms + 200 {
            self.last_vfr_ms = now_ms;
            if let Some(th) = fc.throttle_pct {
                self.throttle_sum += th as f32;
                self.throttle_n += 1;
            }
            if let Some(c) = fc.climb {
                self.climb_sum += c;
                self.climb_n += 1;
            }
            if let (Some(asp), Some(gs)) = (fc.airspeed, fc.groundspeed) {
                // Without a wind estimate the comparison would include the wind.
                if let Some((dir, spd, _)) = fc.wind {
                    let _ = dir;
                    // Bounded comparison: |airspeed − groundspeed| ≤ wind speed
                    // when the estimates agree.
                    let excess = ((asp - gs).abs() - spd).max(0.0);
                    self.airspeed_err_sum += excess;
                    self.airspeed_err_n += 1;
                }
            }
        }
    }

    pub fn att_err_rms(&self) -> Option<[f32; 2]> {
        (self.err_n > 0).then(|| {
            [
                (self.err_sq[0] / self.err_n as f32).sqrt(),
                (self.err_sq[1] / self.err_n as f32).sqrt(),
            ]
        })
    }

    pub fn vibe_peak(&self) -> f32 {
        self.vibe_max.iter().copied().fold(0.0, f32::max)
    }

    pub fn throttle_mean(&self) -> Option<f32> {
        (self.throttle_n > 0).then(|| self.throttle_sum / self.throttle_n as f32)
    }

    pub fn climb_mean(&self) -> Option<f32> {
        (self.climb_n > 0).then(|| self.climb_sum / self.climb_n as f32)
    }

    pub fn airspeed_excess_mean(&self) -> Option<f32> {
        (self.airspeed_err_n > 0).then(|| self.airspeed_err_sum / self.airspeed_err_n as f32)
    }
}

/// RMS of `rates` in the 3–15 Hz band, per axis `[roll, pitch]` (rad/s),
/// from a DFT over the samples in the last [`OSC_WINDOW_MS`]. `None` when
/// the stream is too slow to see the band (< 32 Hz).
pub fn band_rms(rates: &std::collections::VecDeque<RateSample>, now_ms: u64) -> Option<[f32; 2]> {
    let win: Vec<&RateSample> = rates
        .iter()
        .filter(|s| now_ms.saturating_sub(s.t_ms) <= OSC_WINDOW_MS)
        .collect();
    if win.len() < 16 {
        return None;
    }
    let span_s = (win.last()?.t_ms - win.first()?.t_ms) as f32 / 1000.0;
    if span_s <= 0.5 {
        return None;
    }
    let fs = (win.len() - 1) as f32 / span_s;
    if fs < 32.0 {
        return None;
    }
    let n = win.len();
    let mut out = [0.0f32; 2];
    for (axis, slot) in out.iter_mut().enumerate() {
        let x: Vec<f32> = win
            .iter()
            .map(|s| if axis == 0 { s.roll } else { s.pitch })
            .collect();
        let mean = x.iter().sum::<f32>() / n as f32;
        let mut power = 0.0f32;
        let k_lo = (3.0 * n as f32 / fs).ceil() as usize;
        let k_hi = ((15.0 * n as f32 / fs).floor() as usize).min(n / 2);
        for k in k_lo.max(1)..=k_hi {
            let (mut re, mut im) = (0.0f32, 0.0f32);
            for (i, v) in x.iter().enumerate() {
                let ph = 2.0 * PI * k as f32 * i as f32 / n as f32;
                re += (v - mean) * ph.cos();
                im -= (v - mean) * ph.sin();
            }
            // One-sided spectrum: 2·|X_k|²/N² is the mean-square of that bin.
            power += 2.0 * (re * re + im * im) / (n as f32 * n as f32);
        }
        *slot = power.sqrt();
    }
    Some(out)
}

/// Overshoot of a velocity step: peak along the step direction over the
/// commanded speed, minus one (0 = none).
pub fn overshoot(peak: f32, commanded: f32) -> f32 {
    if commanded <= 0.0 {
        0.0
    } else {
        (peak / commanded - 1.0).max(0.0)
    }
}
