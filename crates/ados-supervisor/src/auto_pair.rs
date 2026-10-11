//! Local-radio auto-pair: drive the first-boot WFB bind IN-PROCESS.
//!
//! This lives in the supervisor, not the cloud relay where it used to run. The
//! bind FSM stops + starts the `ados-wfb` unit to flip wfb-ng profiles, so the
//! trigger cannot live in the radio service (it would kill itself). The earlier
//! home was `ados-cloud` only because "the cloud relay does not touch the
//! radio" — but that is a LAN-only function parked in a service that is idle in
//! local-first mode (cloud relay off by default). The supervisor is the unit
//! that does the stopping (never stopped itself), already owns the
//! [`BindOrchestrator`], and always runs, so it is the correct host. The bind
//! runs in-process via the orchestrator directly, with no control-socket
//! round-trip.
//!
//! Each tick, while armed (`video.wfb.auto_pair_enabled`) and not already paired
//! (the role's key file is absent), run one bind. A successful bind writes the
//! key file, so the next tick sees the rig as paired and stops attempting; the
//! key-apply step also flips `auto_pair_enabled` to false in the config.
//!
//! "Already paired" is a file-shape check, and a key left behind by a peer that
//! was since reflashed is a perfectly well-formed key — which is how two rigs
//! once sat unlinked for a whole session with every surface reporting health.
//! The [`crate::pair_proof`] latch is the second opinion: it watches the radio's
//! own verdict and, for a key whose fingerprint has NEVER been confirmed to
//! work, opens a bind window despite the paired-and-disarmed state. A key that
//! has ever worked is latched for life and is never re-armed.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use ados_protocol::logd::emitter::EventEmitter;
use tokio::sync::watch;

use crate::bind::orchestrator::{BindOrchestrator, BindStartError};
use crate::bind::BindRole;

mod failover;
use failover::emit_failover;
#[cfg(test)]
use failover::WFB_FAILOVER_SIDECAR_VERSION;
pub use failover::{FailoverState, FailoverWriter};

/// The operator config the arm flag is read from each tick.
const CONFIG_YAML: &str = "/etc/ados/config.yaml";
/// Settle delay before the first attempt (lets the radio + units come up).
pub const START_DELAY: Duration = Duration::from_secs(15);
/// Base backoff between local-bind attempts.
///
/// Kept SHORT on purpose. The WFB bind is a two-party RENDEZVOUS: the drone's
/// bind tunnel only opens while the ground station's bind server is
/// simultaneously beaconing on the bind channel. Each rig runs this loop
/// independently — a bind session, then a backoff — so a LONG backoff on either
/// side means their active windows drift out of phase and never coincide. Two
/// rigs side-by-side then never bind even though both radios are healthy (the
/// windows are simply never open at the same time). This was masked when both
/// rigs cold-boot together (their loops start aligned), but any rig restarted /
/// re-installed later drifts out of phase forever.
///
/// A short backoff keeps both rigs in an active bind session almost all the
/// time, so their windows overlap within a cycle regardless of when each
/// started — no shared clock or coordination needed (the two rigs' clocks are
/// not synchronised on an offline bench). [`RETRY_BACKOFF_JITTER_SECS`] adds
/// randomness so two rigs looping with identical timing cannot stay phase-locked
/// apart.
pub const RETRY_BACKOFF: Duration = Duration::from_secs(3);
/// Upper bound (seconds) on the random jitter added to [`RETRY_BACKOFF`] before
/// each retry sleep, breaking any phase lock between two independently-looping
/// rigs. Actual jitter is a uniform `0..=RETRY_BACKOFF_JITTER_SECS`.
pub const RETRY_BACKOFF_JITTER_SECS: u64 = 4;

/// The sleep before the next local-bind attempt: [`RETRY_BACKOFF`] plus a random
/// `0..=RETRY_BACKOFF_JITTER_SECS` seconds of jitter. Pure-ish (reads the OS RNG;
/// a best-effort fallback to zero jitter keeps the loop alive if the RNG hiccups,
/// which it never does on the target).
fn retry_backoff() -> Duration {
    let mut b = [0u8; 1];
    let jitter = getrandom::fill(&mut b)
        .map(|_| (b[0] as u64) % (RETRY_BACKOFF_JITTER_SECS + 1))
        .unwrap_or(0);
    RETRY_BACKOFF + Duration::from_secs(jitter)
}

/// Consecutive failed bind attempts after which a rig with a cloud relay
/// configured tells the operator to fall back to it (the `cloud_relay` sidecar
/// state). It is a signal, not a stop: the local bind keeps running on its fixed
/// retry interval, because the peer it is waiting for may be powered on at any
/// time and a rig that stopped looking needs a human to start it again.
pub const MAX_LOCAL_BIND_ATTEMPTS: u32 = 10;

