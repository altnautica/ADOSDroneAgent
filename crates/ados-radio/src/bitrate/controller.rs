//! The 1 Hz sampling loop and the actuation half of the adaptive controller.
//!
//! Owns the I/O the two ladders ([`super::tiers`] and [`crate::mcs_ladder`])
//! deliberately do not: reading the live link stats, driving `wfb_tx` through
//! [`crate::process::RadioProcesses`], publishing the encoder bitrate ceiling to
//! `ados-video`, and refreshing the heartbeat snapshot.

use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use ados_video::profile::{EncoderState, VIDEO_ENCODER_SOCK, VIDEO_PROFILE_SIDECAR};
use tokio::sync::{Mutex, Notify};

use crate::link_quality::LinkStats;
use crate::mcs_ladder::{self, McsLadder};
use crate::process::RadioProcesses;

use super::{
    BitrateTier, EnabledHandle, Hysteresis, SnapshotHandle, TierAction, DEFAULT_TICK_INTERVAL,
    DEFAULT_TIERS,
};

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
    encoder_sock: PathBuf,
    encoder_sidecar: PathBuf,
}

/// Whether the encoder ceiling needs (re)publishing.
///
/// `observed` is `ados-video`'s own published ceiling: `Some(live)` when it has
/// stamped its sidecar, `None` when it has not (service down, or no pipeline on
/// this node). When there is a published truth, reconcile against it — that is
/// what makes the ceiling self-heal across an `ados-video` restart, which resets
/// the ceiling to `None` while this controller still believes it is clamped.
/// With no published truth, fall back to comparing against our own last
/// successful publish, so a down service costs one failed attempt per rung change
/// instead of one per second.
fn ceiling_needs_apply(
    observed: Option<Option<u32>>,
    asserted: Option<u32>,
    want: Option<u32>,
) -> bool {
    match observed {
        Some(live) => live != want,
        None => asserted != want,
    }
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
    /// Each tick reads the live `LinkStats`, folds it through both ladders, and —
    /// only when enabled — applies the results to the data plane. The snapshot is
    /// refreshed every tick (even when disabled) so the heartbeat surface stays
    /// current.
    pub async fn run(
        mut self,
        link: Arc<Mutex<LinkStats>>,
        proc: Arc<Mutex<RadioProcesses>>,
        snapshot: SnapshotHandle,
        counters: crate::watchdog::CounterHandle,
        cancel: Arc<Notify>,
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
                    self.tick(&link, &proc, &snapshot, &counters).await;
                }
                _ = cancel.notified() => {
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
        link: &Arc<Mutex<LinkStats>>,
        proc: &Arc<Mutex<RadioProcesses>>,
        snapshot: &SnapshotHandle,
        counters: &crate::watchdog::CounterHandle,
    ) {
        let (loss, rssi, snr, has_sample) = {
            let s = link.lock().await;
            // Cold-start: with no real sample yet (empty timestamp, 0 packets),
            // hold the rung so default sentinels never force a step-down. Same
            // guard the reactive-hop path uses for the drone-only-rig case.
            let has_sample = !s.timestamp.is_empty() && s.packets_received > 0;
            (s.loss_percent, s.rssi_dbm, s.snr_db, has_sample)
        };

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
            // No link sample. On a drone that is the permanent state, not a
            // cold start: it transmits its own downlink and a single radio in
            // monitor mode cannot capture its own injected frames, so
            // `packets_received` never leaves zero. Gating everything on a
            // sample therefore froze BOTH ladders for the whole flight — most
            // damagingly the step-down, so a link that was visibly over-fed had
            // no way to shed rate.
            //
            // Congestion needs no receiver. A transmit queue that stays deep
            // while the radio drains it says directly that more is being
            // offered than the air is carrying, so drive the bitrate ladder
            // from that instead. The modulation ladder stays parked, because
            // its input is SNR and there is no honest local substitute for it —
            // guessing a rung would risk raising the rate on a weak link.
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
        }

        self.reconcile_encoder_ceiling(want_ceiling, observed.as_ref())
            .await;
    }

    /// Publish the ladder's bitrate ceiling to `ados-video` when it differs from
    /// what is actually applied there. `observed` is the sidecar state already
    /// read this tick, so the steady-state cost is zero socket traffic.
    async fn reconcile_encoder_ceiling(
        &mut self,
        want: Option<u32>,
        observed: Option<&EncoderState>,
    ) {
        let live = observed.map(|s| s.ceiling_kbps);
        if !ceiling_needs_apply(live, self.asserted_ceiling_kbps, want) {
            return;
        }
        match ados_video::profile::set_bitrate_ceiling_at(&self.encoder_sock, want).await {
            Ok(state) => {
                self.asserted_ceiling_kbps = want;
                tracing::info!(
                    ceiling_kbps = ?want,
                    profile = state.profile.as_str(),
                    encoder_bitrate_kbps = state.bitrate_kbps,
                    "encoder_ceiling_applied"
                );
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    ceiling_kbps = ?want,
                    "encoder_ceiling_apply_failed"
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;

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

    /// The controller seeds its MCS ladder from the configured cap and the rung
    /// `wfb_tx` was actually spawned on, not from the ladder's own defaults.
    #[test]
    fn controller_seeds_the_mcs_ladder_from_config() {
        let ctrl = BitrateController::new(Arc::new(AtomicBool::new(true)), 5, 3);
        assert_eq!(ctrl.mcs.cap(), 5);
        assert_eq!(ctrl.mcs.current(), 3);
        // An out-of-range cap is clamped rather than trusted.
        let capped = BitrateController::new(Arc::new(AtomicBool::new(true)), 7, 1);
        assert_eq!(capped.mcs.cap(), mcs_ladder::LADDER_MAX_MCS);
    }

    /// The controller and `ados-video` agree at boot (both "no ceiling"), so the
    /// first tick must not issue a spurious publish.
    #[test]
    fn no_ceiling_publish_when_both_sides_start_unclamped() {
        assert!(!ceiling_needs_apply(None, None, None));
        assert!(!ceiling_needs_apply(Some(None), None, None));
    }

    /// A rung change publishes; a repeat of the same rung does not.
    #[test]
    fn ceiling_publishes_on_change_and_is_idempotent() {
        // Video reports 4000 live, ladder wants 1200 -> publish.
        assert!(ceiling_needs_apply(
            Some(Some(4000)),
            Some(4000),
            Some(1200)
        ));
        // Already 1200 live -> nothing to do, even mid-rung-change.
        assert!(!ceiling_needs_apply(
            Some(Some(1200)),
            Some(4000),
            Some(1200)
        ));
    }

    /// The self-heal: `ados-video` restarted and came back with the ceiling
    /// cleared while this controller still believes it published 1200. The
    /// published truth wins, so the next tick re-asserts.
    #[test]
    fn ceiling_reasserts_after_ados_video_restart_cleared_it() {
        assert!(ceiling_needs_apply(Some(None), Some(1200), Some(1200)));
    }

    /// With `ados-video` down (no sidecar) the controller falls back to its own
    /// last publish, so a dead service costs one attempt per rung change rather
    /// than one attempt every tick.
    #[test]
    fn ceiling_does_not_retry_every_tick_while_video_is_down() {
        // Same intent as last publish: hold.
        assert!(!ceiling_needs_apply(None, Some(1200), Some(1200)));
        // Intent changed: try once.
        assert!(ceiling_needs_apply(None, Some(1200), Some(2000)));
    }

    /// Disabling the ladder clears the clamp rather than pinning the encoder at
    /// rung 0 — the ceiling belongs to the ladder, and an unarmed ladder owns
    /// nothing.
    #[test]
    fn disabling_the_ladder_clears_the_ceiling() {
        assert!(ceiling_needs_apply(Some(Some(1200)), Some(1200), None));
        assert!(!ceiling_needs_apply(Some(None), None, None));
    }

    /// End-to-end over a real unix socket + a real sidecar file: a rung change
    /// reaches `ados-video`'s command socket with the ladder's bitrate, and the
    /// steady state that follows sends nothing.
    #[tokio::test]
    async fn reconcile_publishes_over_the_socket_then_goes_quiet() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("video-encoder.sock");
        let sidecar = dir.path().join("video-profile.json");
        let listener = tokio::net::UnixListener::bind(&sock).unwrap();

        // Stand in for ados-video: accept one request, echo an applied state.
        let seen = Arc::new(Mutex::new(Vec::<String>::new()));
        let seen_srv = seen.clone();
        let server = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let mut reader = BufReader::new(stream);
                let mut line = String::new();
                if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
                    continue;
                }
                seen_srv.lock().await.push(line.trim().to_string());
                let reply = br#"{"ok":true,"profile":"hero","ceiling_kbps":1200,"width":1280,"height":720,"fps":30,"bitrate_kbps":1200}
"#;
                let _ = reader.into_inner().write_all(reply).await;
            }
        });

        let mut ctrl =
            BitrateController::with_tiers(DEFAULT_TIERS.to_vec(), true, DEFAULT_TICK_INTERVAL)
                .with_encoder_paths(&sock, &sidecar);

        // No sidecar yet and nothing asserted: a 1200 ceiling must be published.
        ctrl.reconcile_encoder_ceiling(Some(1200), None).await;
        assert_eq!(ctrl.asserted_ceiling_kbps, Some(1200));
        {
            let s = seen.lock().await;
            assert_eq!(s.len(), 1, "exactly one publish");
            assert!(
                s[0].contains("video.encoder.ceiling.set") && s[0].contains("1200"),
                "unexpected request: {}",
                s[0]
            );
        }

        // ados-video now publishes 1200 as live: the next reconcile is silent.
        std::fs::write(
            &sidecar,
            br#"{"profile":"hero","ceiling_kbps":1200,"width":1280,"height":720,"fps":30,"bitrate_kbps":1200}"#,
        )
        .unwrap();
        let observed = ados_video::profile::read_state_from(&sidecar).expect("sidecar parses");
        ctrl.reconcile_encoder_ceiling(Some(1200), Some(&observed))
            .await;
        assert_eq!(seen.lock().await.len(), 1, "steady state must send nothing");

        server.abort();
    }

    /// `ados-video` unreachable: the reconcile must not panic, must not claim the
    /// ceiling was asserted, and must therefore retry on the next rung change.
    #[tokio::test]
    async fn reconcile_survives_an_unreachable_encoder() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("absent.sock");
        let sidecar = dir.path().join("absent.json");
        let mut ctrl =
            BitrateController::with_tiers(DEFAULT_TIERS.to_vec(), true, DEFAULT_TICK_INTERVAL)
                .with_encoder_paths(&sock, &sidecar);

        ctrl.reconcile_encoder_ceiling(Some(1200), None).await;
        assert_eq!(
            ctrl.asserted_ceiling_kbps, None,
            "a failed publish must not be recorded as asserted"
        );
        assert!(ceiling_needs_apply(
            None,
            ctrl.asserted_ceiling_kbps,
            Some(1200)
        ));
    }
}
