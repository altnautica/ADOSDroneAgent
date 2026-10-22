//! MAVLink TIMESYNC exchanges with the flight controller.
//!
//! The router sends `TIMESYNC{tc1: 0, ts1: host monotonic ns}`; the FC answers
//! with `tc1` set to its own clock (ns since its boot) and `ts1` echoed. A
//! reply is accepted only when `ts1` names a request this router sent and has
//! not matched yet, so another ground station's exchanges on the same link are
//! ignored. Accepted exchanges feed [`TimesyncEstimator`]; an FC clock that
//! runs backwards between replies means the FC rebooted, and the estimator is
//! reset before the new exchange is fed.

use std::collections::VecDeque;

use ados_clock::fit::TimesyncEstimator;
use ados_sensorbus::{SyncObs, SyncSource};

/// Outstanding requests remembered for matching (the cadence is 1 Hz, so this
/// tolerates replies up to this many seconds late).
const PENDING_CAP: usize = 8;

/// The result of one reply.
#[derive(Debug, Clone, PartialEq)]
pub enum Reply {
    /// Not an answer to one of our requests.
    Ignored,
    /// Matched, but the estimator's round-trip gate rejected it.
    Rejected,
    /// Matched and accepted; publish this pair.
    Accepted(SyncObs),
}

#[derive(Debug, Default)]
pub struct TimesyncTracker {
    pending: VecDeque<i64>,
    estimator: TimesyncEstimator,
    last_tc1: Option<i64>,
    /// Bumped on every reset; the lane compares it to drop per-boot state.
    epoch: u64,
}

impl TimesyncTracker {
    /// Record a request sent at host time `ts1_ns`.
    pub fn sent(&mut self, ts1_ns: u64) {
        if self.pending.len() == PENDING_CAP {
            self.pending.pop_front();
        }
        self.pending.push_back(ts1_ns as i64);
    }

    /// Feed a received `TIMESYNC(tc1, ts1)` that arrived at host `rx_ns`.
    pub fn on_timesync(&mut self, tc1: i64, ts1: i64, rx_ns: u64) -> Reply {
        if tc1 == 0 {
            return Reply::Ignored;
        }
        let Some(pos) = self.pending.iter().position(|&p| p == ts1) else {
            return Reply::Ignored;
        };
        self.pending.remove(pos);
        if self.last_tc1.is_some_and(|last| tc1 < last) {
            self.reset();
        }
        self.last_tc1 = Some(tc1);
        match self.estimator.on_exchange(ts1 as u64, tc1, rx_ns) {
            Some((host_ns, source_ns, rtt_ns)) => Reply::Accepted(SyncObs {
                source: SyncSource::Fc,
                host_ns,
                source_ns,
                rtt_ns,
                sim_time: false,
            }),
            None => Reply::Rejected,
        }
    }

    /// Host monotonic ns for an FC boot-clock time, when a relation is known.
    pub fn to_host(&self, fc_ns: i64) -> Option<u64> {
        self.estimator.fit().to_host(fc_ns)
    }

    /// Reset count, so per-FC-boot state elsewhere can follow a reboot.
    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    fn reset(&mut self) {
        self.estimator.reset();
        self.last_tc1 = None;
        self.epoch += 1;
        tracing::info!("fc_timesync_reset_clock_jumped_back");
    }
}
