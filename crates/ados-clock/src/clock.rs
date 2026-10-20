//! Per-source clock fits, the lock verdict and the session timebase choice.
//!
//! Every [`SyncObs`] pair feeds the windowed fit of its source against the
//! host monotonic clock: least squares for pairs with symmetric error, the
//! lower envelope for pairs stamped on arrival (see [`crate::fit::Fit`]). A
//! source is locked when its fit has enough recent pairs and a small
//! residual. The timebase the next capture session uses is the best locked
//! source: simulator time, then GNSS PPS (UTC), then the sensor hub's
//! free-running clock, else the host monotonic clock itself.
//!
//! Simulator time has two sources. A simulated sensor hub runs on the
//! simulator's clock (its pairs carry `sim_time`), and the hub service maps
//! its own samples onto the host through the same pairs, so a session on the
//! hub fit gets the hub's stamps back exactly and frames keep their own
//! simulator stamps: that path is preferred. Without one, the video SEI
//! pairs' envelope ([`SyncSource::Sim`]) carries the session, its offset
//! short by the unmeasured pipeline latency floor unless one is configured,
//! and its sigma saying so.

use ados_sensorbus::{v1, SyncObs, SyncSource};
use serde::Serialize;

use crate::fit::{Fit, LatencyFloor, Miss};

/// Pairs a fit needs before its source can be locked.
pub const LOCK_MIN_PAIRS: usize = 5;
/// The newest pair must be younger than this for the source to be locked.
pub const LOCK_MAX_AGE_NS: u64 = 3_000_000_000;
/// The fit residual must be below this for the source to be locked.
pub const LOCK_MAX_SIGMA_NS: i64 = 5_000_000;
/// A pair this far from the fit's prediction means the source restarted its
/// clock; after [`RESET_AFTER`] such pairs the fit starts over.
pub const RESET_DISAGREE_NS: u64 = 50_000_000;
/// An arrival-stamped pair may read up to this far behind its envelope (a
/// delayed frame); further behind counts toward a restart like
/// [`RESET_DISAGREE_NS`].
pub const ENVELOPE_LATE_MAX_NS: u64 = 1_000_000_000;
/// Large disagreements that restart a fit, when they fall within
/// [`RESET_RUN_WINDOW_NS`] with no in-line pair between them. A single wild
/// pair (a delayed packet, a glitch) is dropped instead.
pub const RESET_AFTER: u32 = 3;
/// A run of large disagreements older than this starts over.
pub const RESET_RUN_WINDOW_NS: u64 = 10_000_000_000;
/// The smallest disagreement treated as an outlier, whatever the sigma.
pub const OUTLIER_FLOOR_NS: u64 = 1_000_000;
/// Outlier gate in sigmas.
pub const OUTLIER_SIGMAS: u64 = 5;
/// Pairs older than this no longer predict anything (the fit window).
const FIT_WINDOW_NS: u64 = 60_000_000_000;

/// What [`ClockState::on_sync`] did with a pair.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncOutcome {
    /// Fed to the fit.
    Accepted,
    /// Disagreed with the prediction; dropped and counted.
    Outlier,
    /// Ended a run of large disagreements: the fit restarted from it.
    Reset,
}

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
    fit: Fit,
    /// Host ns of the newest accepted pair, 0 when none arrived yet.
    last_update_ns: u64,
    /// Large disagreements in the current run.
    large_run: u32,
    /// Host ns of the run's first large disagreement.
    run_start_ns: u64,
    /// Pairs dropped as outliers.
    outliers: u64,
    /// The newest accepted pair said the source runs on simulator time.
    sim_time: bool,
}

/// One `sources[]` row of the clock document.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SourceReport {
    pub id: &'static str,
    pub kind: &'static str,
    /// Tracked: enough fresh pairs and a fit residual under 5 ms. `sigma_ns`
    /// is the offset's error, which can be larger (an unmeasured floor).
    pub locked: bool,
    pub offset_ns: i64,
    pub skew_ppm: f64,
    pub sigma_ns: i64,
    pub last_update_ns: u64,
    /// The source runs on the simulator's clock.
    pub sim_time: bool,
}

