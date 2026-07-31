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
    /// Backoff state for failed ceiling publishes. See [`CeilingRetry`].
    ceiling_retry: CeilingRetry,
    encoder_sock: PathBuf,
    encoder_sidecar: PathBuf,
}

/// Shortest and longest gap between retries of a failed encoder-ceiling publish.
const CEILING_RETRY_MIN: Duration = Duration::from_secs(2);
const CEILING_RETRY_MAX: Duration = Duration::from_secs(60);

/// One warning per this many consecutive failures, after the first.
///
/// At the capped retry gap that is roughly one line every half hour: quiet
/// enough to live with for the life of a process, frequent enough that the
/// fault is still on the record.
const CEILING_LOG_EVERY: u32 = 30;

/// When to retry publishing the encoder ceiling after a failed attempt.
///
/// Without this the controller retried at the tick rate, which is once a second,
/// forever. The path that made it permanent is a node with no encoder at all:
/// nothing publishes an encoder state, so there is no observed ceiling to
/// reconcile against and the fallback compares against the last SUCCESSFUL
/// publish — which never happens, because every attempt fails. So the
/// needs-apply test was true on every tick from boot, and one absent socket
/// became a warning every second for as long as the node ran.
///
/// The answer is not to stop trying. An encoder that starts late, or comes back
/// after a crash, has to get its ceiling. So the attempt itself backs off
/// exponentially to a minute, and the log decays with it rather than the fault
/// going silent: loud on the first failure, then one line per
/// [`CEILING_LOG_EVERY`] failures carrying the running total, so an operator
/// reading the log an hour later still learns that this has been failing the
/// whole time and how many times.
///
/// A change of intent — a rung step, or the ladder being disarmed — clears the
/// backoff. That is new information the encoder has not been told yet, and it
/// preserves the original contract of one attempt per rung change.
#[derive(Debug, Default)]
struct CeilingRetry {
    /// Consecutive failed attempts since the last success. Not reset by a
    /// change of intent: it is the count of how long this has been broken.
    failures: u32,
    /// Earliest instant the next attempt may run, while backing off.
    next_attempt: Option<Instant>,
    /// The ceiling the last attempt tried to publish, so a change of intent can
    /// be told from a repeat of the same one.
    attempted: Option<Option<u32>>,
}

impl CeilingRetry {
    /// Whether an attempt to publish `want` may run now.
    fn should_attempt(&self, want: Option<u32>, now: Instant) -> bool {
        // New intent always gets an immediate attempt.
        if self.attempted != Some(want) {
            return true;
        }
        self.next_attempt.map(|at| now >= at).unwrap_or(true)
    }

    /// Record a failed attempt and schedule the next one.
    fn record_failure(&mut self, want: Option<u32>, now: Instant) {
        self.failures = self.failures.saturating_add(1);
        self.attempted = Some(want);
        // Exponential from CEILING_RETRY_MIN, capped. `saturating_mul` keeps a
        // long-running failure from overflowing the shift.
        let factor = 1u32
            .checked_shl(self.failures.saturating_sub(1))
            .unwrap_or(u32::MAX);
        let gap = CEILING_RETRY_MIN
            .saturating_mul(factor)
            .min(CEILING_RETRY_MAX);
        self.next_attempt = Some(now + gap);
    }

    /// Record a successful attempt, returning how many failures it ended.
    fn record_success(&mut self, want: Option<u32>) -> u32 {
        let recovered_from = self.failures;
        self.failures = 0;
        self.next_attempt = None;
        self.attempted = Some(want);
        recovered_from
    }

    /// Whether this failure count warrants a warning rather than a debug line.
    fn should_warn(failures: u32) -> bool {
        failures == 1 || failures.is_multiple_of(CEILING_LOG_EVERY)
    }
}

/// Where this tick's link measurement came from.
///
/// Surfaced on the snapshot so an operator can tell a rung chosen from a real
/// measurement apart from one chosen from congestion or held for want of any
/// signal at all — the three cases previously looked identical from outside.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SampleSource {
    /// Measured by this node's own receiver. Authoritative for a node that has
    /// one (a ground station receiving video).
    Local,
    /// Reported by the peer that receives our transmission. The only honest
    /// measurement available to a transmit-only node.
    Peer,
    /// No usable measurement this tick.
    None,
}

