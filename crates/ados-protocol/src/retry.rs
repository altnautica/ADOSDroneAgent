//! The one pacing primitive for a recovery loop.
//!
//! Six independent backoff implementations existed across the daemon crates
//! before this module, four of them exponential and two of those byte-identical
//! copies of each other. Each was written by someone reaching for the reflex
//! answer — double the wait, cap it, give up after N — which is correct for a
//! *client of a remote service you are one of many callers of*, and wrong for
//! every recovery loop on this vehicle. The two are not the same problem, and
//! conflating them is what this module exists to stop.
//!
//! ## Why fixed, and why forever
//!
//! Exponential backoff protects a shared remote resource from a thundering
//! herd. A drone recovering its own radio, its own camera, its own local Unix
//! socket is not part of a herd: there is one caller and the resource is on the
//! same board. So the growth buys nothing, and it costs the one thing that
//! matters — time-to-recovery at exactly the moment recovery is needed. A
//! 250 ms→5 s ladder on a *local socket* means the reader is idle for five
//! seconds after the writer comes back.
//!
//! An attempt cap or a latched failed state costs more than that. It converts a
//! recoverable fault into an unrecoverable one: the vehicle stops trying, and
//! the only way back is an operator with an SSH session — which, on a drone in
//! a field, is the same thing as a dead vehicle. Worse, the caps are typically
//! placed so the state they latch cannot clear on its own, because clearing it
//! requires the very attempts the latch stopped.
//!
//! ## Why jitter is a first-class knob and not an option
//!
//! `ados-supervisor`'s WFB bind loop is the case that proves it. The bind is a
//! two-party rendezvous: the drone's tunnel only opens while the ground
//! station's server is beaconing, each rig runs its own independent
//! attempt-then-wait loop, and the two rigs have no shared clock. With a fixed
//! interval and no jitter, two rigs that started at different times stay
//! phase-locked apart forever — both radios healthy, neither ever binding.
//! Jitter is what breaks the lock. Any primitive that treated it as optional
//! would reintroduce a defect that took a bench to find.
//!
//! ## What this is not
//!
//! Not for a bounded, session-scoped operation with a caller waiting on the
//! answer: an RPC that resends twice before reporting no-ack, a setup helper
//! inside one bind session, a socat connect budget. Those have a legitimate
//! terminal outcome, which is the honest per-request answer "it did not
//! respond" — not a latched daemon state. Bounding those is correct and they
//! deliberately do not use this.

use core::time::Duration;

/// Pacing for a recovery loop that never gives up.
///
/// Construct once, `wait()` before each attempt. There is no `next()` that
/// grows, no attempt counter, and no terminal state — that absence is the
/// contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryPace {
    base: Duration,
    jitter: Duration,
}

impl RetryPace {
    /// A fixed interval with no jitter.
    ///
    /// Correct only where nothing else on the estate is looping against the
    /// same resource on the same cadence. If two independent loops can
    /// rendezvous, use [`RetryPace::jittered`].
    pub const fn fixed(base: Duration) -> Self {
        Self {
            base,
            jitter: Duration::ZERO,
        }
    }

    /// A fixed interval plus a uniform random `0..=jitter`.
    ///
    /// The jitter breaks phase-lock between two loops that have no shared
    /// clock, which is what the WFB bind rendezvous needs.
    pub const fn jittered(base: Duration, jitter: Duration) -> Self {
        Self { base, jitter }
    }

    /// The floor of the interval.
    pub const fn base(&self) -> Duration {
        self.base
    }

    /// The width of the jitter window.
    pub const fn jitter(&self) -> Duration {
        self.jitter
    }

    /// The wait before the next attempt.
    ///
    /// Deliberately takes no attempt index: an interval that could vary by
    /// attempt is an interval that can grow, and the whole point is that this
    /// one cannot. Compute the jitter from `rand` in `0..=jitter`; a failure to
    /// read the OS entropy source degrades to zero jitter rather than
    /// propagating, because a loop that stops pacing is worse than a loop that
    /// briefly loses its phase-lock protection.
    pub fn wait(&self) -> Duration {
        if self.jitter.is_zero() {
            return self.base;
        }
        let span = self.jitter.as_millis().saturating_add(1);
        let mut byte = [0u8; 8];
        let offset_ms = match getrandom::getrandom(&mut byte) {
            Ok(()) => (u64::from_le_bytes(byte) as u128 % span) as u64,
            Err(_) => 0,
        };
        self.base + Duration::from_millis(offset_ms)
    }

