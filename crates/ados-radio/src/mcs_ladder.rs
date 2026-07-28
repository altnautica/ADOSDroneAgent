//! SNR-tracking MCS ladder.
//!
//! The data plane shipped with a **static** MCS 1 — QPSK 1/2, 13.0 Mbps PHY,
//! ~5 dB required SNR. That is the second-lowest rung of eight: maximum
//! sensitivity, minimum throughput, held against a bench-measured **35 dB**.
//! One hero video stream at MCS 1 costs 48% airtime, so a static rung is what
//! makes a 24-drone fleet not fit on one channel.
//!
//! This module is the pure decision half: given a measured SNR it picks the
//! highest rung whose required SNR is met with margin, and gates the move behind
//! an asymmetric streak. The I/O half (issuing `set_radio` over the `wfb_tx`
//! management socket, with a respawn fallback) lives in [`crate::process`], and
//! the 1 Hz sampling loop that drives it is [`crate::bitrate::BitrateController`].
//!
//! # Rung table
//!
//! Required-SNR figures are the standard 802.11n 20 MHz values; the trip
//! thresholds hold roughly 10 dB back from them, so a rung is only selected once
//! the link has real margin above the point it would start erroring:
//!
//! | rung  | PHY        | needs | trips at | held margin |
//! |-------|------------|-------|----------|-------------|
//! | MCS 1 | 13.0 Mbps  |  5 dB | floor    | —           |
//! | MCS 2 | 19.5 Mbps  |  9 dB | 15 dB    | 6 dB        |
//! | MCS 3 | 26.0 Mbps  | 11 dB | 21 dB    | 10 dB       |
//! | MCS 4 | 39.0 Mbps  | 15 dB | 24 dB    | 9 dB        |
//! | MCS 5 | 52.0 Mbps  | 18 dB | 30 dB    | 12 dB       |
//!
//! MCS 1 is the floor, not MCS 0: MCS 0 halves throughput for ~2 dB of extra
//! sensitivity, and the link is already at its FEC rescue rung by then.
//!
//! # Hysteresis
//!
//! Deliberately asymmetric, because the two directions are not symmetric risks:
//! losing range is instant and dangerous, gaining it is not urgent.
//!
//! - **Down** after [`MCS_STEP_DOWN_REQUIRED_BAD_SAMPLES`] (2) consecutive
//!   samples whose SNR no longer supports the live rung, and it drops **straight
//!   to the supported rung** rather than one step at a time — an intermediate
//!   rung the SNR does not support is not a safe place to spend 5 s.
//! - **Up** only after `bitrate::STEP_UP_REQUIRED_CLEAN_SAMPLES` (30)
//!   consecutive samples supporting a higher rung, and then by exactly **one**
//!   rung, so a climb probes each rung for 30 s instead of leaping.
//! - Cooldowns are the tier ladder's own (`bitrate::STEP_DOWN_COOLDOWN` 5 s /
//!   `bitrate::STEP_UP_COOLDOWN` 30 s).
//! - A sample that supports exactly the live rung decays both streaks, so a
//!   marginal period straddling a threshold triggers nothing.

use std::time::Instant;

use crate::bitrate::{STEP_DOWN_COOLDOWN, STEP_UP_COOLDOWN, STEP_UP_REQUIRED_CLEAN_SAMPLES};

/// Consecutive SNR samples below the live rung's threshold before stepping down.
/// Two, against thirty for the climb — see the module hysteresis note.
pub const MCS_STEP_DOWN_REQUIRED_BAD_SAMPLES: u32 = 2;

/// The lowest rung the ladder will select.
pub const MCS_FLOOR: u8 = 1;

/// The highest rung the ladder will select regardless of configuration or SNR.
/// MCS 6/7 are 64-QAM 3/4 and 5/6: they need 21+ dB and buy little over MCS 5 on
/// a 20 MHz channel, and nothing in this tree has measured them on the RTL8812EU.
pub const LADDER_MAX_MCS: u8 = 5;

