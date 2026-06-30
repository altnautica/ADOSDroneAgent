//! The live (in-flight) reconstruction cadence.
//!
//! The post-flight pipeline ([`crate::pipeline`]) trains a deliverable from a
//! finished bag. The live world model is the other path: keyframes arrive over
//! the relay as the drone flies, and the node keeps the world model fresh by
//! periodically running a REAL reconstruct over the keyframes accumulated so far.
//!
//! "Live" here is honest, real periodic reconstruction — NOT per-frame
//! incremental gaussian training (that is a research problem and is out of
//! scope). On a cadence (every N new keyframes, or every T since the last cycle,
//! whichever comes first) the node finalizes a snapshot of the growing dataset
//! and submits a fresh reconstruct job; each cycle yields a real `.ply` + `.rrd`
//! tagged to the session so the GCS shows the latest world model.
//!
//! This module owns the PURE cadence decision: given the keyframes persisted and
//! the time elapsed since the last cycle, and whether a cycle is still running,
//! decide whether a new periodic reconstruct is due now. The I/O — snapshotting
//! the dataset, submitting the job, running the backend — lives in
//! [`crate::ingest`] and the daemon, so the cadence stays testable with no disk,
//! no store, and no clock of its own.

use serde::{Deserialize, Serialize};

/// Default cadence: start a periodic reconstruct every this many NEW keyframes...
pub const DEFAULT_RECONSTRUCT_EVERY_KEYFRAMES: u64 = 30;
/// ...or this many milliseconds since the last cycle, whichever comes first.
pub const DEFAULT_RECONSTRUCT_INTERVAL_MS: i64 = 20_000;
/// The minimum keyframes a session needs before the FIRST periodic reconstruct
/// is worth attempting — a reconstruct over one or two frames is not a useful
/// world model.
pub const DEFAULT_MIN_KEYFRAMES: u64 = 8;

/// Live-reconstruction cadence configuration for a node.
///
/// Opt-in: when `enabled` is false the node only reconstructs the final bag (the
/// post-flight path), so a node that does not want a live-updating world model is
/// byte-unchanged. The thresholds are the cadence: a new cycle becomes due once
/// `every_keyframes` new keyframes have arrived since the last cycle OR
/// `interval_ms` has elapsed since it, whichever first, provided the session has
/// at least `min_keyframes` persisted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct LiveReconstructConfig {
    /// Run periodic reconstructs during an active session (opt-in).
    pub enabled: bool,
    /// New keyframes since the last cycle that force a reconstruct.
    pub every_keyframes: u64,
    /// Milliseconds since the last cycle that force a reconstruct.
    pub interval_ms: i64,
    /// Keyframes a session must have before its first periodic reconstruct.
    pub min_keyframes: u64,
}

impl Default for LiveReconstructConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            every_keyframes: DEFAULT_RECONSTRUCT_EVERY_KEYFRAMES,
            interval_ms: DEFAULT_RECONSTRUCT_INTERVAL_MS,
            min_keyframes: DEFAULT_MIN_KEYFRAMES,
        }
    }
}

/// Tracks the live-reconstruction cadence for ONE capture session.
///
/// Pure: it is fed keyframe arrivals + the clock and reports when a periodic
/// reconstruct is due; the caller performs the snapshot + submit, then tells the
/// driver a cycle started ([`begin_cycle`](Self::begin_cycle)) and, when the
/// job reaches a terminal state, finished ([`note_cycle_finished`](Self::note_cycle_finished)).
/// The skip-while-running guard lives here: a new cycle is never due while one is
/// in flight, so cycles coalesce instead of piling up.
#[derive(Debug, Clone)]
pub struct LiveReconstructDriver {
    config: LiveReconstructConfig,
    /// Keyframes persisted this session so far (the snapshot size).
    persisted: u64,
    /// `persisted` at the last cycle start, so the trigger counts NEW keyframes.
    persisted_at_last_cycle: u64,
    /// The clock at the last cycle start (the interval trigger's baseline); the
    /// session start time before the first cycle.
    last_cycle_ms: i64,
    /// Periodic cycles started this session (the per-cycle job-id discriminator).
    cycles: u64,
    /// Whether a periodic cycle is currently in flight (the skip-while-running
    /// guard); a new cycle is never due while this holds.
    cycle_in_flight: bool,
}

impl LiveReconstructDriver {
    /// A driver for `config`, with the interval baseline at `now_ms`.
    pub fn new(config: LiveReconstructConfig, now_ms: i64) -> Self {
        Self {
            config,
            persisted: 0,
            persisted_at_last_cycle: 0,
            last_cycle_ms: now_ms,
            cycles: 0,
            cycle_in_flight: false,
        }
    }

    /// Record one persisted keyframe (the snapshot grows by one).
    pub fn note_keyframe(&mut self) {
        self.persisted += 1;
    }

    /// Whether a periodic reconstruct is due now. False when disabled, while a
    /// cycle is in flight (skip-while-running), below the minimum-keyframe floor,
    /// or when no new keyframe has arrived since the last cycle. Otherwise true
    /// once enough new keyframes have arrived OR the interval has elapsed.
    pub fn due(&self, now_ms: i64) -> bool {
        if !self.config.enabled || self.cycle_in_flight {
            return false;
        }
        if self.persisted < self.config.min_keyframes {
            return false;
        }
        let new_keyframes = self.persisted.saturating_sub(self.persisted_at_last_cycle);
        if new_keyframes == 0 {
            // Nothing new since the last cycle: re-reconstructing the same set
            // would only repeat the same result.
            return false;
        }
        let elapsed = now_ms.saturating_sub(self.last_cycle_ms);
        new_keyframes >= self.config.every_keyframes || elapsed >= self.config.interval_ms
    }

