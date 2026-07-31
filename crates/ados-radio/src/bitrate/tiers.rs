//! The bitrate/FEC rung ladder and its loss+RSSI hysteresis.
//!
//! Pure: no I/O, no clock reads (`now` is injected), so the whole decision is
//! unit-testable. [`super::controller`] owns the sampling loop and the actuation.

use std::time::Instant;

use super::{
    STEP_DOWN_COOLDOWN, STEP_DOWN_LOSS_PCT, STEP_DOWN_REQUIRED_BAD_SAMPLES, STEP_DOWN_RSSI_DBM,
    STEP_UP_COOLDOWN, STEP_UP_LOSS_PCT, STEP_UP_REQUIRED_CLEAN_SAMPLES, STEP_UP_RSSI_DBM,
};

/// One rung of the bitrate / FEC ladder. `fec_k`/`fec_n` drive the `wfb_tx`
/// Reed-Solomon configuration; `bitrate_kbps` is applied to the video encoder as
/// a ceiling (see [`BitrateController`](super::BitrateController)), so a degrading link emits FEWER on-air
/// bytes rather than more.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BitrateTier {
    pub name: &'static str,
    pub bitrate_kbps: u32,
    pub fec_k: u8,
    pub fec_n: u8,
}

/// The default ladder. Rung 0 is the high-quality default the controller climbs
/// back to; the last rung is the rescue rung (200% FEC) for a very degraded
/// link. Byte-identical to the Python `DEFAULT_TIERS`.
pub const DEFAULT_TIERS: [BitrateTier; 4] = [
    BitrateTier {
        name: "high",
        bitrate_kbps: 4000,
        fec_k: 8,
        fec_n: 12,
    },
    BitrateTier {
        name: "medium",
        bitrate_kbps: 3000,
        fec_k: 8,
        fec_n: 14,
    },
    BitrateTier {
        name: "low",
        bitrate_kbps: 2000,
        fec_k: 8,
        fec_n: 16,
    },
    BitrateTier {
        name: "rescue",
        bitrate_kbps: 1200,
        fec_k: 4,
        fec_n: 12,
    },
];

/// The action a single hysteresis tick decides on, given the current rung.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TierAction {
    /// Hold the current rung.
    Hold,
    /// Move down one rung (toward the rescue rung) — the link is degrading.
    StepDown,
    /// Move up one rung (toward the high rung) — the link has recovered.
    StepUp,
}

/// The hysteresis state that survives across ticks. Separated from the I/O so it
/// is pure + unit-testable: the streaks, the cooldown instants, and the current
/// rung index. `decide` is the whole decision; the run loop only does I/O.
#[derive(Debug, Clone)]
pub struct Hysteresis {
    tier_count: usize,
    current_tier_idx: usize,
    bad_streak: u32,
    clean_streak: u32,
    last_down_at: Option<Instant>,
    last_up_at: Option<Instant>,
    last_action_reason: String,
}

impl Hysteresis {
    /// Fresh state starting at `starting_tier_idx` of a `tier_count`-rung ladder.
    pub fn new(tier_count: usize, starting_tier_idx: usize) -> Self {
        debug_assert!(tier_count > 0, "tier ladder must have at least one rung");
        Self {
            tier_count,
            current_tier_idx: starting_tier_idx.min(tier_count.saturating_sub(1)),
            bad_streak: 0,
            clean_streak: 0,
            last_down_at: None,
            last_up_at: None,
            last_action_reason: "initial".to_string(),
        }
    }

    /// The rung the ladder is currently on.
    pub fn current_tier_idx(&self) -> usize {
        self.current_tier_idx
    }

    pub fn bad_streak(&self) -> u32 {
        self.bad_streak
    }

    pub fn clean_streak(&self) -> u32 {
        self.clean_streak
    }

    pub fn last_action_reason(&self) -> &str {
        &self.last_action_reason
    }

