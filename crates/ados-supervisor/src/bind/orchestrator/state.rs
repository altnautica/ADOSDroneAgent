//! Session bookkeeping: state mutators, the no-progress watchdog, cleanup,
//! lifecycle events and the cross-process sentinel.

use std::path::Path;

use ados_protocol::logd::Level;
use serde_json::{json, Value};

use super::prep::summarize_precheck;
use super::{BindOrchestrator, InjectionPrepOutcome};
use crate::bind::bind_event::{
    bind_failed_detail, bind_precheck_detail, BindFailReason, BIND_FAILED_KIND, BIND_PRECHECK_KIND,
};
use crate::bind::fsm::{now_monotonic, BindSession, BindState};
use crate::bind::{socat, BindRole};

impl BindOrchestrator {
    /// Restart the normal wfb unit after a failed/aborted session so the rig is
    /// never left with both bind and normal profiles stopped.
    pub(super) async fn cleanup(&self, role: BindRole) {
        socat::kill_stale_bind_socats().await;
        self.pm.stop(role.bind_unit()).await;
        self.pm.start(role.normal_unit()).await;
        // Same recovery as the success path, for a drone that holds a key: its
        // video pipeline does not re-attach to the restarted wfb_tx on its own.
        // A drone with no key has no radio link for video to re-attach to, and
        // an unpaired drone runs a bind every ~40 s, so nudging it here would
        // tear its LAN video down on every attempt.
        let key_held =
            crate::bind::keys::read_public_fingerprint(Path::new(role.key_path())).is_ok();
        if video_nudge_after_failed_bind(role, key_held) {
            self.pm.try_restart("ados-video.service").await;
        }
        // A bind attempt cycles the adapter's monitor mode and, on a ground
        // station, regenerates the shared key file the swarm bus derives its key
        // from. Restart it with the radio so it is back on the live adapter and
        // the key on disk — if it is running; a stopped bus stays stopped.
        self.pm.try_restart(crate::bind::ADOS_SWARMBUS_UNIT).await;
        // Heal the global reg domain on the way out: a failed/cancelled/watchdog
        // retry cycled the bind unit's monitor mode and may have left the baked
        // country as the global domain. Without this, a retrying bind re-poisons
        // every loop and nothing restores the onboard WiFi's data path. The 1 Hz
        // window guard covers the steady churn; this is the final restore after
        // the normal unit is back. Idempotent + channel-safety-gated.
        crate::reg_reconciler::reconcile_global_domain(&self.events).await;
    }

    /// Stamp the injection-iface prep result onto the session sentinel and ship a
    /// durable `radio.bind_precheck` event. Run once per session, right after the
    /// iface is prepared for monitor mode. A `managed`/`unknown` injection mode
    /// here is the early warning that the bind will time out radiating nothing.
    pub(super) async fn record_bind_precheck(
        &self,
        role: BindRole,
        outcomes: &[InjectionPrepOutcome],
    ) {
        let summary = summarize_precheck(outcomes);
        if !summary.ok {
            tracing::warn!(
                role = role.as_str(),
                reason = summary.reason.unwrap_or("unknown"),
                injection_mode = %summary.injection_mode,
                nm_enumerable = summary.nm_enumerable,
                "bind_precheck_not_ready"
            );
        }
        self.events.emit(
            BIND_PRECHECK_KIND,
            if summary.ok { Level::Info } else { Level::Warn },
            bind_precheck_detail(
                role.as_str(),
                summary.ok,
                summary.reason,
                &summary.injection_mode,
                summary.nm_enumerable,
                summary.iface.as_deref(),
            ),
        );
        {
            let mut g = self.session.lock().await;
            if let Some(s) = g.as_mut() {
                s.bind_precheck = Some(summary);
            }
        }
        self.write_sentinel().await;
    }

    /// Ship a `radio.bind_failed` lifecycle event. A thin wrapper so each
    /// failure branch reads as one call. Best-effort + non-blocking.
    pub(super) fn emit_failed(
        &self,
        role: BindRole,
        reason: BindFailReason,
        phase: Option<&str>,
        message: &str,
        session_id: &str,
        elapsed_s: u64,
    ) {
        self.events.emit(
            BIND_FAILED_KIND,
            Level::Warn,
            bind_failed_detail(role.as_str(), reason, phase, message, session_id, elapsed_s),
        );
    }

    // ── session mutators (each locks briefly, never across an await) ────────
    pub(super) async fn set_state(&self, new: BindState) {
        {
            let mut g = self.session.lock().await;
            if let Some(s) = g.as_mut() {
                s.transition(new);
            }
        }
        self.write_sentinel().await;
    }

    pub(super) async fn set_error(&self, msg: impl Into<String>) {
        let mut g = self.session.lock().await;
        if let Some(s) = g.as_mut() {
            s.error = Some(msg.into());
        }
    }

