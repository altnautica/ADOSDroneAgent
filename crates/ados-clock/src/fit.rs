//! The per-source fit, chosen by how the source's pairs are stamped, and the
//! verdict on a pair against its prediction.

use ados_clock_fit::envelope::{EnvelopeFit, ENVELOPE_LOCK_SPAN_NS};
use ados_clock_fit::fit::ClockFit;
use ados_sensorbus::SyncSource;

use crate::clock::{ENVELOPE_LATE_MAX_NS, OUTLIER_FLOOR_NS, OUTLIER_SIGMAS, RESET_DISAGREE_NS};

/// The latency floor assumed when none is configured: encode, RTSP and
/// decode of a rendered frame take at least this long at most. Its whole
/// width goes into the published sigma, never into the offset.
pub const UNKNOWN_FLOOR_BOUND_NS: u64 = 30_000_000;

/// The pipeline's minimum latency under an arrival-stamped source, which
/// one-way pairs cannot measure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LatencyFloor {
    /// Measured and configured (`clock.sim.latency_floor_ns`): added to the
    /// envelope's offset; its own error is the measurement's.
    Known(u64),
    /// Not measured: the offset is the envelope's, and the published sigma
    /// carries [`UNKNOWN_FLOOR_BOUND_NS`].
    Unknown,
}

/// How a pair relates to its source's prediction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Miss {
    /// Feed it to the fit; it ends any run of large misses.
    Fine,
    /// Feed it to the fit; it says nothing about a restart (an arrival-stamped
    /// pair behind its envelope: latency, not a clock change).
    Late,
    /// Drop it; it ends any run of large misses.
    Outlier,
    /// Drop it. [`crate::clock::RESET_AFTER`] of these within
    /// [`crate::clock::RESET_RUN_WINDOW_NS`] mean the source clock changed.
    Large,
}

#[derive(Debug, Clone)]
pub enum Fit {
    /// Pairs with symmetric error: two-way TIMESYNC midpoints, kernel PPS
    /// stamps, the hub's already-enveloped pairs. Least squares.
    Linear(ClockFit),
    /// Pairs stamped on arrival (the simulator SEI): a one-sided, jittery
    /// latency above a floor. Lower envelope.
    Envelope {
        fit: EnvelopeFit,
        floor: LatencyFloor,
    },
}

impl Fit {
    /// `floor` applies to the arrival-stamped source only.
    pub fn for_source(source: SyncSource, floor: LatencyFloor) -> Self {
        match source {
            SyncSource::Sim => Fit::Envelope {
                fit: EnvelopeFit::sixty_seconds(),
                floor,
            },
            SyncSource::Fc | SyncSource::Hub | SyncSource::GnssPps => {
                Fit::Linear(ClockFit::sixty_seconds())
            }
        }
    }

    /// Judge the pair `(host_ns, source_ns)` against the fit's own line (no
    /// floor): the verdict and how far the pair reads behind the line
    /// (negative: ahead of it). `None` while the fit has no line.
    ///
    /// A linear fit gates `|behind|` at `max(5 sigma, 1 ms)` and calls
    /// anything beyond [`RESET_DISAGREE_NS`] large. An envelope takes any
    /// lateness up to [`ENVELOPE_LATE_MAX_NS`] as [`Miss::Late`]. A pair ahead
    /// of the envelope cannot happen while the relation holds: once sigma is
    /// measured, one ahead by more than the gate is large (a run of them is a
    /// clock step or a lower latency floor, and restarts the fit). Before
    /// that the envelope is still finding the floor, and a pair up to
    /// [`ENVELOPE_LATE_MAX_NS`] ahead is taken.
    pub fn judge(&self, host_ns: u64, source_ns: i64) -> Option<(Miss, i64)> {
        let predicted = match self {
            Fit::Linear(f) => f.to_source(host_ns),
            Fit::Envelope { fit, .. } => fit.to_source(host_ns),
        }?;
        let behind = predicted.saturating_sub(source_ns);
        let sigma = self.fit_sigma_ns();
        let gate = (sigma.max(0) as u64 * OUTLIER_SIGMAS).max(OUTLIER_FLOOR_NS);
        let miss = behind.unsigned_abs();
        let verdict = match self {
            Fit::Linear(_) if miss > RESET_DISAGREE_NS => Miss::Large,
            Fit::Linear(_) if sigma >= 0 && miss > gate => Miss::Outlier,
            Fit::Linear(_) => Miss::Fine,
            Fit::Envelope { .. } if miss > ENVELOPE_LATE_MAX_NS => Miss::Large,
            Fit::Envelope { .. } if behind >= 0 || sigma < 0 || miss <= gate => Miss::Late,
            Fit::Envelope { .. } => Miss::Large,
        };
        Some((verdict, behind))
    }

    /// Whether the pairs span enough host time to lock: always for a linear
    /// fit, [`ENVELOPE_LOCK_SPAN_NS`] for an envelope.
    pub fn baseline_ok(&self) -> bool {
        match self {
            Fit::Linear(_) => true,
            Fit::Envelope { fit, .. } => fit.span_ns() >= ENVELOPE_LOCK_SPAN_NS,
        }
    }

    pub fn push(&mut self, host_ns: u64, source_ns: i64) {
        match self {
            Fit::Linear(f) => f.push(host_ns, source_ns),
            Fit::Envelope { fit, .. } => fit.push(host_ns, source_ns),
        }
    }

    pub fn reset(&mut self) {
        match self {
            Fit::Linear(f) => f.reset(),
            Fit::Envelope { fit, .. } => fit.reset(),
        }
    }

    pub fn len(&self) -> usize {
        match self {
            Fit::Linear(f) => f.len(),
            Fit::Envelope { fit, .. } => fit.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The fit's own residual, ns (`-1` unmeasured): what lock and the
    /// outlier gate judge the fit by.
    pub fn fit_sigma_ns(&self) -> i64 {
        match self {
            Fit::Linear(f) => f.sigma_ns(),
            Fit::Envelope { fit, .. } => fit.sigma_ns(),
        }
    }

    /// The published one-sigma error of the offset, ns (`-1` unmeasured):
    /// the fit residual, and for an envelope with no measured floor the
    /// floor bound with it.
    pub fn sigma_ns(&self) -> i64 {
        let sigma = self.fit_sigma_ns();
        match self {
            Fit::Envelope {
                floor: LatencyFloor::Unknown,
                ..
            } if sigma >= 0 => {
                let (s, b) = (sigma as f64, UNKNOWN_FLOOR_BOUND_NS as f64);
                (s * s + b * b).sqrt().round() as i64
            }
            _ => sigma,
        }
    }

    /// Offset `source - host` at host time `host_ns`, the known floor added.
    pub fn offset_i64(&self, host_ns: u64) -> Option<i64> {
        match self {
            Fit::Linear(f) => f.offset_i64(host_ns),
            Fit::Envelope { fit, floor } => {
                let add = match floor {
                    LatencyFloor::Known(f) => *f as i64,
                    LatencyFloor::Unknown => 0,
                };
                fit.offset_i64(host_ns).map(|o| o.saturating_add(add))
            }
        }
    }

    /// Source time at host time `host_ns`.
    pub fn to_source(&self, host_ns: u64) -> Option<i64> {
        self.offset_i64(host_ns)
            .map(|o| (host_ns as i128 + o as i128) as i64)
    }

    pub fn skew_ppm(&self) -> f64 {
        match self {
            Fit::Linear(f) => f.skew_ppm(),
            Fit::Envelope { fit, .. } => fit.skew_ppm(),
        }
    }
}