    /// Fold one link sample into the hysteresis state and return the action to
    /// take. `now` is supplied so the cooldowns are testable without sleeping.
    ///
    /// A bad sample grows the bad streak (and zeroes the clean streak); once the
    /// streak reaches the required count, there is a lower rung to move to, and
    /// the step-down cooldown has elapsed, it returns `StepDown`. The clean path
    /// is the mirror image with the step-up thresholds/cooldown. An intermediate
    /// sample decays both streaks by one and holds.
    pub fn decide(&mut self, loss_percent: f64, rssi_dbm: f64, now: Instant) -> TierAction {
        let bad = loss_percent > STEP_DOWN_LOSS_PCT || rssi_dbm < STEP_DOWN_RSSI_DBM;
        let clean = loss_percent < STEP_UP_LOSS_PCT && rssi_dbm > STEP_UP_RSSI_DBM;

        if bad {
            self.bad_streak += 1;
            self.clean_streak = 0;
            let cooldown_ok = self
                .last_down_at
                .is_none_or(|t| now.duration_since(t) >= STEP_DOWN_COOLDOWN);
            if self.bad_streak >= STEP_DOWN_REQUIRED_BAD_SAMPLES
                && self.current_tier_idx + 1 < self.tier_count
                && cooldown_ok
            {
                self.current_tier_idx += 1;
                self.last_down_at = Some(now);
                self.bad_streak = 0;
                self.last_action_reason = format!("loss={loss_percent:.1}_rssi={rssi_dbm:.0}");
                return TierAction::StepDown;
            }
            return TierAction::Hold;
        }

        if clean {
            self.clean_streak += 1;
            self.bad_streak = 0;
            let cooldown_ok = self
                .last_up_at
                .is_none_or(|t| now.duration_since(t) >= STEP_UP_COOLDOWN);
            if self.clean_streak >= STEP_UP_REQUIRED_CLEAN_SAMPLES
                && self.current_tier_idx > 0
                && cooldown_ok
            {
                self.current_tier_idx -= 1;
                self.last_up_at = Some(now);
                self.clean_streak = 0;
                self.last_action_reason =
                    format!("clean_loss={loss_percent:.1}_rssi={rssi_dbm:.0}");
                return TierAction::StepUp;
            }
            return TierAction::Hold;
        }

        // Intermediate: decay both streaks so a marginal period triggers nothing.
        self.bad_streak = self.bad_streak.saturating_sub(1);
        self.clean_streak = self.clean_streak.saturating_sub(1);
        TierAction::Hold
    }

    /// Fold one locally-measured congestion observation through the same
    /// machinery as [`decide`](Self::decide).
    ///
    /// This exists for the case where there is no link sample at all. A drone
    /// transmits its downlink and cannot hear it, so it has no loss or RSSI to
    /// fold in, and `decide` never runs — which left the ladder pinned to its
    /// starting rung with no way down even while the encoder was plainly
    /// over-feeding the link.
    ///
    /// Congestion is measurable without hearing anything: the transmit queue
    /// backs up while the radio is still draining it, which says directly that
    /// more is being offered than the air is carrying. `congested` is that
    /// observation, and a clear queue is the honest inverse — the link is
    /// comfortably carrying the current rate, so a higher rung is worth trying.
    ///
    /// Shares the streaks, cooldowns and rung bounds with `decide` so the two
    /// can never race each other along the ladder, and so a step up still costs
    /// a long clear run while a step down is quick. Together with the step-up
    /// path this closes the loop: it settles on the highest rung the link
    /// actually sustains, rather than ratcheting to the floor and staying there.
    pub fn decide_congestion(&mut self, congested: bool, now: Instant) -> TierAction {
        if congested {
            self.bad_streak += 1;
            self.clean_streak = 0;
            let cooldown_ok = self
                .last_down_at
                .is_none_or(|t| now.duration_since(t) >= STEP_DOWN_COOLDOWN);
            if self.bad_streak >= STEP_DOWN_REQUIRED_BAD_SAMPLES
                && self.current_tier_idx + 1 < self.tier_count
                && cooldown_ok
            {
                self.current_tier_idx += 1;
                self.last_down_at = Some(now);
                self.bad_streak = 0;
                self.last_action_reason = "tx_queue_congested".to_string();
                return TierAction::StepDown;
            }
            return TierAction::Hold;
        }

        self.clean_streak += 1;
        self.bad_streak = 0;
        let cooldown_ok = self
            .last_up_at
            .is_none_or(|t| now.duration_since(t) >= STEP_UP_COOLDOWN);
        if self.clean_streak >= STEP_UP_REQUIRED_CLEAN_SAMPLES
            && self.current_tier_idx > 0
            && cooldown_ok
        {
            self.current_tier_idx -= 1;
            self.last_up_at = Some(now);
            self.clean_streak = 0;
            self.last_action_reason = "tx_queue_clear".to_string();
            return TierAction::StepUp;
        }
        TierAction::Hold
    }
}

