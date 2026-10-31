//! The 1 Hz sampling loop and the actuation half of the adaptive controller.
//!
//! Owns the I/O the two ladders ([`super::tiers`] and [`crate::mcs_ladder`])
//! deliberately do not: reading the peer's link report, driving `wfb_tx` through
//! [`crate::process::RadioProcesses`], publishing the encoder bitrate ceiling to
//! `ados-video`, and refreshing the heartbeat snapshot.

use ados_protocol::shutdown::Shutdown;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use ados_video::profile::{EncoderState, VIDEO_ENCODER_SOCK, VIDEO_PROFILE_SIDECAR};
use tokio::sync::Mutex;

use crate::mcs_ladder::{self, McsLadder};
use crate::process::RadioProcesses;

use super::{
    BitrateTier, EnabledHandle, Hysteresis, SnapshotHandle, TierAction, DEFAULT_TICK_INTERVAL,
    DEFAULT_TIERS,
};

mod ceiling;
mod sample;

use ceiling::*;
pub use sample::*;

/// The closed-loop bitrate + FEC + MCS controller.
pub struct BitrateController {
    tiers: Vec<BitrateTier>,
    tick_interval: Duration,
    /// Runtime-flippable enable flag (shared with the command socket). Read each
    /// tick so the auto/manual toggle takes effect without a respawn.
    enabled: EnabledHandle,
    hysteresis: Hysteresis,
    /// The SNR-tracking modulation ladder. The only radio knob varied at runtime.
    mcs: McsLadder,
    /// The encoder bitrate ceiling this controller last successfully published.
    /// Starts `None`, which is exactly `ados-video`'s boot state, so the two
    /// agree before the first tick and no spurious apply is issued.
    asserted_ceiling_kbps: Option<u32>,
    /// Backoff state for failed ceiling publishes. See [`CeilingRetry`].
    ceiling_retry: CeilingRetry,
    encoder_sock: PathBuf,
    encoder_sidecar: PathBuf,
}

impl BitrateController {
    /// Build a controller over the default ladder, reading the enable flag from
    /// the shared handle. Starts at rung 0 (the high-quality default) with the
    /// MCS ladder capped at `mcs_cap` and believing the transmitter came up on
    /// `starting_mcs` (what `wfb_tx` was actually spawned with).
    pub fn new(enabled: EnabledHandle, mcs_cap: u8, starting_mcs: u8) -> Self {
        Self::with_tiers_shared(DEFAULT_TIERS.to_vec(), enabled, DEFAULT_TICK_INTERVAL)
            .with_mcs_ladder(mcs_cap, starting_mcs)
    }

