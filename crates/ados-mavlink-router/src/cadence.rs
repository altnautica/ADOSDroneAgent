//! Measure how fast, and how evenly, a message actually arrives from the
//! flight controller.
//!
//! ## Why this exists
//!
//! A control loop that closes on the aircraft is judged on its tail, not its
//! average. "Roughly 50 Hz" says nothing about whether the worst frame in a
//! second arrived 20 ms late or 200 ms late, and it is the worst frame that
//! decides whether a rate loop is viable at all. Nothing in this agent measured
//! that: the router counted messages, and a count over a window is an average
//! by another name.
//!
//! So this records the arrival of each message individually and reports the
//! distribution of the gaps between them, plus how often the gap exceeded a
//! deadline. Those three numbers -- achieved rate, tail latency, deadline
//! misses -- are what a decision about closing a loop over this link rests on.
//!
//! ## What it deliberately does not do
//!
//! It does not smooth, extrapolate, or fill a gap. A link that stops delivering
//! produces a long period, and that long period is the measurement, not an
//! outlier to be cleaned up. The one place a sample is dropped is the first
//! arrival after a reset, which has no predecessor to be measured against and
//! would otherwise report the age of the process as a period.
//!
//! ## Cost
//!
//! One instant and a ring push per message. The ring is bounded, so a link
//! delivering for hours costs the same as one delivering for seconds, and
//! nothing here allocates after construction.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// How many inter-arrival gaps are retained for the distribution.
///
/// At a few hundred hertz this is a couple of seconds of history, which is the
/// scale a loop-rate question is asked at -- long enough for a tail to appear,
/// short enough that a stall ten minutes ago is not still being reported as
/// though it were current.
pub const CADENCE_WINDOW: usize = 512;

/// One message stream's arrival cadence.
#[derive(Debug)]
pub struct InboundCadence {
    /// When the previous message arrived. `None` before the first, and after a
    /// reset.
    last: Option<Instant>,
    /// Recent inter-arrival gaps, oldest first, bounded to [`CADENCE_WINDOW`].
    gaps: VecDeque<Duration>,
    /// Gaps that exceeded the deadline, cumulative since construction.
    missed: u64,
    /// Total arrivals seen, cumulative. Counts the first arrival, which
    /// produces no gap.
    arrivals: u64,
    /// The gap above which an arrival counts as late.
    deadline: Duration,
}

/// A point-in-time read of the cadence.
#[derive(Debug, Clone, PartialEq)]
pub struct CadenceSnapshot {
    /// Arrivals seen since construction.
    pub arrivals: u64,
    /// Gaps retained in the current window.
    pub samples: usize,
    /// Achieved rate over the retained window, derived from the MEAN gap.
    ///
    /// `None` until at least one gap exists. Derived from the window rather
    /// than from a count over wall time so it describes the same population the
    /// percentiles below do.
    pub achieved_hz: Option<f64>,
    /// Median gap.
    pub p50: Option<Duration>,
    /// 95th-percentile gap.
    pub p95: Option<Duration>,
    /// 99th-percentile gap -- the tail the decision actually rests on.
    pub p99: Option<Duration>,
    /// Largest gap in the window.
    pub worst: Option<Duration>,
    /// Arrivals later than the deadline, cumulative.
    pub missed_deadline: u64,
}

impl InboundCadence {
    /// A cadence tracker whose arrivals are late past `deadline`.
    pub fn new(deadline: Duration) -> Self {
        Self {
            last: None,
            gaps: VecDeque::with_capacity(CADENCE_WINDOW),
            missed: 0,
            arrivals: 0,
            deadline,
        }
    }

    /// Record an arrival at `now`.
    ///
    /// The first arrival after construction or a reset establishes the origin
    /// and contributes no gap: there is nothing before it, and measuring
    /// against the process start would report uptime as a period.
    ///
    /// A clock that steps backwards yields no sample rather than a nonsense
    /// one. `Instant` is monotonic, so this is defence against a future caller
    /// injecting a wall clock, not against the platform.
    pub fn record(&mut self, now: Instant) {
        self.arrivals = self.arrivals.saturating_add(1);
        if let Some(prev) = self.last {
            if let Some(gap) = now.checked_duration_since(prev) {
                if gap > self.deadline {
                    self.missed = self.missed.saturating_add(1);
                }
                if self.gaps.len() == CADENCE_WINDOW {
                    self.gaps.pop_front();
                }
                self.gaps.push_back(gap);
            }
        }
        self.last = Some(now);
    }

    /// Forget the timing history, keeping the cumulative counters.
    ///
    /// Called when the link drops, so the gap spanning an outage is not
    /// reported as though the flight controller had simply been slow. The
    /// outage is a link fact, not a cadence fact, and conflating them would put
    /// a multi-second period into a distribution describing milliseconds.
    pub fn reset_stream(&mut self) {
        self.last = None;
        self.gaps.clear();
    }

    /// The current distribution.
    pub fn snapshot(&self) -> CadenceSnapshot {
        if self.gaps.is_empty() {
            return CadenceSnapshot {
                arrivals: self.arrivals,
                samples: 0,
                achieved_hz: None,
                p50: None,
                p95: None,
                p99: None,
                worst: None,
                missed_deadline: self.missed,
            };
        }
        let mut sorted: Vec<Duration> = self.gaps.iter().copied().collect();
        sorted.sort_unstable();
        let total: Duration = sorted.iter().sum();
        let mean = total / sorted.len() as u32;
        CadenceSnapshot {
            arrivals: self.arrivals,
            samples: sorted.len(),
            achieved_hz: (mean > Duration::ZERO).then(|| 1.0 / mean.as_secs_f64()),
            p50: Some(percentile(&sorted, 0.50)),
            p95: Some(percentile(&sorted, 0.95)),
            p99: Some(percentile(&sorted, 0.99)),
            worst: sorted.last().copied(),
            missed_deadline: self.missed,
        }
    }
}

/// Nearest-rank percentile over an ascending slice, matching the store's own
/// selection so a figure read here and a figure charted there mean the same
/// thing. Panics on an empty slice; every caller checks first.
fn percentile(sorted: &[Duration], q: f64) -> Duration {
    let n = sorted.len();
    let rank = (((q * n as f64).ceil() as usize).max(1) - 1).min(n - 1);
    sorted[rank]
}

#[cfg(test)]
mod tests;