#[cfg(test)]
mod congestion_tests {
    use super::*;

    /// The drone case: no link sample ever arrives, so `decide` never runs and
    /// the ladder used to sit on its starting rung for the whole flight. A
    /// sustained backed-up transmit queue must be able to shed rate on its own.
    #[test]
    fn sustained_congestion_steps_down_without_any_link_sample() {
        let mut h = Hysteresis::new(DEFAULT_TIERS.len(), 0);
        let t0 = Instant::now();

        // Below the required streak nothing moves.
        for _ in 0..STEP_DOWN_REQUIRED_BAD_SAMPLES - 1 {
            assert_eq!(h.decide_congestion(true, t0), TierAction::Hold);
        }
        assert_eq!(h.current_tier_idx(), 0);

        // The streak completes and the ladder sheds a rung.
        assert_eq!(h.decide_congestion(true, t0), TierAction::StepDown);
        assert_eq!(h.current_tier_idx(), 1);
        assert_eq!(h.last_action_reason(), "tx_queue_congested");
    }

    /// A clear queue is the honest inverse and must be able to recover the rung,
    /// otherwise a brief burst of congestion would ratchet the drone to the
    /// floor for the rest of the flight.
    #[test]
    fn a_clear_queue_recovers_the_rung_so_it_does_not_ratchet_down() {
        let mut h = Hysteresis::new(DEFAULT_TIERS.len(), 1);
        let t0 = Instant::now();
        let later = t0 + STEP_UP_COOLDOWN;

        for _ in 0..STEP_UP_REQUIRED_CLEAN_SAMPLES - 1 {
            assert_eq!(h.decide_congestion(false, later), TierAction::Hold);
        }
        assert_eq!(h.decide_congestion(false, later), TierAction::StepUp);
        assert_eq!(h.current_tier_idx(), 0);
        assert_eq!(h.last_action_reason(), "tx_queue_clear");
    }

    /// Recovery must cost far more evidence than shedding does, or the loop
    /// oscillates across rungs instead of settling. Checked at compile time so
    /// retuning the constants cannot quietly invert it.
    const _: () = assert!(STEP_UP_REQUIRED_CLEAN_SAMPLES > STEP_DOWN_REQUIRED_BAD_SAMPLES);

    /// A single clear sample must not wipe out an in-progress bad streak, and
    /// vice versa — the streaks are shared with `decide` for exactly this.
    #[test]
    fn an_interleaved_sample_resets_the_opposing_streak() {
        let mut h = Hysteresis::new(DEFAULT_TIERS.len(), 0);
        let t0 = Instant::now();

        for _ in 0..STEP_DOWN_REQUIRED_BAD_SAMPLES - 1 {
            h.decide_congestion(true, t0);
        }
        assert!(h.bad_streak() > 0);
        h.decide_congestion(false, t0);
        assert_eq!(h.bad_streak(), 0, "a clear queue clears the bad streak");
        // And the rung did not move on that single clear sample.
        assert_eq!(h.current_tier_idx(), 0);
    }

