//! Per-source clock fits, the lock verdict and the session timebase choice.
//!
//! Every [`SyncObs`] pair feeds the windowed linear fit of its source against
//! the host monotonic clock. A source is locked when its fit has enough recent
//! pairs and a small residual. The timebase the next capture session uses is
//! the best locked source: simulator time, then GNSS PPS (UTC), then the
//! sensor hub's free-running clock, else the host monotonic clock itself.

use ados_sensorbus::fit::ClockFit;
use ados_sensorbus::{v1, SyncObs, SyncSource};
use serde::Serialize;

/// Pairs a fit needs before its source can be locked.
pub const LOCK_MIN_PAIRS: usize = 5;
/// The newest pair must be younger than this for the source to be locked.
pub const LOCK_MAX_AGE_NS: u64 = 3_000_000_000;
/// The fit residual must be below this for the source to be locked.
pub const LOCK_MAX_SIGMA_NS: i64 = 5_000_000;
/// A pair this far from the fit's prediction means the source restarted its
/// clock; the fit starts over from that pair.
pub const RESET_DISAGREE_NS: u64 = 50_000_000;

/// The timebase a capture session records against (`/api/v1/clock`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Timebase {
    Gnss,
    HubFreerun,
    HostMonotonic,
    Sim,
}

/// Pick the timebase from the lock state of the candidate sources.
pub fn choose_timebase(sim: bool, gnss: bool, hub: bool) -> Timebase {
    if sim {
        Timebase::Sim
    } else if gnss {
        Timebase::Gnss
    } else if hub {
        Timebase::HubFreerun
    } else {
        Timebase::HostMonotonic
    }
}

#[derive(Debug, Clone)]
struct Source {
    fit: ClockFit,
    /// Host ns of the newest pair, 0 when none arrived yet.
    last_update_ns: u64,
}

/// One `sources[]` row of the clock document.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SourceReport {
    pub id: &'static str,
    pub kind: &'static str,
    pub locked: bool,
    pub offset_ns: i64,
    pub skew_ppm: f64,
    pub sigma_ns: i64,
    pub last_update_ns: u64,
}

/// The `/run/ados/clock.json` document (the `GET /api/v1/clock` body without
/// `present`).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ClockReport {
    pub version: u16,
    pub timebase: Timebase,
    pub locked: bool,
    pub sigma_ns: i64,
    pub t_ns: u64,
    pub utc_ns: Option<i64>,
    pub sources: Vec<SourceReport>,
}

/// The fits of every [`SyncSource`], in [`SyncSource::ALL`] order.
#[derive(Debug, Clone)]
pub struct ClockState {
    sources: [Source; 4],
}

impl Default for ClockState {
    fn default() -> Self {
        Self::new()
    }
}

fn idx(source: SyncSource) -> usize {
    match source {
        SyncSource::Fc => 0,
        SyncSource::Hub => 1,
        SyncSource::GnssPps => 2,
        SyncSource::Sim => 3,
    }
}

impl ClockState {
    pub fn new() -> Self {
        let fresh = || Source {
            fit: ClockFit::sixty_seconds(),
            last_update_ns: 0,
        };
        ClockState {
            sources: [fresh(), fresh(), fresh(), fresh()],
        }
    }

    /// Feed one pair. Returns true when the pair disagreed with the fit by
    /// more than [`RESET_DISAGREE_NS`] and the fit was restarted from it.
    pub fn on_sync(&mut self, obs: &SyncObs) -> bool {
        let src = &mut self.sources[idx(obs.source)];
        let mut reset = false;
        if let Some(predicted) = src.fit.to_source(obs.host_ns) {
            let miss = predicted.abs_diff(obs.source_ns);
            if miss > RESET_DISAGREE_NS {
                tracing::warn!(
                    source = obs.source.id(),
                    miss_ns = miss,
                    "clock_source_reset"
                );
                src.fit.reset();
                reset = true;
            }
        }
        src.fit.push(obs.host_ns, obs.source_ns);
        src.last_update_ns = obs.host_ns;
        reset
    }

    /// Whether `source` is locked at host time `now_ns`.
    pub fn locked(&self, source: SyncSource, now_ns: u64) -> bool {
        let src = &self.sources[idx(source)];
        let sigma = src.fit.sigma_ns();
        src.fit.len() >= LOCK_MIN_PAIRS
            && now_ns.saturating_sub(src.last_update_ns) < LOCK_MAX_AGE_NS
            && (0..LOCK_MAX_SIGMA_NS).contains(&sigma)
    }

    /// The timebase the next session would use at `now_ns`.
    pub fn timebase(&self, now_ns: u64) -> Timebase {
        choose_timebase(
            self.locked(SyncSource::Sim, now_ns),
            self.locked(SyncSource::GnssPps, now_ns),
            self.locked(SyncSource::Hub, now_ns),
        )
    }

    fn timebase_source(timebase: Timebase) -> Option<SyncSource> {
        match timebase {
            Timebase::Sim => Some(SyncSource::Sim),
            Timebase::Gnss => Some(SyncSource::GnssPps),
            Timebase::HubFreerun => Some(SyncSource::Hub),
            Timebase::HostMonotonic => None,
        }
    }

    /// UTC ns at host time `now_ns`, when the GNSS PPS source is locked.
    pub fn utc_ns(&self, now_ns: u64) -> Option<i64> {
        if !self.locked(SyncSource::GnssPps, now_ns) {
            return None;
        }
        self.sources[idx(SyncSource::GnssPps)].fit.to_source(now_ns)
    }

    /// `(timebase, locked, sigma_ns, session time now)`.
    fn session(&self, now_ns: u64) -> (Timebase, bool, i64, u64) {
        let timebase = self.timebase(now_ns);
        match Self::timebase_source(timebase) {
            Some(source) => {
                let fit = &self.sources[idx(source)].fit;
                let t = fit.to_source(now_ns).map_or(now_ns, |t| t.max(0) as u64);
                (timebase, true, fit.sigma_ns(), t)
            }
            None => (timebase, false, -1, now_ns),
        }
    }

    /// The clock document at host time `now_ns`.
    pub fn report(&self, now_ns: u64) -> ClockReport {
        let (timebase, locked, sigma_ns, _) = self.session(now_ns);
        let sources = SyncSource::ALL
            .iter()
            .map(|&s| {
                let src = &self.sources[idx(s)];
                SourceReport {
                    id: s.id(),
                    kind: s.kind(),
                    locked: self.locked(s, now_ns),
                    offset_ns: src.fit.offset_ns(now_ns).map_or(0, |o| o.round() as i64),
                    skew_ppm: src.fit.skew_ppm(),
                    sigma_ns: src.fit.sigma_ns(),
                    last_update_ns: src.last_update_ns,
                }
            })
            .collect();
        ClockReport {
            version: 1,
            timebase,
            locked,
            sigma_ns,
            t_ns: now_ns,
            utc_ns: self.utc_ns(now_ns),
            sources,
        }
    }

    /// The `clock` bus sample at host time `now_ns`.
    pub fn sample(&self, now_ns: u64) -> v1::ClockSample {
        let (_, locked, sigma_ns, t_ns) = self.session(now_ns);
        v1::ClockSample {
            t_ns,
            host_mono_ns: now_ns,
            utc_ns: self.utc_ns(now_ns),
            locked,
            sigma_ns,
        }
    }
}

#[cfg(test)]
#[path = "clock_tests.rs"]
mod tests;
