//! The per-source fit, chosen by how the source's pairs are stamped, and the
//! verdict on a pair against its prediction.

use ados_sensorbus::fit::ClockFit;
use ados_sensorbus::SyncSource;

use crate::clock::{ENVELOPE_LATE_MAX_NS, OUTLIER_FLOOR_NS, OUTLIER_SIGMAS, RESET_DISAGREE_NS};
use crate::envelope::{EnvelopeFit, ENVELOPE_LOCK_SPAN_NS};

/// How a pair relates to its source's prediction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Miss {
    /// Feed it to the fit.
    Fine,
    /// Drop it.
    Outlier,
    /// Far enough off that a run of these means the source clock restarted.
    Large,
}

#[derive(Debug, Clone)]
pub enum Fit {
    /// Pairs with symmetric error: two-way TIMESYNC midpoints, kernel PPS
    /// stamps, the hub's already-enveloped pairs. Least squares.
    Linear(ClockFit),
    /// Pairs stamped on arrival (the simulator SEI): a one-sided, jittery
    /// latency. Lower envelope.
    Envelope(EnvelopeFit),
}

impl Fit {
    pub fn for_source(source: SyncSource) -> Self {
        match source {
            SyncSource::Sim => Fit::Envelope(EnvelopeFit::sixty_seconds()),
            SyncSource::Fc | SyncSource::Hub | SyncSource::GnssPps => {
                Fit::Linear(ClockFit::sixty_seconds())
            }
        }
    }

    /// Judge a pair that reads `behind_ns` behind the prediction (negative:
    /// ahead of it). A linear fit gates `|behind|` at `max(5 sigma, 1 ms)`
    /// and calls anything beyond [`RESET_DISAGREE_NS`] large. An envelope
    /// accepts any lateness up to [`ENVELOPE_LATE_MAX_NS`]; a pair ahead of
    /// the envelope is gated like a linear miss once sigma is measured, and
    /// before that is accepted up to [`ENVELOPE_LATE_MAX_NS`] (the envelope
    /// is still finding the latency floor).
    pub fn judge(&self, behind_ns: i64) -> Miss {
        let sigma = self.sigma_ns();
        let gate = (sigma.max(0) as u64 * OUTLIER_SIGMAS).max(OUTLIER_FLOOR_NS);
        let miss = behind_ns.unsigned_abs();
        match self {
            Fit::Linear(_) => {
                if miss > RESET_DISAGREE_NS {
                    Miss::Large
                } else if sigma >= 0 && miss > gate {
                    Miss::Outlier
                } else {
                    Miss::Fine
                }
            }
            Fit::Envelope(_) => {
                if behind_ns >= 0 || sigma < 0 {
                    if miss > ENVELOPE_LATE_MAX_NS {
                        Miss::Large
                    } else {
                        Miss::Fine
                    }
                } else if miss > RESET_DISAGREE_NS {
                    Miss::Large
                } else if miss > gate {
                    Miss::Outlier
                } else {
                    Miss::Fine
                }
            }
        }
    }

    /// Whether the pairs span enough host time to lock: always for a linear
    /// fit, [`ENVELOPE_LOCK_SPAN_NS`] for an envelope.
    pub fn baseline_ok(&self) -> bool {
        match self {
            Fit::Linear(_) => true,
            Fit::Envelope(f) => f.span_ns() >= ENVELOPE_LOCK_SPAN_NS,
        }
    }

    pub fn push(&mut self, host_ns: u64, source_ns: i64) {
        match self {
            Fit::Linear(f) => f.push(host_ns, source_ns),
            Fit::Envelope(f) => f.push(host_ns, source_ns),
        }
    }

    pub fn reset(&mut self) {
        match self {
            Fit::Linear(f) => f.reset(),
            Fit::Envelope(f) => f.reset(),
        }
    }

    pub fn len(&self) -> usize {
        match self {
            Fit::Linear(f) => f.len(),
            Fit::Envelope(f) => f.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn sigma_ns(&self) -> i64 {
        match self {
            Fit::Linear(f) => f.sigma_ns(),
            Fit::Envelope(f) => f.sigma_ns(),
        }
    }

    pub fn offset_i64(&self, host_ns: u64) -> Option<i64> {
        match self {
            Fit::Linear(f) => f.offset_i64(host_ns),
            Fit::Envelope(f) => f.offset_i64(host_ns),
        }
    }

    pub fn to_source(&self, host_ns: u64) -> Option<i64> {
        match self {
            Fit::Linear(f) => f.to_source(host_ns),
            Fit::Envelope(f) => f.to_source(host_ns),
        }
    }

    pub fn skew_ppm(&self) -> f64 {
        match self {
            Fit::Linear(f) => f.skew_ppm(),
            Fit::Envelope(f) => f.skew_ppm(),
        }
    }
}