/// The `/run/ados/sensing/clock.json` document (the `GET /api/v1/clock` body without
/// `present`).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ClockReport {
    pub version: u16,
    pub timebase: Timebase,
    /// The `sources[].id` the timebase follows, `None` on host monotonic.
    pub source: Option<&'static str>,
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
    /// No measured latency floor under the video SEI pairs.
    pub fn new() -> Self {
        Self::with_sim_floor(LatencyFloor::Unknown)
    }

    /// `sim_floor`: the latency floor under the video SEI pairs.
    pub fn with_sim_floor(sim_floor: LatencyFloor) -> Self {
        let fresh = |source| Source {
            fit: Fit::for_source(source, sim_floor),
            last_update_ns: 0,
            large_run: 0,
            run_start_ns: 0,
            outliers: 0,
            sim_time: false,
        };
        ClockState {
            sources: SyncSource::ALL.map(fresh),
        }
    }

    /// Feed one pair. A pair the fit judges an outlier (see [`Fit::judge`])
    /// is dropped; [`RESET_AFTER`] large disagreements within
    /// [`RESET_RUN_WINDOW_NS`] mean the source clock changed, and the fit
    /// starts over from the last of them.
    pub fn on_sync(&mut self, obs: &SyncObs) -> SyncOutcome {
        let src = &mut self.sources[idx(obs.source)];
        // A fit whose newest pair is older than the window predicts nothing.
        if obs.host_ns.saturating_sub(src.last_update_ns) > FIT_WINDOW_NS {
            src.fit.reset();
            src.large_run = 0;
        }
        let mut outcome = SyncOutcome::Accepted;
        if let Some((verdict, behind)) = src.fit.judge(obs.host_ns, obs.source_ns) {
            match verdict {
                Miss::Large => {
                    if src.large_run == 0
                        || obs.host_ns.saturating_sub(src.run_start_ns) > RESET_RUN_WINDOW_NS
                    {
                        src.large_run = 0;
                        src.run_start_ns = obs.host_ns;
                    }
                    src.large_run += 1;
                    if src.large_run >= RESET_AFTER {
                        tracing::warn!(
                            source = obs.source.id(),
                            behind_ns = behind,
                            "clock_source_reset"
                        );
                        src.fit.reset();
                        src.large_run = 0;
                        outcome = SyncOutcome::Reset;
                    } else {
                        outcome = SyncOutcome::Outlier;
                    }
                }
                Miss::Outlier => {
                    src.large_run = 0;
                    outcome = SyncOutcome::Outlier;
                }
                Miss::Fine => src.large_run = 0,
                Miss::Late => {}
            }
        }
        if outcome == SyncOutcome::Outlier {
            src.outliers += 1;
            tracing::debug!(
                source = obs.source.id(),
                outliers = src.outliers,
                "clock_pair_outlier"
            );
            return outcome;
        }
        src.fit.push(obs.host_ns, obs.source_ns);
        src.last_update_ns = obs.host_ns;
        src.sim_time = obs.sim_time;
        outcome
    }

    /// Pairs from `source` dropped as outliers so far.
    pub fn outliers(&self, source: SyncSource) -> u64 {
        self.sources[idx(source)].outliers
    }

    /// Whether `source` is locked at host time `now_ns`: enough fresh pairs
    /// over enough host time, and a fit residual under
    /// [`LOCK_MAX_SIGMA_NS`].
    pub fn locked(&self, source: SyncSource, now_ns: u64) -> bool {
        let src = &self.sources[idx(source)];
        let sigma = src.fit.fit_sigma_ns();
        src.fit.len() >= LOCK_MIN_PAIRS
            && src.fit.baseline_ok()
            && now_ns.saturating_sub(src.last_update_ns) < LOCK_MAX_AGE_NS
            && (0..LOCK_MAX_SIGMA_NS).contains(&sigma)
    }

    /// The hub is a locked simulator hub: the exact simulator-time source.
    fn sim_hub(&self, now_ns: u64) -> bool {
        self.sources[idx(SyncSource::Hub)].sim_time && self.locked(SyncSource::Hub, now_ns)
    }

    /// The timebase the next session would use at `now_ns`.
    pub fn timebase(&self, now_ns: u64) -> Timebase {
        choose_timebase(
            self.sim_hub(now_ns) || self.locked(SyncSource::Sim, now_ns),
            self.locked(SyncSource::GnssPps, now_ns),
            self.locked(SyncSource::Hub, now_ns),
        )
    }

    /// The source the session timebase follows at `now_ns`.
    fn session_source(&self, now_ns: u64) -> (Timebase, Option<SyncSource>) {
        let timebase = self.timebase(now_ns);
        let source = match timebase {
            Timebase::Sim if self.sim_hub(now_ns) => Some(SyncSource::Hub),
            Timebase::Sim => Some(SyncSource::Sim),
            Timebase::Gnss => Some(SyncSource::GnssPps),
            Timebase::HubFreerun => Some(SyncSource::Hub),
            Timebase::HostMonotonic => None,
        };
        (timebase, source)
    }

    /// UTC ns at host time `now_ns`, when the GNSS PPS source is locked.
    pub fn utc_ns(&self, now_ns: u64) -> Option<i64> {
        if !self.locked(SyncSource::GnssPps, now_ns) {
            return None;
        }
        self.sources[idx(SyncSource::GnssPps)].fit.to_source(now_ns)
    }

    /// `(timebase, source, locked, sigma_ns, session time now)`.
    fn session(&self, now_ns: u64) -> (Timebase, Option<SyncSource>, bool, i64, u64) {
        match self.session_source(now_ns) {
            (timebase, Some(source)) => {
                let fit = &self.sources[idx(source)].fit;
                let t = fit.to_source(now_ns).map_or(now_ns, |t| t.max(0) as u64);
                (timebase, Some(source), true, fit.sigma_ns(), t)
            }
            (timebase, None) => (timebase, None, false, -1, now_ns),
        }
    }

    /// The clock document at host time `now_ns`.
    pub fn report(&self, now_ns: u64) -> ClockReport {
        let (timebase, source, locked, sigma_ns, _) = self.session(now_ns);
        let sources = SyncSource::ALL
            .iter()
            .map(|&s| {
                let src = &self.sources[idx(s)];
                SourceReport {
                    id: s.id(),
                    kind: s.kind(),
                    locked: self.locked(s, now_ns),
                    offset_ns: src.fit.offset_i64(now_ns).unwrap_or(0),
                    skew_ppm: src.fit.skew_ppm(),
                    sigma_ns: src.fit.sigma_ns(),
                    last_update_ns: src.last_update_ns,
                    sim_time: src.sim_time,
                }
            })
            .collect();
        ClockReport {
            version: 1,
            timebase,
            source: source.map(SyncSource::id),
            locked,
            sigma_ns,
            t_ns: now_ns,
            utc_ns: self.utc_ns(now_ns),
            sources,
        }
    }

    /// The `clock` bus sample at host time `now_ns`.
    pub fn sample(&self, now_ns: u64) -> v1::ClockSample {
        let (_, _, locked, sigma_ns, t_ns) = self.session(now_ns);
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

#[cfg(test)]
#[path = "clock_sim_tests.rs"]
mod sim_tests;
