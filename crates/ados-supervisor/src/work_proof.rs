//! Delta-counter liveness: proof of WORK, not proof of process.
//!
//! `systemctl is-active` answers "has the process exited?", which is the wrong
//! question for a daemon that is alive and doing nothing. A MAVLink router
//! whose serial reader has wedged, a swarm beacon whose 2 Hz publish loop has
//! stopped, a CRSF lane holding the last stick frame, a vision engine that has
//! stopped consuming the camera ring — each keeps its unit `active` forever
//! while the lane it owns is dead. The supervisor's only liveness judgement
//! used to be `is-active`, so every one of those was invisible.
//!
//! So a supervised unit also has to show it moved bytes. The signal is the
//! cumulative read+write byte count of the unit's main process
//! (`/proc/<pid>/io`), sampled once per monitor pass. A counter that has not
//! advanced across the whole [`STALL_WINDOW`] while the unit reports active is
//! a stall, and the supervisor treats it exactly as it treats a death.
//!
//! Three rules keep this from becoming a false-positive generator, which on a
//! flight node would be worse than the defect it fixes:
//!
//! 1. **No reading, no verdict.** A counter that cannot be read at all — no
//!    `/proc` (a non-Linux dev host), a manager that cannot resolve a main PID,
//!    a PID recycling window — yields [`WorkVerdict::Unknown`], never a stall.
//! 2. **A gap is not flatness.** A missing sample clears the baseline instead
//!    of carrying the old timestamp forward, so a transient read failure cannot
//!    age into a stall verdict.
//! 3. **Only chosen units.** [`WORK_PROVEN_UNITS`] lists the four lanes whose
//!    silence is a real loss of capability. Units that are legitimately idle
//!    for long stretches are not judged this way.

use std::collections::HashMap;
use std::time::Duration;

// `tokio::time::Instant`, not `std::time::Instant`: identical in production,
// but a paused-clock test can drive the stall window deterministically instead
// of sleeping through 30 s of real time. Same reason `sdnotify` uses it.
use tokio::time::Instant;

/// How long a unit's byte counter may stay flat before it is judged stalled.
///
/// Sized against what each supervised lane does when healthy, so the quietest
/// of them still clears it comfortably: the MAVLink router carries a ≥1 Hz
/// heartbeat in both directions, the swarm bus beacons at 2 Hz, the CRSF lane
/// runs an RC frame train, and the vision engine consumes the camera ring. 30 s
/// is six monitor passes at the 5 s tick — long enough that a scheduling hiccup
/// or one slow pass cannot manufacture a stall.
pub const STALL_WINDOW: Duration = Duration::from_secs(30);

/// The units whose `active` state is not accepted as proof of work.
///
/// Each owns a lane whose silent death is invisible in `systemctl` and costly
/// in the air:
/// * `ados-mavlink` — the only command-and-control path to the flight
///   controller.
/// * `ados-swarmbus` — the 2 Hz state beacon onboard separation reads.
/// * `ados-crsf` — the RC control lane.
/// * `ados-vision` — the detection stream the follow / designate plugins and
///   world-model capture consume.
///
/// `ados-video`, `ados-wfb`, `ados-wfb-rx` and `ados-logd` are deliberately
/// absent: each already asserts its own byte/packet deltas one layer down in
/// its own daemon, and a second, coarser judgement here would only add a way
/// to restart a unit its own watchdog knows is healthy.
pub const WORK_PROVEN_UNITS: &[&str] =
    &["ados-mavlink", "ados-swarmbus", "ados-crsf", "ados-vision"];

/// True when `unit` must prove work rather than merely being active.
pub fn requires_work_proof(unit: &str) -> bool {
    WORK_PROVEN_UNITS.contains(&unit)
}

/// The outcome of folding one counter reading into a unit's history.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkVerdict {
    /// The counter moved, or it is flat but the window has not elapsed yet.
    Progressing,
    /// Flat for at least [`STALL_WINDOW`]. The unit is alive and idle on a lane
    /// that is never idle when healthy.
    Stalled { flat_for: Duration },
    /// No counter reading. Never acted on.
    Unknown,
}

#[derive(Debug, Clone, Copy)]
struct Sample {
    counter: u64,
    flat_since: Instant,
}

/// Per-unit byte-counter history across monitor passes.
#[derive(Debug, Default)]
pub struct WorkProof {
    last: HashMap<String, Sample>,
}

impl WorkProof {
    pub fn new() -> Self {
        Self::default()
    }

    /// Fold one counter reading for `unit` into its history and judge it.
    pub fn observe(&mut self, unit: &str, reading: Option<u64>, now: Instant) -> WorkVerdict {
        let Some(counter) = reading else {
            // A gap in the samples is not elapsed flatness.
            self.last.remove(unit);
            return WorkVerdict::Unknown;
        };
        match self.last.get_mut(unit) {
            Some(prev) if prev.counter == counter => {
                let flat_for = now.duration_since(prev.flat_since);
                if flat_for >= STALL_WINDOW {
                    WorkVerdict::Stalled { flat_for }
                } else {
                    WorkVerdict::Progressing
                }
            }
            Some(prev) => {
                // A counter that went BACKWARDS means the process was replaced
                // under the same unit name. Rebaseline rather than reading the
                // new, smaller value as a jump.
                prev.counter = counter;
                prev.flat_since = now;
                WorkVerdict::Progressing
            }
            None => {
                self.last.insert(
                    unit.to_string(),
                    Sample {
                        counter,
                        flat_since: now,
                    },
                );
                WorkVerdict::Progressing
            }
        }
    }

