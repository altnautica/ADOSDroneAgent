//! The auto-pair re-arm latch: recover a rig holding a key that does not work,
//! without ever disturbing a rig holding one that does.
//!
//! ## The failure
//!
//! Auto-pair asked one question — "is a complete key file on disk?" — and
//! disarmed permanently when the answer was yes. A key left behind by a ground
//! station that was since reflashed answers yes. Two rigs sat unlinked for a
//! whole session on that: the drone held a structurally valid key from a peer
//! that no longer existed and injected into a void, its own radio reporting
//! `rf_unverified` (transmitting, zero confirmed reception); the reflashed
//! ground station had no key at all and blocked. Neither attempted recovery, and
//! every surface reported health.
//!
//! The signal that means "this key does not work" was already being computed and
//! already being surfaced. Nothing consumed it. This does.
//!
//! ## The rule
//!
//! Auto-pair may re-arm itself ONLY for a key whose fingerprint has never once
//! been confirmed to work. That single rule gives the three properties needed:
//!
//! - the stale key from a reflashed peer re-arms, because the peer that would
//!   have proven its fingerprint is gone;
//! - a healthy link never re-arms, because a proven fingerprint is latched for
//!   life — downtime, however long, cannot re-open a bind window on a pair that
//!   has worked;
//! - a successful re-bind writes a new key, whose different fingerprint resets
//!   the record to a clean lifetime.
//!
//! This decides only whether to OPEN a bind window. Whether that bind may then
//! declare a pair is untouched: the orchestrator's peer-evidence gate is still
//! the sole authority on success.
//!
//! ## Bounds
//!
//! A level-triggered confirm hold, a per-fingerprint episode budget and a
//! wall-clock cooldown, all persisted, so neither a reboot nor a crash loop
//! turns recovery into a bind storm. A bind already in flight suspends the
//! trigger, because a bind window is `rf_unverified` by construction and would
//! otherwise feed itself.

pub mod machine;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use ados_protocol::logd::emitter::EventEmitter;
use ados_protocol::logd::{Fields, Level, Value};
use ados_protocol::pair_proof::{
    load_for, write_pair_proof_to, PairProof, ProofStamp, PAIR_PROOF_PATH,
};

use crate::bind::BindRole;
use machine::{
    decide_rearm, drone_signals, gs_signals, HoldTrigger, RearmInput, RearmSignals, RearmStep,
    DEFAULT_MAX_REARM_EPISODES, DEFAULT_REARM_COOLDOWN_S, PAIR_REARM_KIND, REARM_CONFIRM_HOLD,
    STATS_FRESH_CEILING,
};

/// The radio's own stats sidecar — the source of both signals.
const WFB_STATS_PATH: &str = "/run/ados/wfb-stats.json";

/// Operator config, read from `video.wfb.pair_rearm`. Default-ON: the deadlock
/// this recovers from is silent and permanent, so it must not need enabling.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairRearmConfig {
    pub enabled: bool,
    pub confirm_hold: Duration,
    pub max_episodes: u32,
    pub cooldown_s: u64,
    pub stats_fresh_ceiling: Duration,
}

impl Default for PairRearmConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            confirm_hold: REARM_CONFIRM_HOLD,
            max_episodes: DEFAULT_MAX_REARM_EPISODES,
            cooldown_s: DEFAULT_REARM_COOLDOWN_S,
            stats_fresh_ceiling: STATS_FRESH_CEILING,
        }
    }
}

