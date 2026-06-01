//! Closed-loop video bitrate + FEC controller.
//!
//! Watches the live link-quality stats and steps a four-rung bitrate/FEC ladder
//! up or down on packet-loss + RSSI hysteresis. On each rung change it applies
//! the rung's Reed-Solomon `(k, n)` to the live data plane by restarting only
//! `wfb_tx` (the control planes keep their fixed FEC); the bitrate half is a
//! cross-process no-op here because the video encoder lives in `ados-video`, so
//! the controller surfaces the intended bitrate without driving the encoder.
//!
//! OFF by default (`adaptive_bitrate_enabled = false`). When off, the loop still
//! ticks at its cadence so the snapshot surface stays populated for the
//! `/api/video/config` consumers, but it never restarts the data plane.
//!
//! Hysteresis (1 Hz sampling on the bench):
//! - **Step down** after a sustained bad window: `loss > 5%` OR `rssi < -75` for
//!   5 consecutive samples (5 s). A degrading link is the urgent case.
//! - **Step up** after a sustained clean window: `loss < 1%` AND `rssi > -65`
//!   for 30 consecutive samples (30 s). Conservative, to avoid ping-pong.
//! - **Step-down cooldown** 5 s, **step-up cooldown** 30 s, so the link settles
//!   on a new rung before the next decision.
//! - An intermediate sample (neither bad nor clean) decays both streaks toward
//!   zero so a marginal period triggers nothing.
//!
//! Numeric thresholds, streak lengths, cooldowns, and the rung ladder are
//! byte-identical to the Python controller they replace.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::{Mutex, Notify};

use crate::config::WfbConfig;
use crate::link_quality::LinkStats;
use crate::process::RadioProcesses;

/// Step-down trip thresholds + the consecutive bad-sample count required.
const STEP_DOWN_LOSS_PCT: f64 = 5.0;
const STEP_DOWN_RSSI_DBM: f64 = -75.0;
const STEP_DOWN_REQUIRED_BAD_SAMPLES: u32 = 5;

/// Step-up trip thresholds + the consecutive clean-sample count required.
const STEP_UP_LOSS_PCT: f64 = 1.0;
const STEP_UP_RSSI_DBM: f64 = -65.0;
const STEP_UP_REQUIRED_CLEAN_SAMPLES: u32 = 30;

/// Minimum spacing between consecutive rung changes in each direction.
const STEP_DOWN_COOLDOWN: Duration = Duration::from_secs(5);
const STEP_UP_COOLDOWN: Duration = Duration::from_secs(30);

/// Default poll cadence (1 Hz).
const DEFAULT_TICK_INTERVAL: Duration = Duration::from_secs(1);

/// One rung of the bitrate / FEC ladder. `bitrate_kbps` is the intended video
/// encoder bitrate (surfaced, not actuated here); `fec_k`/`fec_n` drive the
/// `wfb_tx` Reed-Solomon configuration.
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
}

/// The diagnostic snapshot the heartbeat reads onto `wfb-stats.json`. Carries
/// the controller's intent so the GCS panel renders it even with the encoder
/// restart deferred. Shared via [`SnapshotHandle`].
#[derive(Debug, Clone)]
pub struct BitrateSnapshot {
    pub link_preset: String,
    pub adaptive_bitrate_enabled: bool,
    pub recommended_bitrate_kbps: u32,
    pub tier_idx: usize,
    pub tier_name: &'static str,
}

impl BitrateSnapshot {
    /// The initial snapshot before the controller has acted: rung 0's bitrate.
    fn initial(cfg: &WfbConfig, tiers: &[BitrateTier], starting_idx: usize) -> Self {
        let idx = starting_idx.min(tiers.len().saturating_sub(1));
        let tier = &tiers[idx];
        Self {
            link_preset: cfg.wfb_link_preset.clone(),
            adaptive_bitrate_enabled: cfg.adaptive_bitrate_enabled,
            recommended_bitrate_kbps: tier.bitrate_kbps,
            tier_idx: idx,
            tier_name: tier.name,
        }
    }
}

/// Shared handle to the controller snapshot (mirrors the `LinkStats` share).
pub type SnapshotHandle = Arc<Mutex<BitrateSnapshot>>;

