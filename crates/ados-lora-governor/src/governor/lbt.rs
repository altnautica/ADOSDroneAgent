//! `lbt` (listen before talk): the decision over a listen interval's RSSI
//! samples. The caller samples the radio on the granted channel for at least
//! `listen_us`, transmits within `gap_max_us` of the last sample when every
//! sample is at or below the threshold, and otherwise backs off a random
//! multiple (1..=8) of the listen interval and retries, up to
//! [`MAX_ATTEMPTS`], before refusing the frame with `lbt_busy`.

use crate::regions::LbtRule;

/// Attempts per frame before `lbt_busy`.
pub const MAX_ATTEMPTS: u32 = 3;

/// Whether a listen interval found the channel clear.
pub fn clear(samples: &[i16], rule: &LbtRule) -> bool {
    !samples.is_empty() && samples.iter().all(|s| *s <= rule.threshold_dbm)
}

/// The back-off before retry `attempt` (1-based), from a per-frame random
/// value: a multiple 1..=8 of the listen interval.
pub fn backoff_us(rule: &LbtRule, random: u32) -> u64 {
    u64::from(rule.listen_us) * u64::from(1 + random % 8)
}

#[cfg(test)]
mod tests {
    use super::*;

    const JP: LbtRule = LbtRule {
        threshold_dbm: -80,
        listen_us: 5000,
        gap_max_us: 5000,
    };

    #[test]
    fn every_sample_at_or_below_the_threshold_is_clear() {
        assert!(clear(&[-112, -95, -80], &JP));
        assert!(!clear(&[-112, -70], &JP));
        assert!(!clear(&[], &JP));
    }

    #[test]
    fn backoff_is_one_to_eight_listen_intervals() {
        assert_eq!(backoff_us(&JP, 0), 5000);
        assert_eq!(backoff_us(&JP, 7), 40_000);
        assert_eq!(backoff_us(&JP, 8), 5000);
    }
}