    pub(super) async fn set_fingerprint(&self, fp: Option<String>) {
        let mut g = self.session.lock().await;
        if let Some(s) = g.as_mut() {
            s.fingerprint = fp;
        }
    }

    pub(super) async fn set_peer_verified(&self, verified: Option<bool>) {
        {
            let mut g = self.session.lock().await;
            if let Some(s) = g.as_mut() {
                s.peer_verified = verified;
            }
        }
        self.write_sentinel().await;
    }

    pub(super) async fn set_finished(&self) {
        let mut g = self.session.lock().await;
        if let Some(s) = g.as_mut() {
            s.finished_at = Some(crate::bind::fsm::iso_now());
        }
    }

    pub(super) async fn current_state(&self) -> Option<BindState> {
        self.session.lock().await.as_ref().map(|s| s.state)
    }

    /// No-progress watchdog: re-arms on every phase transition rather than racing
    /// one fixed global timer. It polls the live `(state, phase_entered_at)` on a
    /// short cadence and resolves the moment the current phase has been parked
    /// past its own budget (a true wedge) — a healthy bind whose phases
    /// legitimately sum past any single budget never trips, because each
    /// transition restamps `phase_entered_at` and re-derives the deadline.
    /// Resolves with the wedged phase + how long it was parked; an idle session
    /// (no record, or a terminal/budget-less state) parks forever so the other
    /// select arms own that path.
    pub(super) async fn watch_no_progress(&self) -> (BindState, f64) {
        // Tight cadence so the wedge surfaces close to its budget without busy
        // spinning; the phase budgets are tens of seconds, so 1 s is ample.
        const POLL: std::time::Duration = std::time::Duration::from_secs(1);
        loop {
            let snapshot = {
                let g = self.session.lock().await;
                g.as_ref().map(|s| (s.state, s.phase_entered_at))
            };
            if let Some((state, phase_entered_at)) = snapshot {
                if let (Some(budget), Some(entered)) = (state.watchdog_budget(), phase_entered_at) {
                    let parked = (now_monotonic() - entered).max(0.0);
                    if parked >= budget.as_secs_f64() {
                        return (state, parked);
                    }
                }
            }
            tokio::time::sleep(POLL).await;
        }
    }

    pub(super) async fn write_sentinel(&self) {
        let snap = self.session.lock().await.clone();
        write_sentinel(snap.as_ref());
    }
}

/// Write the cross-process bind sentinel atomically. `active` is the negation of
/// the terminal-state set; an absent session writes `{"active": false}`.
pub(super) fn write_sentinel(snap: Option<&BindSession>) {
    let mut v = snap.map(|s| s.to_json()).unwrap_or_else(|| json!({}));
    let active = snap.map(|s| s.state.is_active()).unwrap_or(false);
    if let Some(obj) = v.as_object_mut() {
        obj.insert("active".to_string(), json!(active));
        // Sidecar schema version (best-effort drift signal for readers).
        obj.insert(
            "version".to_string(),
            json!(crate::bind::BIND_STATE_SIDECAR_VERSION),
        );
    }
    if let Ok(body) = serde_json::to_vec(&v) {
        let sentinel = Path::new(crate::bind::BIND_STATE_SENTINEL);
        if let Err(e) = ados_protocol::sidecar::write_atomic(sentinel, &body, 0o644) {
            tracing::debug!(error = %e, "bind_sentinel_write_failed");
        }
    }
}

/// Out-of-process `is_bind_active()`: read the sentinel's `active` flag. Used by
/// the radio service + hop supervisor (separate processes from the supervisor
/// that owns the FSM). Absent/garbled sentinel → `false`.
pub fn read_sentinel_active() -> bool {
    let Ok(text) = std::fs::read_to_string(crate::bind::BIND_STATE_SENTINEL) else {
        return false;
    };
    let Ok(v) = serde_json::from_str::<Value>(&text) else {
        return false;
    };
    // Best-effort schema-drift signal (never reject): warn when the sentinel was
    // written by an agent with a different schema version, then read anyway.
    let got = v.get("version").and_then(Value::as_u64).unwrap_or(0) as u16;
    ados_protocol::sidecar::check_sidecar_version(
        "bind-state",
        got,
        crate::bind::BIND_STATE_SIDECAR_VERSION,
    );
    v.get("active").and_then(Value::as_bool).unwrap_or(false)
}

/// Whether a failed or aborted bind should cycle the drone's video pipeline.
///
/// Only a drone that holds a key has a radio link its video must re-attach to.
/// A drone with none is simply unpaired, and its auto-pair loop runs a bind
/// roughly every 40 s: nudging video there tears the LAN stream down on every
/// attempt for no link at all. Pure for testing.
pub(super) fn video_nudge_after_failed_bind(role: BindRole, key_held: bool) -> bool {
    role == BindRole::Drone && key_held
}