/// Parse `video.wfb.pair_rearm`. Absent / malformed → enabled defaults, matching
/// the sibling reconcilers: a config the agent cannot read must never silently
/// disable a self-heal.
pub fn read_config_from(text: &str) -> PairRearmConfig {
    #[derive(serde::Deserialize, Default)]
    struct Raw {
        #[serde(default)]
        video: VideoSec,
    }
    #[derive(serde::Deserialize, Default)]
    struct VideoSec {
        #[serde(default)]
        wfb: Option<WfbSec>,
    }
    #[derive(serde::Deserialize, Default)]
    struct WfbSec {
        #[serde(default)]
        pair_rearm: Option<Rearm>,
    }
    #[derive(serde::Deserialize)]
    struct Rearm {
        #[serde(default = "default_true")]
        enabled: bool,
        #[serde(default)]
        confirm_hold_s: Option<u64>,
        #[serde(default)]
        max_episodes: Option<u32>,
        #[serde(default)]
        cooldown_s: Option<u64>,
        #[serde(default)]
        stats_fresh_ceiling_s: Option<u64>,
    }
    fn default_true() -> bool {
        true
    }
    let raw = match serde_norway::from_str::<Raw>(text) {
        Ok(raw) => raw,
        Err(_) => return PairRearmConfig::default(),
    };
    match raw.video.wfb.and_then(|w| w.pair_rearm) {
        None => PairRearmConfig::default(),
        Some(r) => PairRearmConfig {
            enabled: r.enabled,
            confirm_hold: r
                .confirm_hold_s
                .map(|s| Duration::from_secs(s.max(1)))
                .unwrap_or(REARM_CONFIRM_HOLD),
            // Zero episodes would mean "never recover"; disabling is what
            // `enabled: false` is for, so the floor is one.
            max_episodes: r.max_episodes.unwrap_or(DEFAULT_MAX_REARM_EPISODES).max(1),
            cooldown_s: r.cooldown_s.unwrap_or(DEFAULT_REARM_COOLDOWN_S),
            stats_fresh_ceiling: r
                .stats_fresh_ceiling_s
                .map(|s| Duration::from_secs(s.max(1)))
                .unwrap_or(STATS_FRESH_CEILING),
        },
    }
}

/// What the latch wants recorded. Emitting is the caller's job so the decision
/// stays pure and a test can assert that a healthy rig produces NOTHING.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RearmEvent {
    /// A bind window is being opened for a key that has never worked.
    Armed { episode: u32, forced: bool },
    /// This key was confirmed working for the first time; it is latched now.
    Proven,
    /// Every episode for this key is spent and it still does not work.
    Exhausted { episodes: u32 },
    /// The record was discarded because the key on disk changed.
    Cleared,
}

impl RearmEvent {
    /// The bland wire string for the event's `state` field.
    pub fn state(&self) -> &'static str {
        match self {
            RearmEvent::Armed { .. } => "armed",
            RearmEvent::Proven => "proven",
            RearmEvent::Exhausted { .. } => "exhausted",
            RearmEvent::Cleared => "cleared",
        }
    }

    /// Severity: opening a bind window on a rig that believed it was paired, and
    /// giving up on one, are both conditions an operator should see; a proof or
    /// a key swap is informational.
    pub fn level(&self) -> Level {
        match self {
            RearmEvent::Armed { .. } | RearmEvent::Exhausted { .. } => Level::Warn,
            RearmEvent::Proven | RearmEvent::Cleared => Level::Info,
        }
    }
}

/// The decision for one tick.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LatchOutcome {
    /// Open a bind window now, overriding the paired + disarmed state.
    pub rearm: bool,
    pub step: RearmStep,
    /// At most one event per tick; `None` on the overwhelmingly common path.
    pub event: Option<RearmEvent>,
}

impl LatchOutcome {
    fn quiet(step: RearmStep) -> Self {
        Self {
            rearm: false,
            step,
            event: None,
        }
    }
}

/// Everything the latch needs from the outside world, read once by the caller so
/// [`PairProofLatch::step`] touches nothing but the record file.
#[derive(Debug, Clone)]
pub struct LatchInputs {
    /// The fingerprint of the complete key on disk; `None` when there is no
    /// complete key (in which case ordinary auto-pair already handles the rig).
    pub fingerprint: Option<String>,
    /// The radio's verdict, or `None` when there is no FRESH verdict.
    pub signals: Option<RearmSignals>,
    /// A bind is already running (ours, a REST call, or the CLI).
    pub session_active: bool,
    pub cfg: PairRearmConfig,
}

/// The re-arm latch. Owns the debounce and the record path; every other input is
/// injected.
pub struct PairProofLatch {
    role: BindRole,
    path: PathBuf,
    trigger: HoldTrigger,
    hold: Duration,
    /// `Exhausted` is a level that repeats every tick, so it is announced once
    /// and re-announced only after the state leaves it.
    exhausted_reported: bool,
}

