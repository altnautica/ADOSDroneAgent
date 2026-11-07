//! Pilot failsafes (C2): controller staleness, surface link loss and the
//! heartbeat cadence the FC's GCS failsafe watches.

/// No `input` for this long while a session is live: brake and hold.
pub const INPUT_STALE_MS: u64 = 300;
/// The pilot heartbeat period (2 Hz).
pub const HEARTBEAT_PERIOD_MS: u64 = 500;
/// The link-loss countdown is shown once this much of the window has passed.
pub const LINK_COUNTDOWN_SHOW_MS: u64 = 2000;

#[derive(Debug, Clone, Default)]
pub struct Failsafe {
    last_input_ms: Option<u64>,
    last_surface_ms: Option<u64>,
    last_heartbeat_ms: Option<u64>,
    /// Set once the link-loss action fired; cleared by the next surface frame.
    pub link_loss_fired: bool,
}

impl Failsafe {
    pub fn on_input(&mut self, now_ms: u64) {
        self.last_input_ms = Some(now_ms);
        self.on_surface(now_ms);
    }

    /// Any frame from an operator surface (input, hb, action, session ops).
    pub fn on_surface(&mut self, now_ms: u64) {
        self.last_surface_ms = Some(now_ms);
        self.link_loss_fired = false;
    }

    /// Forget the surface link (on disarm), so the next flight only arms the
    /// link-loss timer after a surface has spoken.
    pub fn reset_link(&mut self) {
        self.last_surface_ms = None;
        self.last_input_ms = None;
        self.link_loss_fired = false;
    }

    pub fn input_age_ms(&self, now_ms: u64) -> Option<u64> {
        self.last_input_ms.map(|t| now_ms.saturating_sub(t))
    }

    pub fn input_stale(&self, now_ms: u64) -> bool {
        self.input_age_ms(now_ms)
            .is_some_and(|a| a > INPUT_STALE_MS)
    }

    /// Milliseconds since the last surface frame (None: no surface yet).
    pub fn surface_silence_ms(&self, now_ms: u64) -> Option<u64> {
        self.last_surface_ms.map(|t| now_ms.saturating_sub(t))
    }

    /// True once per window: the surface link has been silent for `window_ms`.
    pub fn link_lost(&mut self, now_ms: u64, window_ms: u64) -> bool {
        if self.link_loss_fired {
            return false;
        }
        if self
            .surface_silence_ms(now_ms)
            .is_some_and(|s| s >= window_ms)
        {
            self.link_loss_fired = true;
            return true;
        }
        false
    }

    /// Seconds left before the link-loss action, once the countdown shows.
    pub fn link_countdown_s(&self, now_ms: u64, window_ms: u64) -> Option<f32> {
        if self.link_loss_fired {
            return None;
        }
        let s = self.surface_silence_ms(now_ms)?;
        (s >= LINK_COUNTDOWN_SHOW_MS && s < window_ms).then(|| (window_ms - s) as f32 / 1000.0)
    }

    pub fn heartbeat_due(&mut self, now_ms: u64) -> bool {
        let due = self
            .last_heartbeat_ms
            .is_none_or(|t| now_ms.saturating_sub(t) >= HEARTBEAT_PERIOD_MS);
        if due {
            self.last_heartbeat_ms = Some(now_ms);
        }
        due
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn input_goes_stale_after_300_ms() {
        let mut f = Failsafe::default();
        f.on_input(1000);
        assert!(!f.input_stale(1300));
        assert!(f.input_stale(1301));
    }

    #[test]
    fn link_loss_fires_once_after_the_window() {
        let mut f = Failsafe::default();
        assert!(!f.link_lost(50_000, 10_000), "no surface yet: never fires");
        f.on_surface(0);
        assert_eq!(f.link_countdown_s(1000, 10_000), None);
        assert_eq!(f.link_countdown_s(4000, 10_000), Some(6.0));
        assert!(!f.link_lost(9999, 10_000));
        assert!(f.link_lost(10_000, 10_000));
        assert!(!f.link_lost(11_000, 10_000));
        f.on_surface(12_000);
        assert!(!f.link_loss_fired);
    }

    #[test]
    fn heartbeat_is_2_hz() {
        let mut f = Failsafe::default();
        assert!(f.heartbeat_due(0));
        assert!(!f.heartbeat_due(499));
        assert!(f.heartbeat_due(500));
    }
}