/// Whether auto-pair should attempt a bind this tick. Pure for testing.
pub fn should_attempt(armed: bool, already_paired: bool) -> bool {
    armed && !already_paired
}

/// Whether a bind window opens this tick, folding in the re-arm latch.
///
/// The latch deliberately overrides BOTH gates: a rig holding a key that has
/// never worked is paired by the file check and disarmed by the config, and
/// those are exactly the two states that left it stuck. The latch's own hold
/// and cooldown are what bound it. Pure for testing.
pub fn should_attempt_or_rearm(armed: bool, already_paired: bool, rearm: bool) -> bool {
    should_attempt(armed, already_paired) || rearm
}

/// Whether the loop should run a bind given the adapter presence. A dongle-less
/// boot (no injection-capable WFB adapter) must NOT run a bind: it would fail,
/// count toward the cloud-relay signal, and tell the operator to fall back even
/// though they simply have not plugged the radio in yet. Skipping without
/// counting lets the rig keep waiting for the adapter indefinitely and bind the
/// moment it appears. Pure for testing.
pub fn adapter_ready_to_bind(wfb_adapter_present: bool) -> bool {
    wfb_adapter_present
}

/// Decide whether the attempt count has reached the cap at which a rig with a
/// cloud relay configured signals the fallback. Pure for testing.
pub fn failover_reached(attempt: u32) -> bool {
    attempt >= MAX_LOCAL_BIND_ATTEMPTS
}

/// What the failover sidecar should say after a failed local bind: the cloud
/// relay once the cap is reached on a node that has one configured, nothing new
/// otherwise. A local-first node has no relay to fall back to, so it never
/// signals one. Pure for testing.
pub fn failover_after_failure(attempt: u32, cloud_relay_enabled: bool) -> Option<FailoverState> {
    (cloud_relay_enabled && failover_reached(attempt)).then_some(FailoverState::CloudRelay)
}

/// Whether a bind runs this tick. The cloud-relay signal deliberately does not
/// appear here: signalling the fallback never stops the local loop.
///
/// A re-arm window stops the normal radio unit, so it opens only while the FC
/// is provably disarmed (`fc_armed == Some(false)`); an armed or unknown FC
/// keeps the video link up. The explicit arm-flag path is not gated: it only
/// fires on an unpaired rig, which has no link to lose. Pure for testing.
pub fn bind_this_tick(
    armed: bool,
    already_paired: bool,
    rearm: bool,
    fc_armed: Option<bool>,
    adapter_present: bool,
) -> bool {
    let rearm = rearm && matches!(fc_armed, Some(false));
    should_attempt_or_rearm(armed, already_paired, rearm) && adapter_ready_to_bind(adapter_present)
}

/// Apply an operator local-retry request: restart the attempt count so the
/// cloud-relay signal is re-earned from zero. Returns whether a request was
/// applied. Pure for testing.
pub fn apply_retry_request(requested: bool, attempt: &mut u32) -> bool {
    if requested {
        *attempt = 0;
    }
    requested
}

/// Parse `video.wfb.auto_pair_enabled` out of a config body. Default TRUE: a
/// fresh rig auto-binds on first boot unless the operator has explicitly
/// disarmed it, and a successful bind disarms it. An absent `video.wfb` section
/// therefore reads as armed, which also keeps this in step with the Python
/// config model's default (the two diverged before, so the status display read
/// "armed" while the supervisor was silently disarmed).
pub fn read_armed_from(text: &str) -> bool {
    #[derive(serde::Deserialize)]
    struct Raw {
        #[serde(default)]
        video: VideoSec,
    }
    #[derive(serde::Deserialize, Default)]
    struct VideoSec {
        #[serde(default)]
        wfb: Option<WfbSec>,
    }
    #[derive(serde::Deserialize)]
    struct WfbSec {
        #[serde(default = "default_true")]
        auto_pair_enabled: bool,
    }
    fn default_true() -> bool {
        true
    }
    match serde_norway::from_str::<Raw>(text) {
        Ok(raw) => raw.video.wfb.map(|w| w.auto_pair_enabled).unwrap_or(true),
        Err(_) => true,
    }
}

/// Read the arm flag off disk; any read/parse failure keeps the default-armed
/// posture so a transient fs hiccup never silently disables first-boot pairing.
fn read_armed() -> bool {
    match std::fs::read_to_string(CONFIG_YAML) {
        Ok(text) => read_armed_from(&text),
        Err(_) => true,
    }
}