    /// Mark a periodic cycle started: arm the skip-while-running guard, reset the
    /// new-keyframe + interval baselines, and return the cycle index (the
    /// per-cycle job-id / output discriminator).
    pub fn begin_cycle(&mut self, now_ms: i64) -> u64 {
        let cycle = self.cycles;
        self.cycles += 1;
        self.persisted_at_last_cycle = self.persisted;
        self.last_cycle_ms = now_ms;
        self.cycle_in_flight = true;
        cycle
    }

    /// Release the skip-while-running guard: the in-flight cycle's job reached a
    /// terminal state (or vanished). A no-op when no cycle is in flight.
    pub fn note_cycle_finished(&mut self) {
        self.cycle_in_flight = false;
    }

    /// Whether a periodic cycle is currently in flight.
    pub fn cycle_in_flight(&self) -> bool {
        self.cycle_in_flight
    }

    /// Keyframes persisted this session so far.
    pub fn persisted(&self) -> u64 {
        self.persisted
    }

    /// Periodic cycles started this session.
    pub fn cycles(&self) -> u64 {
        self.cycles
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enabled(every: u64, interval_ms: i64, min: u64) -> LiveReconstructConfig {
        LiveReconstructConfig {
            enabled: true,
            every_keyframes: every,
            interval_ms,
            min_keyframes: min,
        }
    }

    #[test]
    fn a_fresh_driver_with_no_keyframes_is_not_due() {
        let d = LiveReconstructDriver::new(enabled(4, 10_000, 2), 0);
        assert!(!d.due(0));
        assert!(
            !d.due(1_000_000),
            "no keyframes: never due, even far in time"
        );
        assert_eq!(d.cycles(), 0);
        assert!(!d.cycle_in_flight());
    }

    #[test]
    fn a_disabled_driver_is_never_due() {
        let mut d = LiveReconstructDriver::new(LiveReconstructConfig::default(), 0);
        for _ in 0..100 {
            d.note_keyframe();
        }
        assert!(
            !d.due(1_000_000),
            "the default config is disabled (opt-in), so a cycle is never due"
        );
    }

    #[test]
    fn the_keyframe_count_trigger_fires_a_cycle() {
        let mut d = LiveReconstructDriver::new(enabled(4, i64::MAX, 2), 0);
        for _ in 0..3 {
            d.note_keyframe();
        }
        assert!(!d.due(10), "3 < every_keyframes(4): not due yet");
        d.note_keyframe(); // 4th
        assert!(d.due(10), "4 new keyframes hits the count trigger");
    }

    #[test]
    fn the_interval_trigger_fires_a_cycle_for_a_slow_capture() {
        // A slow capture: few keyframes, but past the interval. Still needs at
        // least one new keyframe and the minimum-keyframe floor.
        let mut d = LiveReconstructDriver::new(enabled(30, 20_000, 2), 1_000);
        d.note_keyframe();
        d.note_keyframe(); // persisted = 2 == min
        assert!(!d.due(10_000), "before the interval elapses: not due");
        assert!(
            d.due(21_001),
            "past the 20s interval with >= min keyframes: due"
        );
    }

    #[test]
    fn the_minimum_keyframe_floor_gates_the_first_cycle() {
        let mut d = LiveReconstructDriver::new(enabled(4, 1_000, 8), 0);
        for _ in 0..7 {
            d.note_keyframe();
        }
        assert!(
            !d.due(1_000_000),
            "7 < min_keyframes(8): not due even past the interval and count"
        );
        d.note_keyframe(); // 8th: at the floor, and 8 >= every_keyframes(4)
        assert!(
            d.due(10),
            "at the minimum-keyframe floor the cycle becomes due"
        );
    }

    #[test]
    fn an_in_flight_cycle_blocks_the_next_until_it_finishes() {
        let mut d = LiveReconstructDriver::new(enabled(4, i64::MAX, 2), 0);
        for _ in 0..4 {
            d.note_keyframe();
        }
        assert!(d.due(10));

        let cycle = d.begin_cycle(10);
        assert_eq!(cycle, 0);
        assert!(d.cycle_in_flight());

        // Four more keyframes arrive while the cycle runs: still not due (the
        // skip-while-running guard coalesces, never piles up).
        for _ in 0..4 {
            d.note_keyframe();
        }
        assert!(!d.due(20), "a cycle in flight blocks the next");

        // The job finishes: the next cycle (over the new keyframes) becomes due.
        d.note_cycle_finished();
        assert!(!d.cycle_in_flight());
        assert!(d.due(20), "after the cycle finishes, the next is due");
        assert_eq!(d.begin_cycle(20), 1, "the cycle index advances");
    }

    #[test]
    fn no_new_keyframe_since_the_last_cycle_is_never_due() {
        let mut d = LiveReconstructDriver::new(enabled(4, 1_000, 2), 0);
        for _ in 0..4 {
            d.note_keyframe();
        }
        d.begin_cycle(10);
        d.note_cycle_finished();
        // No keyframe arrived since the cycle; even far past the interval, a
        // re-reconstruct of the identical set is not due.
        assert!(
            !d.due(1_000_000),
            "no new keyframes since the last cycle: not due"
        );
        d.note_keyframe();
        assert!(d.due(1_000_000), "one new keyframe past the interval: due");
    }
}
