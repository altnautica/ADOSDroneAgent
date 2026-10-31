//! Lane-open retry gate and the tee's counter accounting.

use std::sync::atomic::Ordering;
use std::time::Instant;

use ados_protocol::aux_egress::AuxEgressError;

use super::{
    LaneGate, TeeCounters, TeeCountersSnapshot, DISABLED_RECHECK, OPEN_BACKOFF_MAX,
    OPEN_BACKOFF_MIN,
};

impl TeeCounters {
    /// A consistent-enough point-in-time copy for logging and status surfaces.
    pub fn snapshot(&self) -> TeeCountersSnapshot {
        TeeCountersSnapshot {
            frames_forwarded: self.frames_forwarded.load(Ordering::Relaxed),
            bytes_forwarded: self.bytes_forwarded.load(Ordering::Relaxed),
            dropped_oversize: self.dropped_oversize.load(Ordering::Relaxed),
            dropped_rate_limit: self.dropped_rate_limit.load(Ordering::Relaxed),
            dropped_lane_down: self.dropped_lane_down.load(Ordering::Relaxed),
            dropped_lagged: self.dropped_lagged.load(Ordering::Relaxed),
            open_errors: self.open_errors.load(Ordering::Relaxed),
            send_errors: self.send_errors.load(Ordering::Relaxed),
        }
    }
}

impl Default for LaneGate {
    fn default() -> Self {
        Self {
            next_attempt: None,
            backoff: OPEN_BACKOFF_MIN,
            outage_logged: false,
        }
    }
}

impl LaneGate {
    pub(super) fn may_attempt(&self, now: Instant) -> bool {
        self.next_attempt.is_none_or(|at| now >= at)
    }

    /// The lane came up. Only ever called on a transition, because an open lane
    /// short-circuits before the gate is consulted.
    pub(super) fn on_open(&mut self) {
        self.next_attempt = None;
        self.backoff = OPEN_BACKOFF_MIN;
        self.outage_logged = false;
        tracing::info!("mavlink_aux_lane_open");
    }

    /// The operator dead switch is off. Reported once, then re-checked slowly.
    pub(super) fn on_disabled(&mut self, now: Instant) {
        if !self.outage_logged {
            self.outage_logged = true;
            tracing::info!(
                recheck_s = DISABLED_RECHECK.as_secs(),
                "mavlink_aux_lane_disabled"
            );
        }
        self.next_attempt = Some(now + DISABLED_RECHECK);
    }

    /// The lane could not be opened, or a send failed. Backs off exponentially.
    pub(super) fn on_failure(&mut self, now: Instant, error: &AuxEgressError) {
        if !self.outage_logged {
            self.outage_logged = true;
            tracing::warn!(error = %error, "mavlink_aux_lane_unavailable");
        } else {
            tracing::debug!(error = %error, "mavlink_aux_lane_still_unavailable");
        }
        self.next_attempt = Some(now + self.backoff);
        self.backoff = (self.backoff * 2).min(OPEN_BACKOFF_MAX);
    }
}

/// Log the counters when they have moved since the last report.
pub(super) fn report(counters: &TeeCounters, last: TeeCountersSnapshot) -> TeeCountersSnapshot {
    let now = counters.snapshot();
    if now != last {
        tracing::info!(
            frames_forwarded = now.frames_forwarded,
            bytes_forwarded = now.bytes_forwarded,
            dropped_oversize = now.dropped_oversize,
            dropped_rate_limit = now.dropped_rate_limit,
            dropped_lane_down = now.dropped_lane_down,
            dropped_lagged = now.dropped_lagged,
            open_errors = now.open_errors,
            send_errors = now.send_errors,
            "mavlink_aux_tee_counters"
        );
    }
    now
}
