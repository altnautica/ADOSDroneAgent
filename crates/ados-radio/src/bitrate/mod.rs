//! Closed-loop video bitrate + FEC + MCS controller.
//!
//! Watches the live link-quality stats and drives two independent ladders off
//! one 1 Hz sample:
//!
//! - the **bitrate/FEC ladder** ([`Hysteresis`] over [`DEFAULT_TIERS`]), stepped on packet-loss + RSSI
//!   hysteresis, which applies the rung's Reed-Solomon `(k, n)` to the live data
//!   plane and the rung's `bitrate_kbps` to the video encoder as a ceiling;
//! - the **SNR/MCS ladder** ([`crate::mcs_ladder`]), which tracks measured SNR up
//!   and down a five-rung modulation table under a configured cap.
//!
//! Both apply through the running `wfb_tx`'s management socket
//! ([`crate::tx_cmd`]) whenever it answers, and fall back to the historical
//! kill-and-respawn only when it does not — so the routine case costs no video
//! blackout instead of the 1-2 s a respawn costs.
//!
//! Exactly one radio knob is varied at runtime: the **MCS index**. Channel width
//! is pinned at 20 MHz and TX power is left to the adapter's own
//! ramp-until-accepted path. See [`crate::tx_cmd::PINNED_BANDWIDTH_MHZ`].
//!
//! The encoder half is no longer a cross-process no-op. The rung's bitrate is
//! published to `ados-video` as a ceiling composed under the hero/thumbnail
//! profile (`min(profile, ceiling)`), so a degrading link emits FEWER on-air
//! bytes rather than more — previously the rescue rung tripled FEC on an
//! already-strained channel while the encoder kept pushing 4000 kbps.
//!
//! OFF by default per-rig (`adaptive_bitrate_enabled`). When off, the loop still
//! ticks at its cadence so the snapshot surface stays populated for the
//! `/api/video/config` consumers, it never touches the data plane, and it clears
//! the encoder ceiling (an unarmed ladder must not clamp the encoder).
//!
//! Hysteresis (1 Hz sampling on the bench):
//! - **Step down** after a sustained bad window: `loss > 5%` OR `rssi < -75` for
//!   5 consecutive samples (5 s). A degrading link is the urgent case. The MCS
//!   ladder is harsher still — 2 samples — because losing modulation margin
//!   shows up as a dead link, not a soft-degraded one.
//! - **Step up** after a sustained clean window: `loss < 1%` AND `rssi > -65`
//!   for 30 consecutive samples (30 s). Conservative, to avoid ping-pong.
//! - **Step-down cooldown** 5 s, **step-up cooldown** 30 s, so the link settles
//!   on a new rung before the next decision.
//! - An intermediate sample (neither bad nor clean) decays both streaks toward
//!   zero so a marginal period triggers nothing.
//!
//! Numeric thresholds, streak lengths, cooldowns, and the rung ladder are
//! byte-identical to the Python controller they replace.

mod controller;
mod tiers;

use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Mutex;

use crate::config::WfbConfig;

pub use controller::{resolve_sample, BitrateController, ResolvedSample, SampleSource};
pub use tiers::{BitrateTier, Hysteresis, TierAction, DEFAULT_TIERS};

/// Step-down trip thresholds + the consecutive bad-sample count required.
const STEP_DOWN_LOSS_PCT: f64 = 5.0;
const STEP_DOWN_RSSI_DBM: f64 = -75.0;
const STEP_DOWN_REQUIRED_BAD_SAMPLES: u32 = 5;

/// Step-up trip thresholds + the consecutive clean-sample count required.
const STEP_UP_LOSS_PCT: f64 = 1.0;
const STEP_UP_RSSI_DBM: f64 = -65.0;
/// Shared with the SNR/MCS ladder ([`crate::mcs_ladder`]), which climbs on the
/// same 30-sample patience against a 2-sample drop.
pub(crate) const STEP_UP_REQUIRED_CLEAN_SAMPLES: u32 = 30;

/// Minimum spacing between consecutive rung changes in each direction. Shared
/// with the SNR/MCS ladder so both ladders settle on the same schedule.
pub(crate) const STEP_DOWN_COOLDOWN: Duration = Duration::from_secs(5);
pub(crate) const STEP_UP_COOLDOWN: Duration = Duration::from_secs(30);

/// Default poll cadence (1 Hz).
const DEFAULT_TICK_INTERVAL: Duration = Duration::from_secs(1);