/// The fingerprint of this role's key file, or `None` when there is no complete
/// key.
///
/// This answers BOTH questions the loop asks about the key, from one read.
/// `Some` means paired: `read_public_fingerprint` succeeds only on a file that
/// is exactly 64 bytes (the libsodium crypto_box size) with a hashable
/// peer-public half, so a half-written key (power loss mid-bind, a truncated
/// copy) reads as unpaired and the rig re-runs its own auto-bind instead of
/// skipping forever on a torn file. And the value itself is the identity the
/// re-arm latch keys its proof record on, so the two can never disagree about
/// which key is on disk.
fn key_fingerprint(role: BindRole) -> Option<String> {
    key_fingerprint_at(Path::new(role.key_path()))
}

/// [`key_fingerprint`] against an explicit path, so the completeness contract is
/// unit-testable against a temp file without a real role key path.
fn key_fingerprint_at(path: &Path) -> Option<String> {
    crate::bind::keys::read_public_fingerprint(path).ok()
}

/// Wall-clock seconds, for the latch's reboot-surviving cooldown anchor.
fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Step the re-arm latch for this tick and return whether a bind window opens.
///
/// Everything the latch needs from the outside world is read here — the config,
/// the radio's fresh verdict, whether a bind is already running — so the latch
/// itself touches nothing but its record file and stays testable without a rig.
/// The event is emitted here too, for the same reason.
async fn run_latch(
    latch: &mut crate::pair_proof::PairProofLatch,
    role: BindRole,
    fingerprint: Option<String>,
    orch: &Arc<BindOrchestrator>,
    events: &EventEmitter,
) -> bool {
    use crate::pair_proof::{
        emit_rearm, read_config_from, read_signals_default, LatchInputs, PairRearmConfig,
    };

    let cfg = match std::fs::read_to_string(CONFIG_YAML) {
        Ok(text) => read_config_from(&text),
        Err(_) => PairRearmConfig::default(),
    };
    // Only read the radio when there is something to judge: no key on disk means
    // ordinary auto-pair already owns the rig.
    let signals = if cfg.enabled && fingerprint.is_some() {
        read_signals_default(role, cfg.stats_fresh_ceiling).await
    } else {
        None
    };
    let outcome = latch.step(
        LatchInputs {
            fingerprint: fingerprint.clone(),
            signals,
            session_active: orch.session_active(),
            cfg,
        },
        std::time::Instant::now(),
        now_unix(),
    );
    if let Some(event) = &outcome.event {
        let fp = fingerprint.as_deref().unwrap_or("");
        tracing::warn!(
            role = role.as_str(),
            state = event.state(),
            "auto_pair_rearm_transition"
        );
        emit_rearm(events, event, role.as_str(), fp);
    }
    outcome.rearm
}

/// Run the auto-pair loop until `shutdown` flips. Drives the bind in-process via
/// the shared orchestrator. `cloud_relay_enabled` reflects `server.mode`: in
/// local-first mode the loop never gives up on the local bind.
pub async fn run(
    orch: Arc<BindOrchestrator>,
    role: BindRole,
    mut shutdown: watch::Receiver<bool>,
    cloud_relay_enabled: bool,
) {
    run_with_failover(
        orch,
        role,
        &mut shutdown,
        FailoverWriter::new(),
        cloud_relay_enabled,
        EventEmitter::new("ados-supervisor"),
    )
    .await
}

