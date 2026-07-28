//! Beacon cadence and the transmit jitter.
//!
//! ## Why 2 Hz
//!
//! 1 Hz is the evidence-backed floor and 2 Hz the comfortable rate. Published
//! outdoor flocking results fly N=10 in 33 km/h gusts on 1 Hz telemetry with 2 Hz
//! control, and the N=30 Science Robotics result explicitly prices in outages of
//! about a second. ADS-B settled on 2 Hz for traffic moving at 250 m/s. Two drones
//! closing at 8 m/s cover 4 m between beacons, which is why the control loop runs
//! at 10 Hz against dead-reckoned positions ([`crate::NeighborTable::predicted`])
//! rather than at the beacon rate.
//!
//! ## Why the jitter
//!
//! A fleet powered up from one battery cart starts every beacon timer within
//! milliseconds of the others. Without jitter those timers stay locked together
//! and the same pairs of drones collide on the air every single period — a
//! self-inflicted, self-sustaining loss pattern. Adding a fresh uniform delay to
//! *every* transmission (not once at startup) decorrelates them permanently. This
//! is the standard ADS-B squitter trick.

use std::time::Duration;

/// Beacon transmissions per second.
pub const BEACON_HZ: f64 = 2.0;

/// The nominal beacon period, before jitter.
pub const BEACON_PERIOD: Duration = Duration::from_millis(500);

/// Maximum jitter added to each transmission, in milliseconds. 100 ms is a fifth
/// of the period: large enough to break lockstep within one cycle, small enough
/// that the effective rate stays inside 1.67–2.0 Hz.
pub const BEACON_JITTER_MS: u64 = 100;

/// The delay before the next transmission: the nominal period plus a uniform
/// `0..=BEACON_JITTER_MS`.
///
/// `random` is supplied by the caller rather than drawn here, which is what makes
/// the scheduling policy a pure function and testable off-target. The modulo bias
/// across a `u64` reduced to 101 buckets is on the order of 1e-17 and is not worth
/// a rejection loop.
pub fn beacon_delay(random: u64) -> Duration {
    BEACON_PERIOD + Duration::from_millis(random % (BEACON_JITTER_MS + 1))
}

/// Draw a random word for [`beacon_delay`], falling back to a time-derived value
/// if the OS random source fails.
///
/// The fallback is not cryptographic and does not need to be: this decorrelates
/// transmit timers, it does not protect anything. Nanosecond-resolution wall time
/// is uncorrelated enough between two independently-booted aircraft to do that job.
pub fn random_word() -> u64 {
    let mut buf = [0u8; 8];
    if getrandom::getrandom(&mut buf).is_ok() {
        return u64::from_le_bytes(buf);
    }
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_period_matches_the_declared_rate() {
        assert_eq!(BEACON_HZ, 2.0);
        assert_eq!(BEACON_PERIOD, Duration::from_secs_f64(1.0 / BEACON_HZ));
    }

    /// Every delay lands inside the window. A jitter applied with the wrong sign,
    /// or one that replaced the period instead of adding to it, escapes it.
    #[test]
    fn every_delay_is_the_period_plus_at_most_the_jitter() {
        let lo = BEACON_PERIOD;
        let hi = BEACON_PERIOD + Duration::from_millis(BEACON_JITTER_MS);
        for r in [0u64, 1, 50, 100, 101, 12345, u64::MAX, u64::MAX / 3] {
            let d = beacon_delay(r);
            assert!(d >= lo && d <= hi, "delay {d:?} outside [{lo:?}, {hi:?}]");
        }
    }

    /// The endpoints are reachable and the mapping wraps at the modulus, so the
    /// distribution really covers the whole window rather than a slice of it.
    #[test]
    fn the_jitter_window_is_fully_covered_and_inclusive_at_both_ends() {
        assert_eq!(beacon_delay(0), BEACON_PERIOD, "zero jitter is reachable");
        assert_eq!(
            beacon_delay(BEACON_JITTER_MS),
            BEACON_PERIOD + Duration::from_millis(BEACON_JITTER_MS),
            "the maximum jitter is reachable"
        );
        // 101 buckets, and the mapping wraps rather than clamping.
        assert_eq!(beacon_delay(BEACON_JITTER_MS + 1), BEACON_PERIOD);
        let buckets: std::collections::BTreeSet<Duration> =
            (0..=BEACON_JITTER_MS).map(beacon_delay).collect();
        assert_eq!(buckets.len() as u64, BEACON_JITTER_MS + 1);
    }

    /// The decorrelation property itself: a sweep of the input must actually spread
    /// the delays. A stubbed jitter (always the same value) passes the range test
    /// above but fails this, and would leave a whole fleet in lockstep.
    #[test]
    fn the_jitter_spreads_transmissions_across_the_window() {
        let delays: Vec<u64> = (0..1000)
            .map(|i| beacon_delay(i * 7919).as_millis() as u64)
            .collect();
        let distinct: std::collections::BTreeSet<u64> = delays.iter().copied().collect();
        assert!(
            distinct.len() > 80,
            "only {} distinct delays; the jitter is not spreading",
            distinct.len()
        );
        // The mean sits near the middle of the window, so the distribution is not
        // piled up at one end.
        let mean = delays.iter().sum::<u64>() as f64 / delays.len() as f64;
        let want = 500.0 + BEACON_JITTER_MS as f64 / 2.0;
        assert!((mean - want).abs() < 6.0, "mean {mean} want about {want}");
    }

    /// Two nodes drawing independently must not settle on the same offset. The
    /// live source cannot be asserted deterministically, but it must at least not
    /// be a constant.
    #[test]
    fn the_random_source_is_not_a_constant() {
        let words: std::collections::BTreeSet<u64> = (0..8).map(|_| random_word()).collect();
        assert!(
            words.len() > 1,
            "random_word returned one value eight times"
        );
    }
}