/// The diagnostic snapshot the heartbeat reads onto `wfb-stats.json`. Carries
/// both halves of the loop: what the controller recommends and what it observed
/// actually take effect. Shared via [`SnapshotHandle`].
#[derive(Debug, Clone, Default)]
pub struct BitrateSnapshot {
    pub link_preset: String,
    pub adaptive_bitrate_enabled: bool,
    pub recommended_bitrate_kbps: u32,
    pub tier_idx: usize,
    pub tier_name: &'static str,
    /// The data plane's LIVE Reed-Solomon ratio + MCS index, refreshed from the
    /// running process group each tick. Distinct from the configured values:
    /// after a runtime FEC/MCS/preset change (or an adaptive step) these reflect
    /// what `wfb_tx` is actually transmitting, so the heartbeat surfaces reality
    /// rather than the boot-time config.
    pub fec_k: u8,
    pub fec_n: u8,
    pub mcs_index: u8,
    /// The SNR the MCS ladder last decided on, in dB. Surfaced beside
    /// `mcs_index` so the operator sees *why* a rung is live, not just which —
    /// the "auto (MCS N at X dB)" readout the settings page renders.
    pub snr_db: f64,
    /// The configured ceiling on the MCS ladder's top rung. Without it the
    /// operator cannot tell a link-limited rung from a policy-limited one.
    pub mcs_ladder_cap: u8,
    /// The encoder bitrate `ados-video` reports actually running, in kbps —
    /// `min(profile nominal, ladder ceiling)` as resolved on that side. `None`
    /// means `ados-video` published no state (service down, or no pipeline on
    /// this node), which is deliberately distinct from "running unclamped": an
    /// unreachable encoder must not render as a healthy one.
    pub encoder_bitrate_kbps: Option<u32>,
    /// How the live FEC/MCS changes were applied. `tx_cmd_applies` used the
    /// `wfb_tx` management socket (no video gap); `respawn_applies` fell back to
    /// kill-and-respawn (a 1-2 s gap). A rig where the second counter grows is a
    /// rig where `-C` never reached the transmitter.
    pub tx_cmd_applies: u64,
    pub respawn_applies: u64,
    /// Management-socket attempts that failed and forced a respawn.
    pub tx_cmd_failures: u64,
    /// Which measurement drove this tick: this node's own receiver, the peer's
    /// report of what it decoded, or nothing. Without it a rung held for want
    /// of any signal is indistinguishable from one chosen on a good sample.
    pub sample_source: &'static str,
}

impl BitrateSnapshot {
    /// The initial snapshot before the controller has acted: rung 0's bitrate
    /// and the configured trio (the data plane comes up on the config values).
    fn initial(cfg: &WfbConfig, tiers: &[BitrateTier], starting_idx: usize) -> Self {
        let idx = starting_idx.min(tiers.len().saturating_sub(1));
        let tier = &tiers[idx];
        Self {
            link_preset: cfg.wfb_link_preset.clone(),
            adaptive_bitrate_enabled: cfg.adaptive_bitrate_enabled,
            recommended_bitrate_kbps: tier.bitrate_kbps,
            tier_idx: idx,
            tier_name: tier.name,
            fec_k: cfg.fec_k,
            fec_n: cfg.fec_n,
            mcs_index: cfg.mcs_index,
            // No sample yet: 0.0 is the same no-measurement sentinel `LinkStats`
            // starts at, and the sidecar gates it behind a real decode.
            snr_db: 0.0,
            sample_source: SampleSource::None.as_str(),
            mcs_ladder_cap: crate::mcs_ladder::clamp_cap(cfg.adaptive_mcs_max),
            encoder_bitrate_kbps: None,
            tx_cmd_applies: 0,
            respawn_applies: 0,
            tx_cmd_failures: 0,
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
/// flag (the controller resumes stepping FEC/MCS on link quality), `manual`
/// clears it (the operator's pinned trio stands). One flag is shared across radio
/// respawns so the choice survives a watchdog kill or a channel hop.
pub type EnabledHandle = Arc<AtomicBool>;

/// Construct the shared enable handle, seeded from config.
pub fn new_enabled(cfg: &WfbConfig) -> EnabledHandle {
    Arc::new(AtomicBool::new(cfg.adaptive_bitrate_enabled))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;

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
        // The live trio seeds from the configured values until the first tick.
        assert_eq!(snap.fec_k, 8);
        assert_eq!(snap.fec_n, 12);
        assert_eq!(snap.mcs_index, 1);
        // The ladder cap comes from config, clamped; no measurement yet, and no
        // encoder state observed yet (which is NOT the same as "no ceiling").
        assert_eq!(
            snap.mcs_ladder_cap,
            crate::mcs_ladder::DEFAULT_LADDER_MAX_MCS
        );
        assert_eq!(snap.snr_db, 0.0);
        assert!(snap.encoder_bitrate_kbps.is_none());
        assert_eq!(snap.tx_cmd_applies, 0);
        assert_eq!(snap.respawn_applies, 0);
    }

    /// An out-of-range configured cap is clamped into the snapshot, so the panel
    /// never advertises a rung the ladder will not select.
    #[test]
    fn snapshot_clamps_an_out_of_range_ladder_cap() {
        let cfg = WfbConfig {
            adaptive_mcs_max: 7,
            ..WfbConfig::default()
        };
        let snap = BitrateSnapshot::initial(&cfg, &DEFAULT_TIERS, 0);
        assert_eq!(snap.mcs_ladder_cap, crate::mcs_ladder::LADDER_MAX_MCS);
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
        let off = WfbConfig {
            adaptive_bitrate_enabled: false,
            ..WfbConfig::default()
        };
        assert!(!new_enabled(&off).load(Ordering::Relaxed));
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