/// Default cap on the ladder's top rung.
///
/// Three, not five, on purpose. OpenIPC's `adaptive-link` — the working
/// production reference for this exact loop — ships a profile table that **tops
/// out at MCS 2 in the field**. The rung table in this module is arithmetic from
/// standard 802.11n required-SNR figures, not a measurement of the vendored
/// `rtl8812eu` driver: nothing here has verified that the driver actually applies
/// MCS 4 or 5, or that it holds them under load. Anything above 3 is bench-only
/// until the sweep in `product/specs/ados-agent-rust-hybrid/wfb-video-pipeline-runbook.md`
/// ("MCS ladder characterisation sweep") measures this driver at three ranges and
/// says otherwise.
pub const DEFAULT_LADDER_MAX_MCS: u8 = 3;

/// One rung of the SNR ladder.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct McsRung {
    pub mcs_index: u8,
    /// Measured SNR at or above which this rung is selected.
    pub min_snr_db: f64,
    /// Standard 802.11n 20 MHz PHY rate, long GI.
    pub phy_mbps: f64,
    /// Standard 802.11n 20 MHz required SNR — the point this rung starts erroring.
    pub required_snr_db: f64,
}

/// The ladder, ascending. `min_snr_db` on the floor rung is negative infinity:
/// there is nothing below MCS 1 to fall to, so it accepts any measurement
/// including the pre-measurement 0.0 sentinel.
pub const MCS_LADDER: [McsRung; 5] = [
    McsRung {
        mcs_index: 1,
        min_snr_db: f64::NEG_INFINITY,
        phy_mbps: 13.0,
        required_snr_db: 5.0,
    },
    McsRung {
        mcs_index: 2,
        min_snr_db: 15.0,
        phy_mbps: 19.5,
        required_snr_db: 9.0,
    },
    McsRung {
        mcs_index: 3,
        min_snr_db: 21.0,
        phy_mbps: 26.0,
        required_snr_db: 11.0,
    },
    McsRung {
        mcs_index: 4,
        min_snr_db: 24.0,
        phy_mbps: 39.0,
        required_snr_db: 15.0,
    },
    McsRung {
        mcs_index: 5,
        min_snr_db: 30.0,
        phy_mbps: 52.0,
        required_snr_db: 18.0,
    },
];

/// Clamp a configured cap into the ladder's own bounds. A cap below the floor
/// pins the ladder to the floor (a legal way to disable climbing); a cap above
/// [`LADDER_MAX_MCS`] is clamped rather than rejected, because the config value
/// is operator-facing and a typo must not radiate an unmeasured rung.
pub fn clamp_cap(cap: u8) -> u8 {
    cap.clamp(MCS_FLOOR, LADDER_MAX_MCS)
}

/// The rung a measured SNR supports, ignoring any cap. `NaN` (never produced by
/// the stats parser, but cheap to be honest about) resolves to the floor.
pub fn rung_for_snr(snr_db: f64) -> u8 {
    if snr_db.is_nan() {
        return MCS_FLOOR;
    }
    // Highest rung first: the first threshold met wins.
    MCS_LADDER
        .iter()
        .rev()
        .find(|r| snr_db >= r.min_snr_db)
        .map(|r| r.mcs_index)
        .unwrap_or(MCS_FLOOR)
}

/// The rung a measured SNR supports under a configured cap — what the ladder
/// actually aims at.
pub fn target_mcs(snr_db: f64, cap: u8) -> u8 {
    rung_for_snr(snr_db).min(clamp_cap(cap))
}

/// Look up a rung's published figures, for logging a step with its rate.
pub fn rung(mcs_index: u8) -> Option<&'static McsRung> {
    MCS_LADDER.iter().find(|r| r.mcs_index == mcs_index)
}

/// The SNR ladder's cross-tick state: the live rung, the two streaks, and the
/// per-direction cooldown instants. Pure — `decide` is the whole decision and
/// `now` is injected, so the asymmetry is testable without sleeping.
#[derive(Debug, Clone)]
pub struct McsLadder {
    cap: u8,
    current: u8,
    bad_streak: u32,
    good_streak: u32,
    last_down_at: Option<Instant>,
    last_up_at: Option<Instant>,
    last_reason: String,
}