/// The loop body with an injectable failover writer (so a test can target a temp
/// path). Resets the failover sidecar to `local` on start, drives one bind per
/// armed+unpaired tick forever, flips the sidecar to `cloud_relay` after the
/// attempt cap *only when a cloud relay is configured*, and persists `local`
/// again on a successful pair.
async fn run_with_failover(
    orch: Arc<BindOrchestrator>,
    role: BindRole,
    shutdown: &mut watch::Receiver<bool>,
    mut failover: FailoverWriter,
    cloud_relay_enabled: bool,
    events: EventEmitter,
) {
    tracing::info!(role = role.as_str(), "auto_pair_supervisor_started");
    // Settle before the first attempt.
    tokio::select! {
        _ = shutdown.changed() => return,
        _ = tokio::time::sleep(START_DELAY) => {}
    }
    // A fresh run resets the failover sidecar to `local`: an operator who re-armed
    // auto-pair from the cloud-relay fallback should see the rig retry local bind.
    // Emit the transition only when the sidecar actually changed (a deduped no-op
    // write does not spam the store).
    if failover.sync(FailoverState::Local) {
        emit_failover(&events, FailoverState::Local);
    }
    // Consecutive failed bind attempts; reaching the cap signals the cloud
    // relay (it never stops the loop).
    let mut attempt: u32 = 0;
    // The per-key proof latch: the only thing that can re-open a bind window on
    // a rig this loop otherwise considers finished.
    let mut latch = crate::pair_proof::PairProofLatch::new(role);
    loop {
        if *shutdown.borrow() {
            break;
        }
        // The operator's "retry local" request: the cloud-relay signal goes
        // back to local and the attempt count restarts from zero.
        let retry_requested = ados_protocol::pair_proof::take_local_retry_request_at(
            std::path::Path::new(ados_protocol::pair_proof::AUTO_PAIR_RETRY_PATH),
        );
        if apply_retry_request(retry_requested, &mut attempt) {
            tracing::info!(role = role.as_str(), "auto_pair_local_retry_requested");
            if failover.sync(FailoverState::Local) {
                emit_failover(&events, FailoverState::Local);
            }
        }
        let armed = read_armed();
        let fingerprint = key_fingerprint(role);
        let paired = fingerprint.is_some();
        // A dongle-less boot must not run (and fail) a bind: that would count
        // toward the cloud-relay signal when the operator has simply not plugged
        // the radio in yet. Skip without counting so the rig keeps waiting and
        // binds the moment the adapter appears.
        let adapter_present = adapter_ready_to_bind(crate::hardware::has_wfb_adapter());
        if should_attempt(armed, paired) && !adapter_present {
            tracing::info!(role = role.as_str(), "auto_pair_waiting_for_wfb_adapter");
        }
        // Ask the latch only when a bind could actually run. Stepping it without
        // an adapter would spend an episode on a window that cannot open.
        let rearm = if adapter_present {
            run_latch(&mut latch, role, fingerprint.clone(), &orch, &events).await
        } else {
            false
        };
        // The re-arm path is gated on a disarmed FC; only read the state socket
        // when the latch actually asks for a window.
        let fc_armed = if rearm {
            crate::fc_state::read_fc_armed().await
        } else {
            None
        };
        if rearm && fc_armed != Some(false) {
            tracing::info!(
                role = role.as_str(),
                ?fc_armed,
                "auto_pair_rearm_deferred_fc_not_disarmed"
            );
        }
        if bind_this_tick(armed, paired, rearm, fc_armed, adapter_present) {
            // Tear down an in-flight bind if the supervisor shuts down.
            let mut cancel_rx = shutdown.clone();
            let cancel = async move {
                let _ = cancel_rx.changed().await;
            };
            // No fleet assignment: an auto re-bind is unattended, so there is no
            // operator round-trip to the ground station's registry to source one
            // from. `None` leaves whatever slot the config already carries, which
            // is the right outcome for a rig re-binding to the same fleet.
            match orch
                .start_local_bind(role, None, "auto", None, cancel)
                .await
            {
                Ok(session) => {
                    let state = session.get("state").and_then(|s| s.as_str()).unwrap_or("");
                    if state == "paired" {
                        tracing::info!(role = role.as_str(), "auto_pair_bind_completed");
                        // Stay on the local path; the key-apply step also disarms
                        // auto-pair so the next tick sees the rig as paired.
                        if failover.sync(FailoverState::Local) {
                            emit_failover(&events, FailoverState::Local);
                        }
                        // Keep the loop alive rather than exiting. A paired rig
                        // costs one cheap evaluation per tick (the key-apply step
                        // disarms auto-pair, so `should_attempt` says no), and the
                        // loop staying up is what lets a rig whose peer is later
                        // reflashed act on that. Exiting here meant the recovery
                        // path could only ever run once, at boot.
                        attempt = 0;
                        continue;
                    }
                    // A non-paired terminal. An external shutdown aborts the bind
                    // and breaks below without counting toward failover; otherwise
                    // this is a real failure worth counting.
                    if *shutdown.borrow() {
                        tracing::info!(role = role.as_str(), state, "auto_pair_aborted");
                        break;
                    }
                    attempt = attempt.saturating_add(1);
                    tracing::info!(
                        role = role.as_str(),
                        state,
                        attempt,
                        "auto_pair_attempt_unpaired"
                    );
                    // Signal the cloud relay only when one is actually
                    // configured (`server.mode` = cloud / self_hosted); a
                    // local-first node has nothing to fall back to. Either way
                    // the loop keeps binding on its fixed interval: the peer it
                    // is waiting for may come up at any moment.
                    if let Some(state) = failover_after_failure(attempt, cloud_relay_enabled) {
                        if failover.sync(state) {
                            emit_failover(&events, state);
                            tracing::warn!(attempts = attempt, "wfb_failover_to_cloud_relay");
                        }
                    }
                }
                Err(BindStartError::Busy) => {
                    // Another bind path raced us (REST handler, manual CLI). Defer
                    // without counting toward the failover cap; the busy session
                    // succeeds or fails and the next tick picks up from there.
                    tracing::info!("auto_pair_busy_retry");
                }
            }
        }
        tokio::select! {
            _ = shutdown.changed() => break,
            _ = tokio::time::sleep(retry_backoff()) => {}
        }
    }
}

#[cfg(test)]
mod tests;