impl SampleSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Peer => "peer",
            Self::None => "none",
        }
    }
}

/// One tick's resolved link measurement.
pub struct ResolvedSample {
    pub loss_percent: f64,
    pub rssi_dbm: f64,
    pub snr_db: f64,
    pub source: SampleSource,
}

impl ResolvedSample {
    /// Whether the ladders may act on this sample.
    pub fn has_sample(&self) -> bool {
        self.source != SampleSource::None
    }
}

/// Pick this tick's measurement.
///
/// A node that measures its own link is authoritative for itself, so a real
/// local sample always wins. A transmit-only node has no local sample by
/// construction — a single radio in monitor mode cannot capture its own
/// injected frames — so it falls back to what the receiving peer reported.
///
/// The peer sample must be BOTH fresh and an actual measurement. Feedback most
/// often stops because the link got worse, so treating an old report as current
/// would hold the rate high at exactly the moment it should fall; and a peer
/// that heard nothing is reporting deafness, not a clean link.
pub fn resolve_sample(
    local: &LinkStats,
    peer: Option<&ados_protocol::link_feedback::LinkFeedbackSidecar>,
    now_unix_ms: u64,
) -> ResolvedSample {
    let local_real = !local.timestamp.is_empty() && local.packets_received > 0;
    if local_real {
        return ResolvedSample {
            loss_percent: local.loss_percent,
            rssi_dbm: local.rssi_dbm,
            snr_db: local.snr_db,
            source: SampleSource::Local,
        };
    }
    if let Some(p) = peer {
        if p.is_usable_at(now_unix_ms) {
            return ResolvedSample {
                loss_percent: p.loss_percent,
                rssi_dbm: p.rssi_dbm,
                snr_db: p.snr_db,
                source: SampleSource::Peer,
            };
        }
    }
    ResolvedSample {
        loss_percent: local.loss_percent,
        rssi_dbm: local.rssi_dbm,
        snr_db: local.snr_db,
        source: SampleSource::None,
    }
}

