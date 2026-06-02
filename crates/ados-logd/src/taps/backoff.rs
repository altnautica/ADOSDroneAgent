//! Capped exponential backoff for the reconnect loops.
//!
//! Each tap connects to a socket that is routinely absent (no agent on a host,
//! an idle agent before a seam comes up). The backoff keeps a failing reconnect
//! from hot-spinning: the delay doubles from a small base up to a ceiling, and
//! resets to the base the moment a connection succeeds.

use std::time::Duration;

/// First reconnect delay.
pub const BASE_DELAY: Duration = Duration::from_millis(500);

/// Ceiling the delay grows to.
pub const MAX_DELAY: Duration = Duration::from_secs(10);

/// A capped exponential backoff. [`next_delay`](ReconnectBackoff::next_delay)
/// returns the current delay and doubles it for next time (capped);
/// [`reset`](ReconnectBackoff::reset) returns to the base after a success.
#[derive(Debug, Clone)]
pub struct ReconnectBackoff {
    current: Duration,
}

impl Default for ReconnectBackoff {
    fn default() -> Self {
        Self {
            current: BASE_DELAY,
        }
    }
}

impl ReconnectBackoff {
    /// The delay to wait before the next reconnect attempt, doubling the stored
    /// value (capped at [`MAX_DELAY`]) for the attempt after this one.
    pub fn next_delay(&mut self) -> Duration {
        let delay = self.current;
        self.current = (self.current * 2).min(MAX_DELAY);
        delay
    }

    /// Reset the delay to the base after a successful connection.
    pub fn reset(&mut self) {
        self.current = BASE_DELAY;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delay_doubles_then_caps_and_resets() {
        let mut b = ReconnectBackoff::default();
        assert_eq!(b.next_delay(), BASE_DELAY);
        assert_eq!(b.next_delay(), BASE_DELAY * 2);
        assert_eq!(b.next_delay(), BASE_DELAY * 4);
        // Drive it well past the ceiling; it never exceeds MAX_DELAY.
        for _ in 0..20 {
            assert!(b.next_delay() <= MAX_DELAY);
        }
        assert_eq!(b.next_delay(), MAX_DELAY);
        // A success resets to the base.
        b.reset();
        assert_eq!(b.next_delay(), BASE_DELAY);
    }
}