impl PairProofLatch {
    /// A latch for `role` against the canonical record path.
    pub fn new(role: BindRole) -> Self {
        Self::with_path(role, PathBuf::from(PAIR_PROOF_PATH))
    }

    /// A latch against an explicit record path (tests).
    pub fn with_path(role: BindRole, path: PathBuf) -> Self {
        Self {
            role,
            path,
            trigger: HoldTrigger::with_hold(REARM_CONFIRM_HOLD),
            hold: REARM_CONFIRM_HOLD,
            exhausted_reported: false,
        }
    }

    /// Reset the debounce. Used whenever there is no verdict to accumulate: a
    /// stale sidecar, a bind in flight, a proof, a key swap. A stale read must
    /// RESET the hold, never freeze it part-way, or a radio that flaps in and out
    /// of reporting would eventually accumulate a full window it never actually
    /// held.
    fn release(&mut self, now: Instant) {
        self.trigger.observe(false, now);
    }

    /// One decision. Reads and writes only the record file; `now` / `now_unix`
    /// are injected so the whole contract is testable without a clock.
    pub fn step(&mut self, inputs: LatchInputs, now: Instant, now_unix: u64) -> LatchOutcome {
        if self.hold != inputs.cfg.confirm_hold {
            self.hold = inputs.cfg.confirm_hold;
            self.trigger = HoldTrigger::with_hold(self.hold);
        }
        if !inputs.cfg.enabled {
            self.release(now);
            return LatchOutcome::quiet(RearmStep::Idle);
        }
        // No complete key on disk: the rig is not "paired but broken", it is
        // simply unpaired, and ordinary auto-pair is already arming for it.
        let Some(fingerprint) = inputs.fingerprint.clone() else {
            self.release(now);
            return LatchOutcome::quiet(RearmStep::Idle);
        };

        let loaded = load_for(&self.path, self.role.as_str(), &fingerprint);
        let mut proof = loaded.proof;
        if loaded.reset {
            // The key changed under us. Persist the clean record, restart the
            // debounce (a new key deserves a fresh hold, not the previous key's
            // accumulated fault), and say so once.
            self.release(now);
            self.exhausted_reported = false;
            self.save(&proof);
            return LatchOutcome {
                rearm: false,
                step: RearmStep::Idle,
                event: Some(RearmEvent::Cleared),
            };
        }

        // A bind window is `rf_unverified` by construction, so a bind in flight
        // would feed the very trigger that opens one.
        if inputs.session_active {
            self.release(now);
            return LatchOutcome::quiet(RearmStep::Idle);
        }

        let mut event = None;
        let hold_armed = match inputs.signals {
            // No fresh verdict: nothing to accumulate.
            None => {
                self.release(now);
                false
            }
            Some(sig) if sig.proven => {
                self.release(now);
                match proof.mark_proven(now_unix) {
                    ProofStamp::First => {
                        self.save(&proof);
                        event = Some(RearmEvent::Proven);
                    }
                    ProofStamp::Refreshed => self.save(&proof),
                    ProofStamp::Unchanged => {}
                }
                false
            }
            Some(sig) => self.trigger.observe(sig.unproven, now),
        };

        let step = decide_rearm(RearmInput {
            proven: proof.is_proven(),
            forced: proof.force_rearm,
            hold_armed,
            episodes: proof.rearm_episodes,
            max_episodes: inputs.cfg.max_episodes,
            last_rearm_at: proof.last_rearm_at,
            cooldown_s: inputs.cfg.cooldown_s,
            now_unix,
        });

        if !matches!(step, RearmStep::Exhausted) {
            self.exhausted_reported = false;
        }

        let mut rearm = false;
        match step {
            RearmStep::Arm { episode, forced } => {
                // Spend the episode at ARM time, not on success: the budget
                // exists to bound attempts, and an attempt that fails is exactly
                // the one that must count.
                proof.record_rearm(now_unix);
                self.save(&proof);
                rearm = true;
                event = Some(RearmEvent::Armed { episode, forced });
            }
            // Exhausted is a level that repeats every tick, so it is announced
            // on the way in and not again until the state leaves it.
            RearmStep::Exhausted if !self.exhausted_reported => {
                self.exhausted_reported = true;
                event = Some(RearmEvent::Exhausted {
                    episodes: proof.rearm_episodes,
                });
            }
            _ => {}
        }

        LatchOutcome { rearm, step, event }
    }