/// Current wall clock in epoch milliseconds, for the freshness comparison.
fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
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
        // Cold-start: with no real sample yet (empty timestamp, 0 packets), hold
        // the rung so default sentinels never force a step-down. Same guard the
        // reactive-hop path uses for the drone-only-rig case.
        //
        // A transmit-only node never leaves that sentinel, which used to freeze
        // BOTH ladders for the whole flight — most damagingly the step-down, so
        // an over-fed link had no way to shed rate. The receiving peer does
        // measure the link and reports it on the aux lane, so fall back to that
        // sample when there is no local one.
        // Read the peer report BEFORE taking the link lock: it is blocking file
        // I/O, and the link mutex is on the receive path's hot loop. Holding it
        // across a disk read would make a slow filesystem a receive stall.
        let peer = ados_protocol::link_feedback::read_sidecar_from(
            &ados_protocol::link_feedback::sidecar_path(),
        );
        let sample = {
            let s = link.lock().await;
            resolve_sample(&s, peer.as_ref(), now_unix_ms())
        };
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
            snap.sample_source = sample.source.as_str();
            // Only a real sample carries a loss figure. Without a sample the
            // number in `sample` is the local sentinel, which on a drone is
            // permanently zero and would read as a clean link.
            snap.sample_loss_percent = has_sample.then_some(loss);
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
        // On a node with no encoder the check above is true on every tick
        // forever, because only a SUCCESSFUL publish advances what it compares
        // against. Backing the attempt off is what stops that from being a
        // warning every second for the life of the process, without the
        // controller giving up on an encoder that has not started yet.
        let now = Instant::now();
        if !self.ceiling_retry.should_attempt(want, now) {
            return;
        }
        match ados_video::profile::set_bitrate_ceiling_at(&self.encoder_sock, want).await {
            Ok(state) => {
                self.asserted_ceiling_kbps = want;
                let recovered_from = self.ceiling_retry.record_success(want);
                tracing::info!(
                    ceiling_kbps = ?want,
                    profile = state.profile.as_str(),
                    encoder_bitrate_kbps = state.bitrate_kbps,
                    recovered_after_failures = recovered_from,
                    "encoder_ceiling_applied"
                );
            }
            Err(e) => {
                self.ceiling_retry.record_failure(want, now);
                let failures = self.ceiling_retry.failures;
                // Loud the first time, then one line per CEILING_LOG_EVERY
                // carrying the running total, so the fault stays on the record
                // without filling it.
                if CeilingRetry::should_warn(failures) {
                    tracing::warn!(
                        error = %e,
                        ceiling_kbps = ?want,
                        consecutive_failures = failures,
                        "encoder_ceiling_apply_failed"
                    );
                } else {
                    tracing::debug!(
                        error = %e,
                        ceiling_kbps = ?want,
                        consecutive_failures = failures,
                        "encoder_ceiling_apply_failed"
                    );
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ados_protocol::link_feedback::{LinkFeedback, LinkFeedbackSidecar};
    use std::sync::atomic::AtomicBool;

    /// What a transmit-only drone's own stats look like: the permanent
    /// no-measurement sentinel, because its radio cannot hear itself.
    fn transmit_only_sentinel() -> LinkStats {
        LinkStats::default()
    }

    fn ground_measured() -> LinkStats {
        LinkStats {
            packets_received: 485,
            loss_percent: 3.0,
            rssi_dbm: -46.0,
            snr_db: 20.0,
            timestamp: "2026-07-31T12:00:00Z".to_string(),
            ..LinkStats::default()
        }
    }

    fn peer_report(loss: f64, at_ms: u64) -> LinkFeedbackSidecar {
        LinkFeedbackSidecar::stamped(
            &LinkFeedback {
                loss_percent: loss,
                rssi_dbm: -36.0,
                snr_db: 12.4,
                packets_received: 485,
                fec_failed: 25,
                bitrate_kbps: 2242,
                has_measurement: true,
                target_slot: 1,
            },
            at_ms,
        )
    }

    #[test]
    fn a_transmit_only_node_uses_the_peers_report() {
        // The regression this guards is the whole reason the contract exists:
        // with only the local sentinel the ladder had no sample, both ladders
        // froze, and an over-fed link could never shed rate.
        let peer = peer_report(24.29, 10_000);
        let s = resolve_sample(&transmit_only_sentinel(), Some(&peer), 10_500);
        assert_eq!(s.source, SampleSource::Peer);
        assert!(s.has_sample(), "the ladder must be able to act");
        assert!((s.loss_percent - 24.29).abs() < 0.01);
    }

    #[test]
    fn a_local_measurement_beats_a_peer_report() {
        // A node with its own receiver is authoritative for its own link.
        let peer = peer_report(99.0, 10_000);
        let s = resolve_sample(&ground_measured(), Some(&peer), 10_500);
        assert_eq!(s.source, SampleSource::Local);
        assert_eq!(s.loss_percent, 3.0);
    }

    #[test]
    fn a_stale_peer_report_is_not_a_sample() {
        // Feedback usually stops because the link got WORSE. Holding the last
        // good report would keep the rate high exactly when it should fall.
        let peer = peer_report(2.0, 10_000);
        let s = resolve_sample(&transmit_only_sentinel(), Some(&peer), 99_000);
        assert_eq!(s.source, SampleSource::None);
        assert!(!s.has_sample());
    }

    #[test]
    fn a_peer_that_heard_nothing_is_not_a_clean_link() {
        let deaf = LinkFeedbackSidecar::stamped(
            &LinkFeedback {
                loss_percent: 0.0,
                rssi_dbm: -100.0,
                snr_db: 0.0,
                packets_received: 0,
                fec_failed: 0,
                bitrate_kbps: 0,
                has_measurement: false,
                target_slot: 1,
            },
            10_000,
        );
        let s = resolve_sample(&transmit_only_sentinel(), Some(&deaf), 10_100);
        assert_eq!(
            s.source,
            SampleSource::None,
            "a deaf receiver reporting 0% loss must not read as a perfect link"
        );
    }

    #[test]
    fn no_peer_report_at_all_leaves_the_node_without_a_sample() {
        let s = resolve_sample(&transmit_only_sentinel(), None, 10_000);
        assert_eq!(s.source, SampleSource::None);
        assert!(
            !s.has_sample(),
            "falls through to the congestion path, not to a fabricated sample"
        );
    }

    #[test]
    fn the_source_is_reportable_so_a_held_rung_is_explainable() {
        assert_eq!(SampleSource::Local.as_str(), "local");
        assert_eq!(SampleSource::Peer.as_str(), "peer");
        assert_eq!(SampleSource::None.as_str(), "none");
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

    /// The gap the existing "does not retry every tick" test could not cover.
    ///
    /// It only exercised `asserted == Some(_)`, i.e. a controller that had
    /// published successfully at least once. A node with NO encoder never gets
    /// there: `asserted` stays `None`, so `ceiling_needs_apply(None, None,
    /// Some(x))` is true on every tick from boot, and the failed publish it
    /// triggers logged a warning once a second for the life of the process.
    #[test]
    fn a_node_that_has_never_published_still_wants_to_apply_every_tick() {
        // This is the condition the backoff exists to survive; it is correct
        // (the ceiling really has not been delivered) and it never clears on
        // its own, which is exactly why the ATTEMPT has to be rate-limited
        // rather than the check changed.
        assert!(ceiling_needs_apply(None, None, Some(4000)));
    }

    #[test]
    fn a_node_with_no_encoder_does_not_attempt_once_a_second_forever() {
        // Five minutes of 1 Hz ticks against an encoder that never answers.
        // Before the backoff this was 300 attempts and 300 warnings.
        let mut retry = CeilingRetry::default();
        let t0 = Instant::now();
        let mut attempts = 0u32;
        for tick in 0..300u64 {
            let now = t0 + Duration::from_secs(tick);
            if retry.should_attempt(Some(4000), now) {
                attempts += 1;
                retry.record_failure(Some(4000), now);
            }
        }
        assert!(
            attempts <= 15,
            "backed-off retries over five minutes should be a handful, got {attempts}"
        );
        // But it must NOT give up: an encoder that starts late still has to be
        // told the ceiling, so the retries keep coming at the capped rate.
        assert!(
            attempts >= 5,
            "the controller must keep trying, got only {attempts}"
        );
    }

    #[test]
    fn the_failure_is_still_reported_just_not_every_second() {
        // The signal must survive the rate limit. Loud on the first failure,
        // then periodically with the running total, never silent.
        assert!(
            CeilingRetry::should_warn(1),
            "the first failure must be loud"
        );
        assert!(!CeilingRetry::should_warn(2));
        assert!(!CeilingRetry::should_warn(29));
        assert!(
            CeilingRetry::should_warn(CEILING_LOG_EVERY),
            "a persistent failure must keep surfacing"
        );
        assert!(CeilingRetry::should_warn(CEILING_LOG_EVERY * 4));
    }

    #[test]
    fn a_rung_change_is_attempted_at_once_despite_the_backoff() {
        // The backoff must not delay NEW information. A rung step is something
        // the encoder has not been told yet, so it goes out immediately.
        let mut retry = CeilingRetry::default();
        let t0 = Instant::now();
        retry.record_failure(Some(4000), t0);
        assert!(
            !retry.should_attempt(Some(4000), t0),
            "the same intent waits for the backoff"
        );
        assert!(
            retry.should_attempt(Some(2000), t0),
            "a rung change must not wait"
        );
        // Disarming the ladder clears the clamp, which is also new intent.
        assert!(retry.should_attempt(None, t0));
    }

    #[test]
    fn a_successful_publish_clears_the_backoff_and_reports_the_outage() {
        let mut retry = CeilingRetry::default();
        let t0 = Instant::now();
        retry.record_failure(Some(4000), t0);
        retry.record_failure(Some(4000), t0);
        assert_eq!(
            retry.record_success(Some(4000)),
            2,
            "recovery must say how long it had been failing"
        );
        assert!(retry.should_attempt(Some(4000), t0), "backoff is cleared");
        assert_eq!(retry.failures, 0);
    }

    #[test]
    fn the_retry_gap_grows_but_stays_bounded() {
        // Bounded so a late encoder is picked up within a minute, not hours.
        let mut retry = CeilingRetry::default();
        let t0 = Instant::now();
        for _ in 0..40 {
            retry.record_failure(Some(4000), t0);
        }
        assert!(
            retry.should_attempt(Some(4000), t0 + CEILING_RETRY_MAX),
            "the gap must cap, not grow without bound"
        );
        assert!(!retry.should_attempt(Some(4000), t0 + Duration::from_secs(1)));
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