    /// The widest wait this pace can produce, for a test or a doc.
    pub fn max_wait(&self) -> Duration {
        self.base + self.jitter
    }
}

/// The pacing band a recovery loop is expected to sit in.
///
/// Not enforced by the type — a USB rebind and a Unix-socket reconnect
/// genuinely want different magnitudes, and a type that forbade that would be
/// obeyed by someone adding a growth ladder somewhere the linter cannot see.
/// It is here so the workspace test in `tests/recovery_loops_never_give_up.rs`
/// can name the band, and so a site outside it is a deliberate, commented
/// decision rather than a default.
pub const RECOVERY_BAND: (Duration, Duration) =
    (Duration::from_millis(250), Duration::from_secs(5));

/// A local Unix-socket reader reconnecting to a peer daemon on the same board.
///
/// No jitter: there is exactly one reader per socket, so there is no herd and
/// nothing to de-phase. Short, because the socket reappearing is the common
/// case (the peer restarted) and the cost of the wait is stale telemetry on an
/// operator's screen.
pub const LOCAL_SOCKET: RetryPace = RetryPace::fixed(Duration::from_millis(500));

/// A two-party rendezvous where both sides loop independently with no shared
/// clock. Short base, wide jitter — see the module docs.
pub const RENDEZVOUS: RetryPace =
    RetryPace::jittered(Duration::from_secs(3), Duration::from_secs(4));

/// Re-driving a piece of OS or hardware state that refused once: a regulatory
/// domain that lost a `cfg80211` race, an interface that was not up yet.
pub const OS_STATE: RetryPace = RetryPace::fixed(Duration::from_secs(2));

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fixed_pace_does_not_move() {
        // The property the exponential ladders violated: the interval after
        // the hundredth failure is the interval after the first.
        let pace = RetryPace::fixed(Duration::from_secs(3));
        let waits: Vec<Duration> = (0..100).map(|_| pace.wait()).collect();
        assert!(
            waits.iter().all(|w| *w == Duration::from_secs(3)),
            "a fixed pace grew"
        );
    }

    #[test]
    fn a_jittered_pace_stays_inside_its_band_and_actually_varies() {
        let pace = RetryPace::jittered(Duration::from_secs(3), Duration::from_secs(4));
        let waits: Vec<Duration> = (0..200).map(|_| pace.wait()).collect();
        assert!(
            waits
                .iter()
                .all(|w| *w >= pace.base() && *w <= pace.max_wait()),
            "jitter escaped its band"
        );
        // If the jitter were a no-op the phase-lock defect would be back with
        // the tests still green, so assert it is really random.
        let distinct: std::collections::BTreeSet<u128> =
            waits.iter().map(|w| w.as_millis()).collect();
        assert!(
            distinct.len() > 10,
            "jitter produced only {} distinct waits in 200 draws",
            distinct.len()
        );
    }

    #[test]
    fn zero_jitter_is_exactly_the_base() {
        let pace = RetryPace::jittered(Duration::from_secs(2), Duration::ZERO);
        assert_eq!(pace.wait(), Duration::from_secs(2));
    }

    #[test]
    fn the_shipped_paces_sit_in_the_recovery_band() {
        let (lo, hi) = RECOVERY_BAND;
        for (name, pace) in [
            ("LOCAL_SOCKET", LOCAL_SOCKET),
            ("RENDEZVOUS", RENDEZVOUS),
            ("OS_STATE", OS_STATE),
        ] {
            assert!(
                pace.base() >= lo && pace.base() <= hi,
                "{name} base {:?} is outside the recovery band {lo:?}..={hi:?}",
                pace.base()
            );
        }
    }

    #[test]
    fn the_rendezvous_pace_keeps_the_jitter_the_bench_needed() {
        // Two rigs looping with identical timing must be able to de-phase, so
        // the jitter window has to be a real fraction of the base rather than a
        // token millisecond.
        assert!(RENDEZVOUS.jitter() >= RENDEZVOUS.base());
    }
}