    /// Persist the record; a write failure is logged and swallowed so a full or
    /// read-only filesystem never crashes the auto-pair loop. The cost of a lost
    /// write is a re-armed episode, not a wedged rig.
    fn save(&self, proof: &PairProof) {
        if let Err(e) = write_pair_proof_to(&self.path, proof) {
            tracing::warn!(error = %e, path = %self.path.display(), "pair_proof_persist_failed");
        }
    }
}

/// Build the `wfb.pair.rearm` detail map. Bland fields only.
pub fn rearm_detail(
    event: &RearmEvent,
    role: &str,
    fingerprint: &str,
    max_episodes: u32,
) -> Fields {
    let mut d = Fields::new();
    d.insert("state".to_string(), Value::from(event.state()));
    d.insert("role".to_string(), Value::from(role));
    d.insert("key_fingerprint".to_string(), Value::from(fingerprint));
    d.insert("max_episodes".to_string(), Value::from(max_episodes as u64));
    match event {
        RearmEvent::Armed { episode, forced } => {
            d.insert("episode".to_string(), Value::from(*episode as u64));
            d.insert("forced".to_string(), Value::from(*forced));
        }
        RearmEvent::Exhausted { episodes } => {
            d.insert("episode".to_string(), Value::from(*episodes as u64));
        }
        _ => {}
    }
    d
}

/// Ship one re-arm transition to the logging store. Best-effort: an absent
/// logging daemon drops it.
pub fn emit_rearm(
    events: &EventEmitter,
    event: &RearmEvent,
    role: &str,
    fingerprint: &str,
    max_episodes: u32,
) {
    events.emit(
        PAIR_REARM_KIND,
        event.level(),
        rearm_detail(event, role, fingerprint, max_episodes),
    );
}

/// Read the radio's fresh verdict for `role`, or `None` when there is none.
///
/// The freshness gate runs FIRST and on the file's mtime: the radio rewrites the
/// sidecar every ~1 s, so an older file means the writer stopped and its flags
/// are frozen. A frozen `rf_unverified` describes a radio that is not running,
/// and arming on it would re-bind a rig whose radio is merely stopped.
pub async fn read_signals(
    role: BindRole,
    path: &Path,
    fresh_ceiling: Duration,
) -> Option<RearmSignals> {
    let age = tokio::fs::metadata(path)
        .await
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| std::time::SystemTime::now().duration_since(t).ok());
    if age.map(|a| a > fresh_ceiling).unwrap_or(true) {
        return None;
    }
    let text = tokio::fs::read_to_string(path).await.ok()?;
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    parse_signals(role, &value)
}

/// Read the radio's fresh verdict from the canonical sidecar path.
pub async fn read_signals_default(role: BindRole, fresh_ceiling: Duration) -> Option<RearmSignals> {
    read_signals(role, Path::new(WFB_STATS_PATH), fresh_ceiling).await
}

/// Derive the signals from a parsed `wfb-stats.json` body. `None` when the
/// sidecar belongs to the OTHER profile — the two planes measure different
/// things and neither's rule transfers, so a profile mismatch is no verdict at
/// all rather than a misread one.
pub fn parse_signals(role: BindRole, value: &serde_json::Value) -> Option<RearmSignals> {
    let profile = value.get("profile").and_then(|v| v.as_str())?;
    match (role, profile) {
        (BindRole::Drone, "drone") => Some(drone_signals(
            value.get("rf_unverified").and_then(|v| v.as_bool()),
            value.get("channel_locked").and_then(|v| v.as_bool()),
        )),
        (BindRole::Gs, "ground_station") => Some(gs_signals(
            value.get("state").and_then(|v| v.as_str()).unwrap_or(""),
            value
                .get("packets_received")
                .and_then(|v| v.as_u64())
                .unwrap_or(0),
        )),
        _ => None,
    }
}

#[cfg(test)]
mod tests;