impl McsLadder {
    /// Ladder capped at `cap`, believing the transmitter is on `starting_mcs`.
    ///
    /// `starting_mcs` is recorded as-is, NOT clamped into the cap: it is what
    /// `wfb_tx` was spawned with, and pretending otherwise would make the first
    /// snapshot lie. A start above the cap simply reads as an unsupported rung
    /// and is stepped down on the next two samples.
    pub fn new(cap: u8, starting_mcs: u8) -> Self {
        Self {
            cap: clamp_cap(cap),
            current: starting_mcs,
            bad_streak: 0,
            good_streak: 0,
            last_down_at: None,
            last_up_at: None,
            last_reason: "initial".to_string(),
        }
    }

    /// The rung the ladder believes is live.
    pub fn current(&self) -> u8 {
        self.current
    }

    /// The effective cap (already clamped into `MCS_FLOOR..=LADDER_MAX_MCS`).
    pub fn cap(&self) -> u8 {
        self.cap
    }

    pub fn bad_streak(&self) -> u32 {
        self.bad_streak
    }

    pub fn good_streak(&self) -> u32 {
        self.good_streak
    }

    pub fn last_reason(&self) -> &str {
        &self.last_reason
    }

    /// Re-sync the believed rung to what the transmitter actually reports.
    ///
    /// `set_radio` can be silently ignored by the driver, and a whole-group
    /// respawn re-keys the data plane from the retained tunables. Either way the
    /// ladder must track reality, not its own intent, or every later decision is
    /// computed against a rung that is not on the air.
    pub fn observe(&mut self, live_mcs: u8) {
        if live_mcs != self.current {
            self.current = live_mcs;
        }
    }