/// Construct the shared snapshot handle, seeded from config + the ladder.
pub fn new_snapshot(cfg: &WfbConfig) -> SnapshotHandle {
    Arc::new(Mutex::new(BitrateSnapshot::initial(cfg, &DEFAULT_TIERS, 0)))
}

/// Shared, runtime-flippable adaptive-controller enable flag.
///
/// The controller reads this each tick instead of a fixed bool so the operator
/// command socket's auto/manual tier toggle takes effect live: `auto` arms the
/// flag (the controller resumes stepping FEC on link quality), `manual` clears
/// it (the operator's pinned trio stands). One flag is shared across radio
/// respawns so the choice survives a watchdog kill or a channel hop.
pub type EnabledHandle = Arc<AtomicBool>;

/// Construct the shared enable handle, seeded from config.
pub fn new_enabled(cfg: &WfbConfig) -> EnabledHandle {
    Arc::new(AtomicBool::new(cfg.adaptive_bitrate_enabled))
}

/// The closed-loop bitrate + FEC controller.
pub struct BitrateController {
    tiers: Vec<BitrateTier>,
    tick_interval: Duration,
    /// Runtime-flippable enable flag (shared with the command socket). Read each
    /// tick so the auto/manual toggle takes effect without a respawn.
    enabled: EnabledHandle,
    hysteresis: Hysteresis,
}

impl BitrateController {
    /// Build a controller over the default ladder, reading the enable flag from
    /// the shared handle. Starts at rung 0 (the high-quality default).
    pub fn new(enabled: EnabledHandle) -> Self {
        Self::with_tiers_shared(DEFAULT_TIERS.to_vec(), enabled, DEFAULT_TICK_INTERVAL)
    }

    /// Build a controller over an explicit ladder + a fixed enable bool + tick
    /// cadence (the seam the tests use to drive the loop fast). The bool is
    /// wrapped in a private handle so a test that never flips it behaves exactly
    /// as before.
    pub fn with_tiers(tiers: Vec<BitrateTier>, enabled: bool, tick_interval: Duration) -> Self {
        Self::with_tiers_shared(tiers, Arc::new(AtomicBool::new(enabled)), tick_interval)
    }

    /// Build a controller over an explicit ladder + a shared enable handle + tick
    /// cadence.
    pub fn with_tiers_shared(
        tiers: Vec<BitrateTier>,
        enabled: EnabledHandle,
        tick_interval: Duration,
    ) -> Self {
        let tier_count = tiers.len().max(1);
        Self {
            tiers,
            tick_interval,
            enabled,
            hysteresis: Hysteresis::new(tier_count, 0),
        }
    }

    /// Run the controller until `cancel` fires.
    ///
    /// Each tick reads the live `LinkStats`, folds it through the hysteresis, and
    /// — only when enabled — applies a rung change by restarting the data plane
    /// with the new FEC. The snapshot is refreshed every tick (even when
    /// disabled) so the heartbeat surface stays current.
    pub async fn run(
        mut self,
        link: Arc<Mutex<LinkStats>>,
        proc: Arc<Mutex<RadioProcesses>>,
        snapshot: SnapshotHandle,
        cancel: Arc<Notify>,
    ) {
        tracing::info!(
            enabled = self.enabled.load(Ordering::Relaxed),
            tier = self.tiers[self.hysteresis.current_tier_idx()].name,
            "bitrate_controller_started"
        );
        loop {
            tokio::select! {
                _ = tokio::time::sleep(self.tick_interval) => {
                    self.tick(&link, &proc, &snapshot).await;
                }
                _ = cancel.notified() => {
                    tracing::info!("bitrate_controller_stopped");
                    return;
                }
            }
        }
    }