    /// Build a controller over an explicit ladder + a fixed enable bool + tick
    /// cadence (the seam the tests use to drive the loop fast). The bool is
    /// wrapped in a private handle so a test that never flips it behaves exactly
    /// as before.
    pub fn with_tiers(tiers: Vec<BitrateTier>, enabled: bool, tick_interval: Duration) -> Self {
        Self::with_tiers_shared(
            tiers,
            Arc::new(std::sync::atomic::AtomicBool::new(enabled)),
            tick_interval,
        )
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
            mcs: McsLadder::new(mcs_ladder::DEFAULT_LADDER_MAX_MCS, mcs_ladder::MCS_FLOOR),
            asserted_ceiling_kbps: None,
            ceiling_retry: CeilingRetry::default(),
            encoder_sock: PathBuf::from(VIDEO_ENCODER_SOCK),
            encoder_sidecar: PathBuf::from(VIDEO_PROFILE_SIDECAR),
        }
    }

    /// Override the MCS ladder's cap and its believed starting rung.
    pub fn with_mcs_ladder(mut self, cap: u8, starting_mcs: u8) -> Self {
        self.mcs = McsLadder::new(cap, starting_mcs);
        self
    }

    /// Point the encoder-ceiling plumbing at explicit paths (tests, alternate
    /// run dirs).
    pub fn with_encoder_paths(mut self, sock: &Path, sidecar: &Path) -> Self {
        self.encoder_sock = sock.to_path_buf();
        self.encoder_sidecar = sidecar.to_path_buf();
        self
    }

    /// Run the controller until `cancel` fires.
    ///
    /// Each tick reads the receiving peer's link report, folds it through both
    /// ladders, and — only when enabled — applies the results to the data plane.
    /// The snapshot is refreshed every tick (even when disabled) so the heartbeat
    /// surface stays current.
    pub async fn run(
        mut self,
        proc: Arc<Mutex<RadioProcesses>>,
        snapshot: SnapshotHandle,
        counters: crate::watchdog::CounterHandle,
        cancel: Shutdown,
    ) {
        tracing::info!(
            enabled = self.enabled.load(Ordering::Relaxed),
            tier = self.tiers[self.hysteresis.current_tier_idx()].name,
            mcs = self.mcs.current(),
            mcs_cap = self.mcs.cap(),
            "bitrate_controller_started"
        );
        loop {
            tokio::select! {
                _ = tokio::time::sleep(self.tick_interval) => {
                    self.tick(&proc, &snapshot, &counters).await;
                }
                _ = cancel.wait() => {
                    tracing::info!("bitrate_controller_stopped");
                    return;
                }
            }
        }
    }

    /// One control tick: sample → decide → (when enabled) actuate → snapshot →
    /// reconcile the encoder ceiling.
    ///
    /// The encoder reconcile is deliberately **last**. Applying a ceiling can
    /// cost an encoder respawn, and `ados-video` is allowed several seconds to
    /// confirm one; doing it before the radio actuation would let a slow encoder
    /// delay a step-down on a degrading link, and doing it before the snapshot
    /// would stall the heartbeat surface. Last means the only thing a slow
    /// encoder delays is the *next* sample, which is safe: a later sample just
    /// makes both streaks take longer to trip.
    async fn tick(
        &mut self,
        proc: &Arc<Mutex<RadioProcesses>>,
        snapshot: &SnapshotHandle,
        counters: &crate::watchdog::CounterHandle,
    ) {
        // The receiving peer measures the downlink and reports it on the aux
        // lane; that report is the only sample the ladders act on.
        let peer = ados_protocol::link_feedback::read_sidecar_from(
            &ados_protocol::link_feedback::sidecar_path(),
        );
        let sample = resolve_sample(peer.as_ref(), now_unix_ms());
        let (loss, rssi, snr) = (sample.loss_percent, sample.rssi_dbm, sample.snr_db);
        let has_sample = sample.has_sample();

        let enabled = self.enabled.load(Ordering::Relaxed);
        if enabled && has_sample {
            // Track reality before deciding. A channel hop, a watchdog respawn or
            // an operator `set_mcs` moves the live rung behind the ladder's back,
            // and a decision computed against a rung that is not on the air is
            // wrong in both directions.
            let live_mcs = proc.lock().await.data_mcs();
            self.mcs.observe(live_mcs);

            let now = Instant::now();

            // Ladder 1: bitrate + FEC, on loss and RSSI.
            let action = self.hysteresis.decide(loss, rssi, now);
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
                if !proc.lock().await.set_fec(tier.fec_k, tier.fec_n).await {
                    tracing::warn!(tier = tier.name, "bitrate_tier_set_fec_failed");
                }
            }

            // Ladder 2: modulation, on SNR. The only radio knob varied at runtime.
            if let Some(mcs) = self.mcs.decide(snr, now) {
                tracing::info!(
                    mcs,
                    snr_db = snr,
                    cap = self.mcs.cap(),
                    phy_mbps = mcs_ladder::rung(mcs).map(|r| r.phy_mbps),
                    reason = self.mcs.last_reason(),
                    "mcs_ladder_step"
                );
                let ok = proc.lock().await.set_mcs(mcs).await;
                // Re-sync from the live value either way: a failed apply rolls
                // the retained index back, and a driver that accepted the command
                // but ignored the rung would otherwise be papered over.
                let live = proc.lock().await.data_mcs();
                if !ok || live != mcs {
                    tracing::warn!(
                        requested = mcs,
                        live,
                        applied = ok,
                        "mcs_ladder_step_not_applied"
                    );
                }
                self.mcs.observe(live);
            }
        } else if enabled {
            // No usable peer report: it is stale, it reports that the peer
            // decoded nothing, or none has arrived yet.
            //
            // Congestion needs no receiver. A transmit queue that stays deep
            // while the radio drains it says directly that more is being
            // offered than the air is carrying, so drive the bitrate ladder
            // from that.
            //
            // The modulation ladder falls back to its floor rung. Feedback most
            // often stops because the link got worse, and the floor is the rung
            // a weak link can still carry; holding a high rung would keep the
            // downlink dead at range. It climbs again only on real samples.
            let live_mcs = proc.lock().await.data_mcs();
            self.mcs.observe(live_mcs);
            if let Some(mcs) = self.mcs.decide_no_sample(Instant::now()) {
                tracing::info!(mcs, reason = self.mcs.last_reason(), "mcs_ladder_step");
                let ok = proc.lock().await.set_mcs(mcs).await;
                let live = proc.lock().await.data_mcs();
                if !ok || live != mcs {
                    tracing::warn!(
                        requested = mcs,
                        live,
                        applied = ok,
                        "mcs_ladder_step_not_applied"
                    );
                }
                self.mcs.observe(live);
            }
            let congested = counters.lock().await.tx_video_backpressured;
            let action = self.hysteresis.decide_congestion(congested, Instant::now());
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
                if !proc.lock().await.set_fec(tier.fec_k, tier.fec_n).await {
                    tracing::warn!(tier = tier.name, "bitrate_tier_set_fec_failed");
                }
            }
        }

        let ((fec_k, fec_n), mcs_index, applies) = {
            let p = proc.lock().await;
            (p.data_fec(), p.data_mcs(), p.apply_counters())
        };

        // The ceiling the ladder wants live. `None` when disabled: the ceiling is
        // the adaptive ladder's clamp, so an unarmed ladder must not clamp the
        // encoder at all — flipping to manual un-clamps it on the next tick.
        let tier = self.tiers[self.hysteresis.current_tier_idx()];
        let want_ceiling = enabled.then_some(tier.bitrate_kbps);

        // Cheap sidecar read (no socket), used both for the snapshot and to decide
        // whether the ceiling needs republishing.
        let observed = ados_video::profile::read_state_from(&self.encoder_sidecar);

        {
            let mut snap = snapshot.lock().await;
            snap.adaptive_bitrate_enabled = enabled;
            snap.recommended_bitrate_kbps = tier.bitrate_kbps;
            snap.tier_idx = self.hysteresis.current_tier_idx();
            snap.tier_name = tier.name;
            snap.fec_k = fec_k;
            snap.fec_n = fec_n;
            snap.mcs_index = mcs_index;
            snap.snr_db = snr;
            snap.mcs_ladder_cap = self.mcs.cap();
            snap.encoder_bitrate_kbps = observed.as_ref().map(|s| s.bitrate_kbps);
            snap.tx_cmd_applies = applies.tx_cmd;
            snap.respawn_applies = applies.respawn;
            snap.tx_cmd_failures = applies.tx_cmd_failed;
            snap.sample_source = sample.source.as_str();
            // Only a real sample carries a loss figure; without one the number
            // in `sample` is a placeholder that would read as a clean link.
            snap.sample_loss_percent = has_sample.then_some(loss);
        }

        self.reconcile_encoder_ceiling(want_ceiling, observed.as_ref())
            .await;
    }
}

#[cfg(test)]
mod tests;
