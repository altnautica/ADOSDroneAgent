//! Session entry points: construction, liveness queries and the single-flight
//! `start_local_bind` driver that races the session against cancel and the
//! no-progress watchdog.

use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

use ados_protocol::logd::{emitter::EventEmitter, Level};
use serde_json::{json, Value};
use tokio::sync::Mutex;

use super::state::write_sentinel;
use super::{AbortOnDrop, BindError, BindOrchestrator, BindStartError, InProgressGuard};
use crate::bind::bind_event::{bind_started_detail, BindFailReason, BIND_STARTED_KIND};
use crate::bind::fsm::{now_monotonic, BindSession, BindState};
use crate::bind::{
    keys, socat, BindRole, BIND_REG_RECONCILE_INTERVAL, LOCK_RECLAIM_TIMEOUT, WAITING_PEER_WATCHDOG,
};

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

impl Drop for InProgressGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

/// Re-assert the configured global regulatory domain at a fast cadence for the
/// life of a bind window. Spawned at the top of
/// [`BindOrchestrator::start_local_bind`] and aborted via an [`AbortOnDrop`]
/// guard when the session ends, so the self-managed injection PHY's baked
/// country can never linger as the global cfg80211 domain long enough to blip
/// the onboard management WiFi — on EVERY retry, not only on tunnel-up or
/// success (the failing-bind path never reaches the post-tunnel heal, and the
/// radio service that would normally re-assert is stopped during a bind). The
/// shared reconcile is idempotent (a no-op when already in sync) and only ever
/// forces a domain that permits the configured channel, never the baked country
/// or the world default.
async fn bind_window_reg_guard(events: EventEmitter) {
    loop {
        crate::reg_reconciler::reconcile_global_domain(&events).await;
        tokio::time::sleep(BIND_REG_RECONCILE_INTERVAL).await;
    }
}

impl Default for BindOrchestrator {
    fn default() -> Self {
        Self::new()
    }
}

impl BindOrchestrator {
    /// Construct the orchestrator. Spawns the event emitter's background shipper
    /// on the current tokio runtime, so call this from within a runtime context
    /// (every call site — the supervisor main and the tests — already is).
    pub fn new() -> Self {
        Self {
            lock: Mutex::new(()),
            session: Arc::new(Mutex::new(None)),
            cancel: Arc::new(tokio::sync::Notify::new()),
            session_in_progress: Arc::new(AtomicBool::new(false)),
            events: EventEmitter::new("ados-supervisor"),
            pm: crate::process_manager::select(),
        }
    }

    /// In-process bind-liveness (the supervisor monitor reads this directly).
    /// Out-of-process callers use [`read_sentinel_active`](super::read_sentinel_active).
    ///
    /// Reflects the session's data-plane sub-state — `false` while a bind is
    /// still in its `Idle` setup window (before the first `OpeningTunnel`
    /// transition) even though the radio is already being torn down for the bind
    /// profile. Use [`session_active`](Self::session_active) for the
    /// radio-ownership gate; keep this for consumers that need the data-plane
    /// distinction.
    pub async fn is_active(&self) -> bool {
        matches!(self.session.lock().await.as_ref(), Some(s) if s.state.is_active())
    }

    /// Whether a bind session is in progress anywhere in `start_local_bind` —
    /// `true` from the moment a session is committed (still `Idle`, before the
    /// normal radio unit is stopped) until the function returns. This closes the
    /// gap [`is_active`](Self::is_active) leaves open during the `Idle`
    /// stop→`OpeningTunnel` setup window, where the radio is already claimed for
    /// the bind but the data-plane state has not advanced. The supervisor's
    /// radio auto-restart and usb-rehome gates read THIS so they never re-claim
    /// the adapter out from under a bind that is mid-setup.
    pub fn session_active(&self) -> bool {
        self.session_in_progress.load(Ordering::SeqCst)
    }