    /// One control tick: sample → decide → (when enabled) actuate → snapshot.
    async fn tick(
        &mut self,
        link: &Arc<Mutex<LinkStats>>,
        proc: &Arc<Mutex<RadioProcesses>>,
        snapshot: &SnapshotHandle,
    ) {
        let (loss, rssi, has_sample) = {
            let s = link.lock().await;
            // Cold-start: with no real sample yet (empty timestamp, 0 packets),
            // hold the rung so default sentinels never force a step-down. Same
            // guard the reactive-hop path uses for the drone-only-rig case.
            let has_sample = !s.timestamp.is_empty() && s.packets_received > 0;
            (s.loss_percent, s.rssi_dbm, has_sample)
        };

        let enabled = self.enabled.load(Ordering::Relaxed);
        if enabled && has_sample {
            let action = self.hysteresis.decide(loss, rssi, Instant::now());
            if action != TierAction::Hold {
                let tier = self.tiers[self.hysteresis.current_tier_idx()];
                tracing::info!(
                    tier = tier.name,
                    reason = self.hysteresis.last_action_reason(),
                    fec_k = tier.fec_k,
                    fec_n = tier.fec_n,
                    bitrate_kbps = tier.bitrate_kbps,
                    "bitrate_tier_change"
                );
                // Apply the rung's FEC to the live data plane (control planes
                // untouched). The bitrate half is a cross-process no-op: the
                // encoder lives in another service, so we surface the intended
                // bitrate in the snapshot without driving it here.
                let ok = proc.lock().await.set_fec(tier.fec_k, tier.fec_n).await;
                if !ok {
                    tracing::warn!(tier = tier.name, "bitrate_tier_set_fec_failed");
                }
            }
        }

        // Refresh the snapshot every tick (even disabled) so the panel sees the
        // current recommended rung + the live enable flag.
        let tier = self.tiers[self.hysteresis.current_tier_idx()];
        let mut snap = snapshot.lock().await;
        snap.adaptive_bitrate_enabled = enabled;
        snap.recommended_bitrate_kbps = tier.bitrate_kbps;
        snap.tier_idx = self.hysteresis.current_tier_idx();
        snap.tier_name = tier.name;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn snapshot_seeds_from_config_and_top_rung() {
        let cfg = WfbConfig {
            wfb_link_preset: "balanced".to_string(),
            adaptive_bitrate_enabled: true,
            ..WfbConfig::default()
        };
        let snap = BitrateSnapshot::initial(&cfg, &DEFAULT_TIERS, 0);
        assert_eq!(snap.link_preset, "balanced");
        assert!(snap.adaptive_bitrate_enabled);
        assert_eq!(snap.recommended_bitrate_kbps, 4000);
        assert_eq!(snap.tier_idx, 0);
        assert_eq!(snap.tier_name, "high");
    }

    #[test]
    fn new_enabled_seeds_from_config() {
        // The shared handle the command socket flips is seeded from config so a
        // rig that boots with adaptive on stays on until an operator toggles it.
        let on = WfbConfig {
            adaptive_bitrate_enabled: true,
            ..WfbConfig::default()
        };
        assert!(new_enabled(&on).load(Ordering::Relaxed));
        let off = WfbConfig::default();
        assert!(!new_enabled(&off).load(Ordering::Relaxed));
    }

    #[test]
    fn shared_enable_handle_is_read_live_not_captured() {
        // The controller must read the shared flag, not snapshot it at
        // construction: flipping the handle AFTER the controller is built changes
        // what the next tick would do. Constructing the controller over a shared
        // handle and then flipping it proves the wiring without running a tick
        // (which would fork wfb_tx).
        let flag: EnabledHandle = Arc::new(AtomicBool::new(false));
        let ctrl = BitrateController::with_tiers_shared(
            DEFAULT_TIERS.to_vec(),
            flag.clone(),
            DEFAULT_TICK_INTERVAL,
        );
        // The controller and the command-socket side hold the SAME atomic.
        assert!(!ctrl.enabled.load(Ordering::Relaxed));
        flag.store(true, Ordering::Relaxed);
        assert!(ctrl.enabled.load(Ordering::Relaxed));
    }

    #[test]
    fn thresholds_match_python_constants() {
        // Guard against an accidental edit of the load-bearing tuning constants.
        assert_eq!(STEP_DOWN_LOSS_PCT, 5.0);
        assert_eq!(STEP_DOWN_RSSI_DBM, -75.0);
        assert_eq!(STEP_DOWN_REQUIRED_BAD_SAMPLES, 5);
        assert_eq!(STEP_UP_LOSS_PCT, 1.0);
        assert_eq!(STEP_UP_RSSI_DBM, -65.0);
        assert_eq!(STEP_UP_REQUIRED_CLEAN_SAMPLES, 30);
        assert_eq!(STEP_DOWN_COOLDOWN.as_secs(), 5);
        assert_eq!(STEP_UP_COOLDOWN.as_secs(), 30);
    }
}