    /// Congestion at the bottom rung has nowhere to go and must simply hold,
    /// not error or wrap.
    #[test]
    fn congestion_at_the_floor_holds() {
        let last = DEFAULT_TIERS.len() - 1;
        let mut h = Hysteresis::new(DEFAULT_TIERS.len(), last);
        let t0 = Instant::now();
        for _ in 0..STEP_DOWN_REQUIRED_BAD_SAMPLES * 3 {
            assert_eq!(h.decide_congestion(true, t0), TierAction::Hold);
        }
        assert_eq!(h.current_tier_idx(), last);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// Drive `count` samples through the hysteresis at a fixed instant and return
    /// the last action. The instant is held constant so cooldowns do NOT elapse
    /// between samples (the streak alone must drive the first step).
    fn feed_n(h: &mut Hysteresis, loss: f64, rssi: f64, count: u32, at: Instant) -> TierAction {
        let mut last = TierAction::Hold;
        for _ in 0..count {
            last = h.decide(loss, rssi, at);
        }
        last
    }

    /// Drive `count` samples and report whether ANY of them produced `want`.
    /// A step fires on the first qualifying sample (resetting the streak), so a
    /// batch's LAST action is `Hold` even when a step happened mid-batch — this
    /// captures the step regardless of where in the batch it landed.
    fn batch_contains(
        h: &mut Hysteresis,
        loss: f64,
        rssi: f64,
        count: u32,
        at: Instant,
        want: TierAction,
    ) -> bool {
        let mut hit = false;
        for _ in 0..count {
            if h.decide(loss, rssi, at) == want {
                hit = true;
            }
        }
        hit
    }

    #[test]
    fn default_ladder_matches_python() {
        assert_eq!(DEFAULT_TIERS.len(), 4);
        assert_eq!(DEFAULT_TIERS[0].name, "high");
        assert_eq!(
            (
                DEFAULT_TIERS[0].bitrate_kbps,
                DEFAULT_TIERS[0].fec_k,
                DEFAULT_TIERS[0].fec_n
            ),
            (4000, 8, 12)
        );
        assert_eq!(
            (
                DEFAULT_TIERS[1].bitrate_kbps,
                DEFAULT_TIERS[1].fec_k,
                DEFAULT_TIERS[1].fec_n
            ),
            (3000, 8, 14)
        );
        assert_eq!(
            (
                DEFAULT_TIERS[2].bitrate_kbps,
                DEFAULT_TIERS[2].fec_k,
                DEFAULT_TIERS[2].fec_n
            ),
            (2000, 8, 16)
        );
        assert_eq!(
            (
                DEFAULT_TIERS[3].bitrate_kbps,
                DEFAULT_TIERS[3].fec_k,
                DEFAULT_TIERS[3].fec_n
            ),
            (1200, 4, 12)
        );
    }

    /// The ladder descends in bitrate as it descends in rung — the property the
    /// encoder ceiling relies on. A rung that raised the encoder bitrate while
    /// also raising FEC would put MORE bytes on a degrading channel, which is the
    /// exact bug the closed loop exists to fix.
    #[test]
    fn ladder_bitrate_falls_monotonically_as_fec_grows() {
        for w in DEFAULT_TIERS.windows(2) {
            assert!(
                w[1].bitrate_kbps < w[0].bitrate_kbps,
                "{} -> {} did not reduce bitrate",
                w[0].name,
                w[1].name
            );
            let before = w[0].fec_n as f64 / w[0].fec_k as f64;
            let after = w[1].fec_n as f64 / w[1].fec_k as f64;
            assert!(
                after > before,
                "{} -> {} did not raise redundancy",
                w[0].name,
                w[1].name
            );
        }
    }

    #[test]
    fn one_bad_sample_holds_below_streak() {
        let mut h = Hysteresis::new(4, 0);
        let now = Instant::now();
        // A single bad sample is not enough; the streak must reach 5.
        assert_eq!(h.decide(10.0, -80.0, now), TierAction::Hold);
        assert_eq!(h.current_tier_idx(), 0);
        assert_eq!(h.bad_streak(), 1);
    }

    #[test]
    fn sustained_loss_steps_down_after_five_samples() {
        let mut h = Hysteresis::new(4, 0);
        let now = Instant::now();
        // High loss for 5 consecutive samples trips the step-down on the 5th.
        assert_eq!(h.decide(10.0, -50.0, now), TierAction::Hold); // 1
        assert_eq!(h.decide(10.0, -50.0, now), TierAction::Hold); // 2
        assert_eq!(h.decide(10.0, -50.0, now), TierAction::Hold); // 3
        assert_eq!(h.decide(10.0, -50.0, now), TierAction::Hold); // 4
        assert_eq!(h.decide(10.0, -50.0, now), TierAction::StepDown); // 5
        assert_eq!(h.current_tier_idx(), 1);
        // The streak resets after the step.
        assert_eq!(h.bad_streak(), 0);
    }

    #[test]
    fn weak_rssi_alone_trips_step_down() {
        let mut h = Hysteresis::new(4, 0);
        let now = Instant::now();
        // Loss is fine but RSSI below -75 is "bad" on its own.
        let action = feed_n(&mut h, 0.0, -80.0, 5, now);
        assert_eq!(action, TierAction::StepDown);
        assert_eq!(h.current_tier_idx(), 1);
    }

    #[test]
    fn step_down_respects_cooldown() {
        let mut h = Hysteresis::new(4, 0);
        let t0 = Instant::now();
        // First step down at t0 (the 5th bad sample trips it).
        assert!(batch_contains(
            &mut h,
            10.0,
            -50.0,
            5,
            t0,
            TierAction::StepDown
        ));
        assert_eq!(h.current_tier_idx(), 1);
        // A second sustained bad window 1 s later (< 5 s cooldown): the streak
        // reaches the count again but the cooldown blocks the step, so the rung
        // does not move.
        let t1 = t0 + Duration::from_secs(1);
        assert!(!batch_contains(
            &mut h,
            10.0,
            -50.0,
            5,
            t1,
            TierAction::StepDown
        ));
        assert_eq!(h.current_tier_idx(), 1);
        // After the 5 s cooldown elapses the next bad sample is allowed to step
        // (the streak is already past the count from the blocked window).
        let t2 = t0 + Duration::from_secs(6);
        assert!(batch_contains(
            &mut h,
            10.0,
            -50.0,
            1,
            t2,
            TierAction::StepDown
        ));
        assert_eq!(h.current_tier_idx(), 2);
    }

    #[test]
    fn clean_window_steps_up_after_thirty_samples() {
        let mut h = Hysteresis::new(4, 1);
        let now = Instant::now();
        // 29 clean samples hold; the 30th steps up.
        assert_eq!(feed_n(&mut h, 0.0, -50.0, 29, now), TierAction::Hold);
        assert_eq!(h.current_tier_idx(), 1);
        assert_eq!(h.decide(0.0, -50.0, now), TierAction::StepUp);
        assert_eq!(h.current_tier_idx(), 0);
    }

    #[test]
    fn step_up_blocked_at_top_rung() {
        let mut h = Hysteresis::new(4, 0);
        let now = Instant::now();
        // Already at rung 0 (high); a clean window cannot climb higher.
        assert_eq!(feed_n(&mut h, 0.0, -50.0, 40, now), TierAction::Hold);
        assert_eq!(h.current_tier_idx(), 0);
    }

    #[test]
    fn step_down_blocked_at_bottom_rung() {
        let mut h = Hysteresis::new(4, 3);
        let now = Instant::now();
        // Already at the rescue rung; a bad window cannot drop further.
        assert_eq!(feed_n(&mut h, 50.0, -90.0, 10, now), TierAction::Hold);
        assert_eq!(h.current_tier_idx(), 3);
    }

    #[test]
    fn intermediate_sample_decays_streaks() {
        let mut h = Hysteresis::new(4, 0);
        let now = Instant::now();
        // Build a partial bad streak.
        h.decide(10.0, -50.0, now);
        h.decide(10.0, -50.0, now);
        assert_eq!(h.bad_streak(), 2);
        // An intermediate sample (between clean and bad: loss 3% with strong
        // rssi is neither > 5% loss / < -75 dBm nor < 1% loss) decays it.
        assert_eq!(h.decide(3.0, -50.0, now), TierAction::Hold);
        assert_eq!(h.bad_streak(), 1);
        assert_eq!(h.decide(3.0, -50.0, now), TierAction::Hold);
        assert_eq!(h.bad_streak(), 0);
    }

    #[test]
    fn bad_sample_zeroes_clean_streak() {
        let mut h = Hysteresis::new(4, 1);
        let now = Instant::now();
        // Build a clean streak, then a bad sample wipes it.
        feed_n(&mut h, 0.0, -50.0, 10, now);
        assert_eq!(h.clean_streak(), 10);
        h.decide(10.0, -50.0, now);
        assert_eq!(h.clean_streak(), 0);
        assert_eq!(h.bad_streak(), 1);
    }
}