    /// Test seam: reproduce the `Idle` bind setup window — an `Idle`
    /// (terminal-state) session installed and the in-progress flag raised,
    /// exactly the state `start_local_bind` holds after committing a session but
    /// before the first `OpeningTunnel` transition. Used by the supervisor gate
    /// regression test, which lives in a sibling module that cannot reach the
    /// private fields directly.
    #[cfg(test)]
    pub async fn enter_idle_setup_window_for_test(&self) {
        *self.session.lock().await = Some(BindSession::new(BindRole::Drone, "test", None));
        self.session_in_progress.store(true, Ordering::SeqCst);
    }

    /// Abort the in-flight bind session, if any. Idempotent + safe when idle
    /// (the notify is simply dropped with no waiter). Invoked by the control
    /// socket's `cancel_bind` op from a connection separate from the blocked
    /// `start_bind`.
    pub fn cancel_current(&self) {
        self.cancel.notify_waiters();
    }

    /// Snapshot of the current session for the REST surface, or None if idle.
    pub async fn status(&self) -> Option<Value> {
        self.session.lock().await.as_ref().map(|s| s.to_json())
    }

    /// Open a bind window and run the upstream protocol to completion. Blocks
    /// until a peer is found and the handshake succeeds, `cancel` fires, the
    /// watchdog trips, or the protocol raises. Concurrent calls fail fast with
    /// [`BindStartError::Busy`].
    ///
    /// `fleet` is the addressing to persist alongside the key once the bind
    /// lands. A drone's slot is issued by the ground station's `FleetRegistry`
    /// at pair time and passed in by the caller that triggered this bind — it is
    /// never derived on the drone, because a slot a drone picked for itself
    /// could collide with a flying peer's `channel_id`. `None` leaves whatever
    /// the config already carries.
    pub async fn start_local_bind<F>(
        &self,
        role: BindRole,
        peer_device_id: Option<String>,
        source: &str,
        fleet: Option<keys::FleetIdentity>,
        cancel: F,
    ) -> Result<Value, BindStartError>
    where
        F: Future<Output = ()>,
    {
        // Single-flight gate with stale-session self-heal. A genuinely active,
        // progressing session keeps a new bind out (`Busy`). But a prior session
        // that wedged — its `start_local_bind` still holding the guard while
        // parked past the watchdog — or left a terminal/orphaned record must not
        // lock the operator (and the auto-pair retry loop) out for the whole
        // watchdog window: ask it to cancel and reclaim the guard within a bounded
        // grace. Intra-rig only (reconciles the in-process guard against the
        // in-process session record); it is not a cross-host lock.
        let _guard = match self.lock.try_lock() {
            Ok(g) => g,
            Err(_) => {
                let reclaimable = match self.session.lock().await.as_ref() {
                    // Guard held but no session record — orphaned; reclaim.
                    None => true,
                    Some(s) => {
                        s.state.is_terminal()
                            || s.phase_entered_at.is_none_or(|t| {
                                now_monotonic() - t > WAITING_PEER_WATCHDOG.as_secs_f64()
                            })
                    }
                };
                if !reclaimable {
                    return Err(BindStartError::Busy);
                }
                tracing::warn!(
                    source,
                    "bind_lock_reclaimed_stale: cancelling a wedged/terminal prior session"
                );
                self.cancel_current();
                match tokio::time::timeout(LOCK_RECLAIM_TIMEOUT, self.lock.lock()).await {
                    Ok(g) => g,
                    Err(_) => return Err(BindStartError::Busy),
                }
            }
        };

        // Mark the whole session in progress the instant the single-flight guard
        // is held — BEFORE the radio is touched, so the supervisor's radio-restart
        // / usb-rehome gate (`session_active`) blocks across the `Idle`
        // stop→`OpeningTunnel` setup window too, not just once the data-plane state
        // advances. The RAII drop clears it on every return path (including an
        // early error, the watchdog/cancel branch, or an unwind).
        self.session_in_progress.store(true, Ordering::SeqCst);
        let _in_progress = InProgressGuard(self.session_in_progress.clone());

        // Sweep stragglers before touching the radio (cheap + idempotent).
        socat::kill_stale_bind_socats().await;
        let new_session = BindSession::new(role, source, peer_device_id.clone());
        // Capture the identifiers + a monotonic origin for the lifecycle events
        // before the session is moved behind the lock.
        let session_id = new_session.session_id.clone();
        let started_at = Instant::now();
        *self.session.lock().await = Some(new_session);
        self.write_sentinel().await;
        tracing::info!(role = role.as_str(), source, "bind_session_started");
        self.events.emit(
            BIND_STARTED_KIND,
            Level::Info,
            bind_started_detail(
                role.as_str(),
                source,
                &session_id,
                peer_device_id.as_deref(),
            ),
        );

        // Pin the global regulatory domain at 1 Hz for the whole window. Dropped
        // (aborting the loop) when this function returns, so it covers the bind
        // unit's monitor-mode churn through every retry AND the cleanup restart.
        let _reg_guard = AbortOnDrop(tokio::spawn(bind_window_reg_guard(self.events.clone())));

        let run = self.run_session(role, peer_device_id, fleet);
        tokio::pin!(cancel);
        let internal_cancel = self.cancel.clone();

        enum Outcome {
            Done(Result<(), BindError>),
            Cancelled,
            Watchdog(BindState, f64),
        }

        // Arm order is load-bearing: `run` is polled first, so a session that
        // completes in the same tick the watchdog/cancel becomes ready still
        // wins and a just-reached PAIRED is preserved — the equivalent of the
        // Python `session_task in done` guard. Do NOT reorder these arms.
        // Two cancel sources: the caller-supplied `cancel` future (e.g. auto-
        // pair's stop signal) and the out-of-band `cancel_current()` notify
        // (the control socket's `cancel_bind` op). Either aborts. The watchdog is
        // a no-progress detector that re-arms each phase, so a healthy bind whose
        // phases sum past any single budget no longer trips a fixed global timer.
        let outcome = tokio::select! {
            biased;
            r = run => Outcome::Done(r),
            _ = &mut cancel => Outcome::Cancelled,
            _ = internal_cancel.notified() => Outcome::Cancelled,
            (phase, parked_s) = self.watch_no_progress() => Outcome::Watchdog(phase, parked_s),
        };

        let elapsed_s = started_at.elapsed().as_secs();
        match outcome {
            Outcome::Cancelled => {
                self.set_state(BindState::Aborted).await;
                self.set_error("cancelled by caller").await;
                tracing::info!("bind_session_aborted");
                self.emit_failed(
                    role,
                    BindFailReason::Interrupted,
                    None,
                    "cancelled by caller",
                    &session_id,
                    elapsed_s,
                );
                self.cleanup(role).await;
            }
            Outcome::Watchdog(wedged_phase, parked_s) => {
                let msg = format!(
                    "watchdog fired: phase {} made no progress for {:.0}s",
                    wedged_phase.as_str(),
                    parked_s
                );
                // The watchdog names the phase it found parked past budget, so
                // the GCS badge reports the real wedge, not a generic timeout.
                self.set_state(BindState::Failed).await;
                self.set_error(msg.clone()).await;
                tracing::warn!(
                    phase = wedged_phase.as_str(),
                    parked_s,
                    "bind_session_watchdog_fired"
                );
                self.emit_failed(
                    role,
                    BindFailReason::Timeout,
                    Some(wedged_phase.as_str()),
                    &msg,
                    &session_id,
                    elapsed_s,
                );
                self.cleanup(role).await;
            }
            Outcome::Done(Ok(())) => {
                // run_session already transitioned to PAIRED on the happy path.
            }
            Outcome::Done(Err(e)) => {
                self.set_error(e.message.clone()).await;
                self.set_state(BindState::Failed).await;
                tracing::warn!(error = %e.message, phase = ?e.phase, "bind_session_failed");
                // Prefer the gate's explicit, pre-classified cause; only fall
                // back to string-matching the message when none was carried.
                let reason = e.reason.unwrap_or_else(|| {
                    BindFailReason::classify_error(&e.message, e.phase.as_deref())
                });
                self.emit_failed(
                    role,
                    reason,
                    e.phase.as_deref(),
                    &e.message,
                    &session_id,
                    elapsed_s,
                );
                self.cleanup(role).await;
            }
        }

        self.set_finished().await;
        let snap = self.session.lock().await.clone();
        write_sentinel(snap.as_ref());
        Ok(snap.map(|s| s.to_json()).unwrap_or_else(|| json!({})))
    }
}