    /// Drop a unit's baseline. Called whenever the unit is restarted or goes
    /// inactive: the replacement process starts its counters at zero, and the
    /// stale baseline would otherwise be compared against them.
    pub fn forget(&mut self, unit: &str) {
        self.last.remove(unit);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_flat_counter_is_reported_stalled_once_the_window_elapses() {
        let mut wp = WorkProof::new();
        let t0 = Instant::now();
        // First sample only establishes the baseline.
        assert_eq!(
            wp.observe("ados-mavlink", Some(1_000), t0),
            WorkVerdict::Progressing
        );
        // Still flat, but inside the window: not yet a verdict.
        assert_eq!(
            wp.observe("ados-mavlink", Some(1_000), t0 + Duration::from_secs(20)),
            WorkVerdict::Progressing
        );
        // Flat across the whole window: the router is alive and moving nothing.
        assert_eq!(
            wp.observe("ados-mavlink", Some(1_000), t0 + STALL_WINDOW),
            WorkVerdict::Stalled {
                flat_for: STALL_WINDOW
            }
        );
    }

    #[test]
    fn a_moving_counter_restarts_the_window() {
        let mut wp = WorkProof::new();
        let t0 = Instant::now();
        wp.observe("ados-crsf", Some(10), t0);
        assert_eq!(
            wp.observe("ados-crsf", Some(11), t0 + Duration::from_secs(29)),
            WorkVerdict::Progressing
        );
        // The window runs from the move, not from the first sample.
        assert_eq!(
            wp.observe("ados-crsf", Some(11), t0 + Duration::from_secs(31)),
            WorkVerdict::Progressing
        );
        assert_eq!(
            wp.observe(
                "ados-crsf",
                Some(11),
                t0 + Duration::from_secs(29) + STALL_WINDOW
            ),
            WorkVerdict::Stalled {
                flat_for: STALL_WINDOW
            }
        );
    }

    #[test]
    fn an_unreadable_counter_never_condemns_and_does_not_accumulate() {
        let mut wp = WorkProof::new();
        let t0 = Instant::now();
        wp.observe("ados-vision", Some(5), t0);
        // A read failure mid-window.
        assert_eq!(
            wp.observe("ados-vision", None, t0 + Duration::from_secs(10)),
            WorkVerdict::Unknown
        );
        // The window restarted: the 10 s already served does not carry over,
        // so the very next reading cannot be a stall however old the process is.
        assert_eq!(
            wp.observe("ados-vision", Some(5), t0 + Duration::from_secs(11)),
            WorkVerdict::Progressing
        );
        assert_eq!(
            wp.observe("ados-vision", Some(5), t0 + Duration::from_secs(35)),
            WorkVerdict::Progressing
        );
    }

    #[test]
    fn a_counter_that_went_backwards_rebaselines_instead_of_stalling() {
        // The unit was restarted out from under us: the replacement process
        // starts at zero. Comparing against the dead process's total would
        // otherwise hold the old `flat_since` and condemn a fresh, healthy
        // process at the next window.
        let mut wp = WorkProof::new();
        let t0 = Instant::now();
        wp.observe("ados-swarmbus", Some(9_000_000), t0);
        assert_eq!(
            wp.observe("ados-swarmbus", Some(12), t0 + Duration::from_secs(5)),
            WorkVerdict::Progressing
        );
        assert_eq!(
            wp.observe("ados-swarmbus", Some(12), t0 + Duration::from_secs(20)),
            WorkVerdict::Progressing
        );
    }

    #[test]
    fn forget_clears_the_baseline_so_a_restart_starts_a_fresh_window() {
        let mut wp = WorkProof::new();
        let t0 = Instant::now();
        wp.observe("ados-mavlink", Some(1), t0);
        wp.forget("ados-mavlink");
        assert_eq!(
            wp.observe("ados-mavlink", Some(1), t0 + STALL_WINDOW * 3),
            WorkVerdict::Progressing
        );
    }

    #[test]
    fn units_are_judged_independently() {
        let mut wp = WorkProof::new();
        let t0 = Instant::now();
        wp.observe("ados-mavlink", Some(1), t0);
        wp.observe("ados-crsf", Some(1), t0);
        wp.observe("ados-crsf", Some(2), t0 + Duration::from_secs(10));
        assert_eq!(
            wp.observe("ados-mavlink", Some(1), t0 + STALL_WINDOW),
            WorkVerdict::Stalled {
                flat_for: STALL_WINDOW
            }
        );
        assert_eq!(
            wp.observe("ados-crsf", Some(3), t0 + STALL_WINDOW),
            WorkVerdict::Progressing
        );
    }

    #[test]
    fn the_work_proven_set_is_the_four_lanes_whose_silence_is_invisible() {
        assert!(requires_work_proof("ados-mavlink"));
        assert!(requires_work_proof("ados-swarmbus"));
        assert!(requires_work_proof("ados-crsf"));
        assert!(requires_work_proof("ados-vision"));
        // Units with their own in-daemon delta assertions are not double-judged.
        assert!(!requires_work_proof("ados-video"));
        assert!(!requires_work_proof("ados-wfb"));
        assert!(!requires_work_proof("ados-logd"));
    }
}