    /// Fold one SNR sample in and return the rung to apply, or `None` to hold.
    ///
    /// A sample below the live rung's threshold grows the bad streak; two of them
    /// (past the step-down cooldown) drop straight to the supported rung. A
    /// sample supporting a higher rung grows the good streak; thirty of them
    /// (past the step-up cooldown) climb exactly one rung. A sample supporting
    /// exactly the live rung decays both streaks and holds.
    pub fn decide(&mut self, snr_db: f64, now: Instant) -> Option<u8> {
        let target = target_mcs(snr_db, self.cap);

        if target < self.current {
            self.bad_streak += 1;
            self.good_streak = 0;
            let cooldown_ok = self
                .last_down_at
                .is_none_or(|t| now.duration_since(t) >= STEP_DOWN_COOLDOWN);
            if self.bad_streak >= MCS_STEP_DOWN_REQUIRED_BAD_SAMPLES && cooldown_ok {
                let from = self.current;
                self.current = target;
                self.last_down_at = Some(now);
                self.bad_streak = 0;
                self.last_reason = format!("snr={snr_db:.0}_down_{from}_to_{target}");
                return Some(target);
            }
            return None;
        }

        if target > self.current {
            self.good_streak += 1;
            self.bad_streak = 0;
            let cooldown_ok = self
                .last_up_at
                .is_none_or(|t| now.duration_since(t) >= STEP_UP_COOLDOWN);
            if self.good_streak >= STEP_UP_REQUIRED_CLEAN_SAMPLES && cooldown_ok {
                let from = self.current;
                // One rung per climb, never past the SNR-supported target.
                self.current = (self.current + 1).min(target);
                self.last_up_at = Some(now);
                self.good_streak = 0;
                self.last_reason = format!("snr={snr_db:.0}_up_{from}_to_{}", self.current);
                return Some(self.current);
            }
            return None;
        }

        self.bad_streak = self.bad_streak.saturating_sub(1);
        self.good_streak = self.good_streak.saturating_sub(1);
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// Every documented rung boundary, tested exactly ON the threshold and one
    /// tenth of a dB below it. `>=` is the contract: 30.0 dB selects MCS 5, and
    /// 29.9 must not.
    #[test]
    fn snr_maps_to_the_documented_rung_at_every_boundary() {
        // (snr, expected rung) — on-threshold, just-below, and mid-band.
        let cases = [
            (f64::NEG_INFINITY, 1u8),
            (-10.0, 1),
            (0.0, 1),
            (14.9, 1),
            (15.0, 2),
            (15.1, 2),
            (20.9, 2),
            (21.0, 3),
            (21.1, 3),
            (23.9, 3),
            (24.0, 4),
            (24.1, 4),
            (29.9, 4),
            (30.0, 5),
            (30.1, 5),
            (35.0, 5),
            (f64::INFINITY, 5),
        ];
        for (snr, want) in cases {
            assert_eq!(
                rung_for_snr(snr),
                want,
                "snr {snr} must select MCS {want}, got {}",
                rung_for_snr(snr)
            );
        }
    }

    /// The bench-measured 35 dB selects the ladder's top rung — the whole reason
    /// this module exists, since the shipped static value was 1.
    #[test]
    fn measured_bench_snr_selects_the_top_rung() {
        assert_eq!(rung_for_snr(35.0), LADDER_MAX_MCS);
    }

    /// The no-measurement sentinel and a NaN both resolve to the floor, never to
    /// a high rung: an unmeasured link must not be treated as a clean one.
    #[test]
    fn missing_measurement_resolves_to_the_floor() {
        assert_eq!(rung_for_snr(0.0), MCS_FLOOR);
        assert_eq!(rung_for_snr(f64::NAN), MCS_FLOOR);
    }

    #[test]
    fn cap_is_clamped_into_the_ladder_bounds() {
        assert_eq!(clamp_cap(0), MCS_FLOOR);
        assert_eq!(clamp_cap(1), 1);
        assert_eq!(clamp_cap(3), 3);
        assert_eq!(clamp_cap(5), 5);
        // Above the hard ceiling, including the 0..=7 the wfb_tx arg accepts.
        assert_eq!(clamp_cap(6), LADDER_MAX_MCS);
        assert_eq!(clamp_cap(7), LADDER_MAX_MCS);
        assert_eq!(clamp_cap(255), LADDER_MAX_MCS);
    }

    /// The cap bounds the target even when the SNR would support far more.
    #[test]
    fn target_never_exceeds_the_cap() {
        for cap in 0..=7u8 {
            let t = target_mcs(35.0, cap);
            assert!(
                t <= clamp_cap(cap) && t <= LADDER_MAX_MCS,
                "cap {cap} produced target {t}"
            );
        }
        assert_eq!(target_mcs(35.0, 3), 3);
        assert_eq!(target_mcs(35.0, 2), 2);
        // A cap above what SNR supports does not invent headroom.
        assert_eq!(target_mcs(16.0, 5), 2);
    }

    #[test]
    fn default_cap_is_three_and_is_a_legal_rung() {
        assert_eq!(DEFAULT_LADDER_MAX_MCS, 3);
        assert_eq!(clamp_cap(DEFAULT_LADDER_MAX_MCS), DEFAULT_LADDER_MAX_MCS);
        assert!(rung(DEFAULT_LADDER_MAX_MCS).is_some());
    }

    /// Step DOWN takes exactly two consecutive bad samples: the first must hold.
    #[test]
    fn step_down_requires_two_consecutive_bad_samples() {
        let mut l = McsLadder::new(5, 5);
        let t0 = Instant::now();
        // 10 dB supports only MCS 1 while the ladder sits on 5.
        assert_eq!(l.decide(10.0, t0), None, "first bad sample must hold");
        assert_eq!(l.bad_streak(), 1);
        assert_eq!(l.decide(10.0, t0), Some(1), "second bad sample must act");
        assert_eq!(l.current(), 1);
    }

    /// A good sample between two bad ones resets the bad streak, so a single
    /// dropout cannot accumulate into a step-down across minutes.
    #[test]
    fn interleaved_good_sample_resets_the_bad_streak() {
        let mut l = McsLadder::new(5, 3);
        let t0 = Instant::now();
        assert_eq!(l.decide(10.0, t0), None); // bad: 10 dB < MCS 3's 21
        assert_eq!(l.bad_streak(), 1);
        assert_eq!(l.decide(35.0, t0), None); // good: supports 5 > 3
        assert_eq!(l.bad_streak(), 0);
        assert_eq!(l.decide(10.0, t0), None, "streak restarted, must hold");
        assert_eq!(l.current(), 3);
    }

    /// An on-rung sample decays the bad streak rather than freezing it, so a
    /// marginal period straddling a threshold triggers nothing.
    #[test]
    fn on_rung_sample_decays_both_streaks() {
        let mut l = McsLadder::new(3, 3);
        let t0 = Instant::now();
        assert_eq!(l.decide(10.0, t0), None);
        assert_eq!(l.bad_streak(), 1);
        // 21 dB supports exactly MCS 3, the live rung: neither bad nor good.
        assert_eq!(l.decide(21.0, t0), None);
        assert_eq!(l.bad_streak(), 0);
        assert_eq!(l.good_streak(), 0);
        assert_eq!(l.current(), 3);
    }

    /// A collapse drops straight to the supported rung, not one step at a time:
    /// an intermediate rung the SNR does not support is not a safe place to sit.
    #[test]
    fn step_down_lands_on_the_supported_rung_in_one_move() {
        let mut l = McsLadder::new(5, 5);
        let t0 = Instant::now();
        l.decide(10.0, t0);
        assert_eq!(l.decide(10.0, t0), Some(1));
        assert_eq!(l.current(), 1);
    }

    /// Step UP takes thirty consecutive good samples — sample 29 must hold.
    #[test]
    fn step_up_requires_thirty_consecutive_good_samples() {
        let mut l = McsLadder::new(5, 1);
        let t0 = Instant::now();
        for i in 1..STEP_UP_REQUIRED_CLEAN_SAMPLES {
            assert_eq!(l.decide(35.0, t0), None, "sample {i} must hold");
            assert_eq!(l.good_streak(), i);
        }
        assert_eq!(
            l.decide(35.0, t0),
            Some(2),
            "sample {STEP_UP_REQUIRED_CLEAN_SAMPLES} must act"
        );
        assert_eq!(l.current(), 2);
    }

    /// A climb probes one rung at a time even with 35 dB of headroom, and each
    /// step needs its own thirty samples plus the 30 s cooldown.
    #[test]
    fn climb_is_one_rung_per_thirty_samples_and_stops_at_the_cap() {
        let mut l = McsLadder::new(3, 1);
        let mut now = Instant::now();
        let mut reached = Vec::new();
        // Six full 30-sample windows: enough for 1->2->3 with three to spare.
        for _ in 0..6 {
            for _ in 0..STEP_UP_REQUIRED_CLEAN_SAMPLES {
                if let Some(m) = l.decide(35.0, now) {
                    reached.push(m);
                }
            }
            now += STEP_UP_COOLDOWN;
        }
        assert_eq!(reached, vec![2, 3], "one rung per window, then capped");
        assert_eq!(l.current(), 3);
    }

    /// The acceptance invariant: no sequence of samples, however clean, pushes
    /// the ladder past its configured cap — nor past the hard ceiling.
    #[test]
    fn ladder_never_exceeds_the_configured_cap() {
        for cap in [1u8, 2, 3, 4, 5, 7] {
            let mut l = McsLadder::new(cap, MCS_FLOOR);
            let mut now = Instant::now();
            for _ in 0..40 {
                for _ in 0..STEP_UP_REQUIRED_CLEAN_SAMPLES {
                    l.decide(f64::INFINITY, now);
                    assert!(
                        l.current() <= clamp_cap(cap),
                        "cap {cap} breached: {}",
                        l.current()
                    );
                    assert!(l.current() <= LADDER_MAX_MCS);
                }
                now += STEP_UP_COOLDOWN;
            }
            assert_eq!(l.current(), clamp_cap(cap), "cap {cap} not reached");
        }
    }

    /// A start above the cap is honest (not silently clamped) and is corrected
    /// downward by the normal two-bad-sample path.
    #[test]
    fn start_above_the_cap_is_reported_then_stepped_down() {
        let mut l = McsLadder::new(3, 5);
        assert_eq!(l.current(), 5, "must report the rung wfb_tx was spawned on");
        assert_eq!(l.cap(), 3);
        let t0 = Instant::now();
        // 35 dB supports MCS 5, but the cap makes the target 3 → a bad sample.
        assert_eq!(l.decide(35.0, t0), None);
        assert_eq!(l.decide(35.0, t0), Some(3));
        assert_eq!(l.current(), 3);
    }

    /// The step-down cooldown gates a second consecutive drop, so a flapping
    /// link cannot walk the ladder down faster than 5 s per step.
    #[test]
    fn step_down_cooldown_gates_a_second_drop() {
        let mut l = McsLadder::new(5, 5);
        let t0 = Instant::now();
        l.decide(22.0, t0); // supports 3, live 5 → bad
        assert_eq!(l.decide(22.0, t0), Some(3));
        // Still bad, but inside the 5 s cooldown: no further move.
        l.decide(10.0, t0);
        assert_eq!(l.decide(10.0, t0), None);
        assert_eq!(l.current(), 3);
        // Past the cooldown the very next sample drops: the bad streak already
        // sits past the required count from the blocked window, so the cooldown
        // was the only thing holding it — the same carry-over the tier ladder has.
        let later = t0 + STEP_DOWN_COOLDOWN + Duration::from_millis(1);
        assert_eq!(l.decide(10.0, later), Some(1));
        assert_eq!(l.current(), 1);
    }

    /// `observe` re-syncs to reality: after a respawn or a driver that ignored
    /// the request, later decisions are computed against the live rung.
    #[test]
    fn observe_resyncs_to_the_live_rung() {
        let mut l = McsLadder::new(5, 5);
        l.observe(1);
        assert_eq!(l.current(), 1);
        let t0 = Instant::now();
        // Now on rung 1 with 35 dB, this is a CLIMB, not a hold.
        assert_eq!(l.decide(35.0, t0), None);
        assert_eq!(l.good_streak(), 1);
    }

    /// A cap of 1 pins the ladder: no climb is ever possible, which is how an
    /// operator disables the rate half while leaving FEC adaptation armed.
    #[test]
    fn cap_of_one_pins_the_ladder_to_the_floor() {
        let mut l = McsLadder::new(1, 1);
        let mut now = Instant::now();
        for _ in 0..10 {
            for _ in 0..STEP_UP_REQUIRED_CLEAN_SAMPLES {
                assert_eq!(l.decide(35.0, now), None);
            }
            now += STEP_UP_COOLDOWN;
        }
        assert_eq!(l.current(), 1);
    }

    /// Every rung's trip threshold clears its own required SNR with real margin,
    /// and the table is strictly ascending in both rate and threshold. A future
    /// edit that inverts two rows fails here rather than on the air.
    #[test]
    fn ladder_table_is_ordered_and_holds_margin() {
        for w in MCS_LADDER.windows(2) {
            assert!(w[1].mcs_index > w[0].mcs_index);
            assert!(w[1].min_snr_db > w[0].min_snr_db);
            assert!(w[1].phy_mbps > w[0].phy_mbps);
            assert!(w[1].required_snr_db > w[0].required_snr_db);
        }
        // Skip the floor: it has no threshold to hold margin against.
        for r in MCS_LADDER.iter().skip(1) {
            let margin = r.min_snr_db - r.required_snr_db;
            assert!(
                margin >= 6.0,
                "MCS {} holds only {margin} dB of margin",
                r.mcs_index
            );
        }
        assert_eq!(MCS_LADDER[0].mcs_index, MCS_FLOOR);
        assert_eq!(MCS_LADDER[MCS_LADDER.len() - 1].mcs_index, LADDER_MAX_MCS);
    }
}
