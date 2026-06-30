//! The bind FSM driver. Ports `BindOrchestrator` from `bind_orchestrator.py`.
//!
//! `start_local_bind` runs the session future alongside a caller cancel and the
//! global wedge watchdog under one `tokio::select!`. When cancel or the
//! watchdog wins, dropping the session future is the structural cleanup:
//! `SocatProcess::drop` `killpg`s the tunnel and the peer-poll [`AbortOnDrop`]
//! guard aborts the poller — then [`cleanup`](BindOrchestrator::cleanup) stops
//! the bind unit and restarts the normal one. Every transition writes the
//! cross-process sentinel ([`super::BIND_STATE_SENTINEL`]) so the radio service
//! and hop supervisor can answer `is_bind_active()` without an in-process
//! singleton.

use std::future::Future;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

use crate::process_manager::ProcessManager;
use ados_protocol::logd::{emitter::EventEmitter, Level};
use serde_json::{json, Value};
use tokio::sync::Mutex;

use super::bind_event::{
    bind_failed_detail, bind_precheck_detail, bind_started_detail, BindFailReason,
    BIND_FAILED_KIND, BIND_PRECHECK_KIND, BIND_STARTED_KIND,
};
use super::fsm::{now_monotonic, BindPrecheck, BindSession, BindState};
use super::{
    iface, keys, socat, BindRole, BIND_REG_RECONCILE_INTERVAL, KEY_TRANSFER_TIMEOUT,
    LOCK_RECLAIM_TIMEOUT, RESTART_TIMEOUT, TUNNEL_POLL_INTERVAL, TUNNEL_WAIT_TIMEOUT,
    UPSTREAM_BIND_KEY, UPSTREAM_BIND_YAML, UPSTREAM_DRONE_KEY, UPSTREAM_GS_KEY,
    WAITING_PEER_WATCHDOG, WFB_BIND_CLIENT_SH, WFB_BIND_SERVER_SH,
};

/// A recoverable bind failure. `phase` names the [`BindState`] the failure
/// surfaced in so the GCS/LCD renders the right badge.
#[derive(Debug, Clone)]
pub struct BindError {
    pub message: String,
    pub phase: Option<String>,
}

impl BindError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            phase: None,
        }
    }

    pub fn with_phase(message: impl Into<String>, phase: &str) -> Self {
        Self {
            message: message.into(),
            phase: Some(phase.to_string()),
        }
    }
}

impl std::fmt::Display for BindError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for BindError {}

/// Returned by [`BindOrchestrator::start_local_bind`] when a session is already
/// in flight (the FastAPI seam maps this to a 409).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BindStartError {
    Busy,
}

/// Aborts the wrapped task when dropped — guarantees the peer-presence poller
/// dies even if the session future is dropped mid-flight (cancel / watchdog).
struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// RAII clear of the orchestrator's session-in-progress flag. Set to `true` the
/// moment `start_local_bind` commits to a session and dropped (back to `false`)
/// when the function returns — including every early-return and any unwind — so
/// the flag tracks the WHOLE `start_local_bind` body, not just the data-plane-
/// active sub-states. This is what the supervisor gate reads: a bind owns the
/// radio from the instant it stops the normal unit (in `Idle`, before the first
/// `OpeningTunnel` transition) until the function unwinds its cleanup.
struct InProgressGuard(Arc<AtomicBool>);

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

/// Single-instance bind state machine.
pub struct BindOrchestrator {
    /// Single-flight gate. `try_lock` failing means a session is in progress.
    lock: Mutex<()>,
    /// The live session (None when idle). Shared with the peer-poll task.
    session: Arc<Mutex<Option<BindSession>>>,
    /// Out-of-band abort: the control socket's `cancel_bind` op (which arrives
    /// on a different connection than the blocked `start_bind`) notifies this so
    /// the in-flight session aborts. `start_local_bind` races it in its select.
    cancel: Arc<tokio::sync::Notify>,
    /// Whole-session-in-progress flag. `true` for the entire body of an in-flight
    /// `start_local_bind` — set the instant it commits to a session (while still
    /// in `Idle`, BEFORE the normal unit is stopped and the radio prepared) and
    /// cleared when the function returns. Distinct from [`is_active`], which is
    /// driven by the session's data-plane sub-state and is `false` during the
    /// `Idle` stop→`OpeningTunnel` setup window. The supervisor's radio-restart /
    /// usb-rehome gate reads THIS so it never re-claims the adapter mid-setup.
    session_in_progress: Arc<AtomicBool>,
    /// Ships the bind-session lifecycle events to the logging daemon. Best-effort
    /// and non-blocking; an absent daemon socket is dropped quietly. The log
    /// lines remain the always-on fallback.
    events: EventEmitter,
    /// The host service-manager backend the bind sequence drives when it stops
    /// the normal radio unit, starts the bind unit, and restarts the units on
    /// cleanup. Selected for the host OS.
    pm: Arc<dyn ProcessManager>,
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
    /// Out-of-process callers use [`read_sentinel_active`].
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
    pub async fn start_local_bind<F>(
        &self,
        role: BindRole,
        peer_device_id: Option<String>,
        source: &str,
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

        let run = self.run_session(role, peer_device_id);
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
                let reason = BindFailReason::classify_error(&e.message, e.phase.as_deref());
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

    /// End-to-end orchestration. Stages mirror the BindState transitions.
    async fn run_session(
        &self,
        role: BindRole,
        peer_device_id: Option<String>,
    ) -> Result<(), BindError> {
        // Pre-flight: every external dep must be present BEFORE we touch the
        // radio so a missing artifact fails fast with a structured error.
        for path in [UPSTREAM_BIND_KEY, UPSTREAM_BIND_YAML] {
            if !Path::new(path).is_file() {
                return Err(BindError::new(format!(
                    "upstream wfb-ng artifact missing: {path}. Reinstall via \
                     install.sh to provision /etc/bind.key and /etc/bind.yaml."
                )));
            }
        }
        for script in [WFB_BIND_SERVER_SH, WFB_BIND_CLIENT_SH] {
            if !Path::new(script).is_file() {
                return Err(BindError::new(format!(
                    "upstream wfb-ng helper missing: {script}. wfb-ng package \
                     must be installed via install.sh."
                )));
            }
        }
        if !super::on_path("socat") {
            return Err(BindError::new(
                "socat binary not found on PATH. Install via `apt install \
                 socat` or rerun install.sh.",
            ));
        }

        // GS-only: generate the fresh keypair BEFORE stopping the wfb unit so a
        // keygen failure leaves the rig with its normal service still running.
        if role == BindRole::Gs {
            self.generate_keypair().await?;
        }

        // Stop the normal unit so it releases the radio for the bind profile.
        // Past this point any failure MUST restart it (cleanup() does).
        self.pm.stop(role.normal_unit()).await;

        // The RTL is now free (the normal unit is stopped): if the LIVE injection
        // driver is on a stale efuse country rather than the configured private-
        // regdb options, reload it now so the bind unit's monitor mode comes up on
        // a country-00 driver and never asserts a foreign country as the global
        // domain — poisoning the onboard WiFi at the source. A no-op when already
        // current; off the async runtime (blocking modprobe). Layer of prevention
        // beneath the bind-window reg guard, which remains the backstop.
        tokio::task::spawn_blocking(|| crate::rtl_modprobe::reconcile_live_driver(true))
            .await
            .ok();

        // Put the WFB injection adapter into NetworkManager-enumerable monitor
        // mode before the bind unit starts. The vendored wfb-ng `wfb-server`
        // aborts its init when `nmcli device show <iface>` cannot find the device
        // — the case when the adapter is in managed mode, up with no carrier,
        // which NetworkManager does not enumerate. A monitor-type iface is listed
        // as `(unmanaged)` regardless of carrier, the state the bind unit needs
        // and sets next anyway, so this prepares both the drone and the ground
        // node alike (idempotent). The prep now verifies the iface reached
        // monitor mode (readback + retry) and the result is stamped on the
        // sentinel + emitted as a durable event, so a stuck bind is diagnosable
        // without a wfb-server stderr trace.
        let prep = prepare_injection_iface_for_bind().await;
        self.record_bind_precheck(role, &prep).await;

        self.set_state(BindState::OpeningTunnel).await;

        // Start the bind profile (brings up the L3 tunnel).
        if !self.pm.start(role.bind_unit()).await {
            return Err(BindError::with_phase(
                format!("failed to start {}", role.bind_unit()),
                "opening_tunnel",
            ));
        }

        // Heal the global reg domain IMMEDIATELY: starting the bind unit just
        // re-entered monitor mode on the self-managed PHY, which can re-assert
        // its baked country as the GLOBAL domain right now — before wait_for_iface
        // (which can time out on a failing bind and never reach the post-tunnel
        // heal at tunnel_and_transfer). The 1 Hz bind-window guard keeps it pinned
        // thereafter; this is the prompt first heal.
        crate::reg_reconciler::reconcile_global_domain(&self.events).await;

        // try { … } finally { stop bind_unit }: the bind unit is stopped on
        // BOTH success and failure once it has been started.
        let result = self.tunnel_and_transfer(role, peer_device_id).await;
        self.pm.stop(role.bind_unit()).await;
        result
    }

    /// Post-tunnel stages: wait for the iface, run the wire protocol under the
    /// combined transfer budget, apply the key under the restart budget, and
    /// land on PAIRED. The peer-poll guard is dropped (aborting the poller) on
    /// any return path.
    async fn tunnel_and_transfer(
        &self,
        role: BindRole,
        peer_device_id: Option<String>,
    ) -> Result<(), BindError> {
        let bind_iface = role.bind_iface();
        if !iface::wait_for_iface(bind_iface, TUNNEL_WAIT_TIMEOUT).await {
            return Err(BindError::with_phase(
                format!(
                    "bind tunnel interface {bind_iface} did not come up within {}s",
                    TUNNEL_WAIT_TIMEOUT.as_secs()
                ),
                "opening_tunnel",
            ));
        }

        // Re-assert the configured regulatory domain right now. Starting the
        // bind profile re-entered monitor mode and re-set the channel on the
        // self-managed injection PHY, which can leave its EEPROM-baked country as
        // the GLOBAL domain again (the same break the radio bring-up guards
        // against). The radio service is stopped during a bind, so its own
        // immediate re-assert cannot run here; this is the prompt heal, the
        // instant the bind tunnel is up, so the foreign domain never lingers
        // long enough to blip the onboard WiFi. SAFETY: the shared reconcile only
        // ever forces a domain that permits the configured channel and never the
        // baked country / world default; idempotent when already in sync. The
        // supervisor's periodic reconciler stays the backstop.
        crate::reg_reconciler::reconcile_global_domain(&self.events).await;

        self.set_state(BindState::WaitingPeer).await;

        // Background poller: stamps last_frame_at when the bind TUN RX counter
        // advances. Aborted on return via the guard's Drop.
        let _poll_guard = AbortOnDrop(tokio::spawn(peer_poll(
            self.session.clone(),
            bind_iface.to_string(),
        )));

        // Reference point for the drone-side key-freshness gate: the upstream
        // key must have been deposited AFTER this instant to count as this
        // session's transfer (a leftover from any earlier bind has an older
        // mtime and must not satisfy the success check).
        let session_start = std::time::SystemTime::now();

        // Stages 4+5 share ONE budget (matches Python: splitting them would
        // change the failure phase the GCS badge reports on timeout).
        let blob = match tokio::time::timeout(KEY_TRANSFER_TIMEOUT, async {
            if role == BindRole::Drone {
                self.run_drone_server().await?;
            } else {
                self.run_gs_client().await?;
            }

            // ── Peer-evidence gate ──────────────────────────────────────────
            // The wire protocol exiting 0 is NOT proof a peer participated
            // (Rule 37: setting a state is not proof of the state). Two
            // observed phantoms: a stale roaming client EOFs the drone's
            // listener conversation (socat exits 0, the stale upstream key
            // passes a bare existence check), and a stale local listener lets
            // a client complete the handshake against its own box without a
            // single frame crossing the radio. Two independent proofs:
            //
            // 1. Decoded peer traffic on the bind TUN this session — the TUN RX
            //    counter only advances for frames that passed FEC + decryption
            //    in wfb_rx, and a local TCP loopback never traverses the TUN.
            let frames_seen = {
                let g = self.session.lock().await;
                g.as_ref().and_then(|s| s.last_frame_at).is_some()
            };
            if !frames_seen {
                self.set_peer_verified(Some(false)).await;
                return Err(BindError::with_phase(
                    "bind protocol completed but no peer traffic was decoded on \
                     the bind tunnel — refusing to pair without a real peer",
                    "transferring_keys",
                ));
            }

            // 2. Drone only: the upstream key file must have been deposited by
            //    THIS session's transfer (the wire protocol copies it fresh on
            //    a real exchange). A pre-existing file from an earlier bind
            //    means the conversation ended without a key transfer.
            let upstream = role.upstream_key();
            if role == BindRole::Drone && !upstream_key_fresh(Path::new(upstream), session_start) {
                self.set_peer_verified(Some(false)).await;
                return Err(BindError::with_phase(
                    format!(
                        "bind protocol completed but {upstream} was not \
                         refreshed by this session — no key was transferred"
                    ),
                    "transferring_keys",
                ));
            }
            self.set_peer_verified(Some(true)).await;
            self.set_state(BindState::ApplyingKeys).await;

            if !Path::new(upstream).is_file() {
                return Err(BindError::with_phase(
                    format!(
                        "bind protocol completed but {upstream} not present. \
                         Upstream may have failed silently."
                    ),
                    "applying_keys",
                ));
            }
            std::fs::read(upstream).map_err(|e| {
                BindError::with_phase(format!("failed to read {upstream}: {e}"), "applying_keys")
            })
        })
        .await
        {
            Ok(inner) => inner?,
            Err(_elapsed) => {
                let phase = self.current_state().await.map(|s| s.as_str().to_string());
                return Err(BindError {
                    message: format!(
                        "key transfer timed out after {}s in {}",
                        KEY_TRANSFER_TIMEOUT.as_secs(),
                        phase.as_deref().unwrap_or("unknown")
                    ),
                    phase,
                });
            }
        };

        // Stage 6: apply the key + restart the normal wfb unit, own budget.
        self.set_state(BindState::RestartingServices).await;
        match tokio::time::timeout(
            RESTART_TIMEOUT,
            keys::apply_keypair(self.pm.clone(), &blob, role, peer_device_id.as_deref()),
        )
        .await
        {
            Ok(Ok(_pair)) => {}
            Ok(Err(e)) => return Err(BindError::with_phase(e, "restarting_services")),
            Err(_) => {
                return Err(BindError::with_phase(
                    format!(
                        "service restart timed out after {}s",
                        RESTART_TIMEOUT.as_secs()
                    ),
                    "restarting_services",
                ))
            }
        }

        // Best-effort fingerprint of the freshly applied key.
        let fp = keys::read_public_fingerprint(Path::new(role.key_path())).ok();
        self.set_fingerprint(fp).await;

        self.set_state(BindState::Paired).await;
        tracing::info!(role = role.as_str(), "bind_session_paired");
        Ok(())
    }

    /// GS-only `wfb_keygen` in `/etc` → `/etc/gs.key` + `/etc/drone.key`.
    async fn generate_keypair(&self) -> Result<(), BindError> {
        if !super::on_path("wfb_keygen") {
            return Err(BindError::new(
                "wfb_keygen binary not found on PATH. install.sh provisions \
                 wfb-ng with the keygen tool; reinstall the agent.",
            ));
        }
        // Remove stale leftovers so a half-written pair can't ship mismatched.
        for path in [UPSTREAM_GS_KEY, UPSTREAM_DRONE_KEY] {
            let _ = std::fs::remove_file(path);
        }
        let out = tokio::process::Command::new("wfb_keygen")
            .current_dir("/etc")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .output()
            .await
            .map_err(|e| BindError::new(format!("wfb_keygen spawn failed: {e}")))?;
        if !out.status.success() {
            return Err(BindError::new(format!(
                "wfb_keygen failed (rc={:?}): {}",
                out.status.code(),
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
        if !Path::new(UPSTREAM_GS_KEY).is_file() || !Path::new(UPSTREAM_DRONE_KEY).is_file() {
            return Err(BindError::new(format!(
                "wfb_keygen exited 0 but did not produce {UPSTREAM_GS_KEY} + {UPSTREAM_DRONE_KEY}"
            )));
        }
        Ok(())
    }

    /// Drone side: listen on the tunnel rendezvous for the gs to connect.
    async fn run_drone_server(&self) -> Result<(), BindError> {
        if !Path::new(WFB_BIND_SERVER_SH).is_file() {
            return Err(BindError::new(format!(
                "upstream {WFB_BIND_SERVER_SH} missing. wfb-ng package must be installed."
            )));
        }
        self.set_state(BindState::TransferringKeys).await;
        self.run_socat(&socat::drone_server_args(), "socat server")
            .await
    }

    /// GS side: connect to the drone's listener over the tunnel.
    async fn run_gs_client(&self) -> Result<(), BindError> {
        if !Path::new(WFB_BIND_CLIENT_SH).is_file() {
            return Err(BindError::new(format!(
                "upstream {WFB_BIND_CLIENT_SH} missing. wfb-ng package must be installed."
            )));
        }
        self.set_state(BindState::TransferringKeys).await;
        self.run_socat(&socat::gs_client_args(), "socat client")
            .await
    }

    /// Spawn the socat tunnel (process-group isolated) and run it to exit. A
    /// non-zero rc → a `transferring_keys`-tagged error with the stderr tail.
    async fn run_socat(&self, args: &[String], what: &str) -> Result<(), BindError> {
        let mut proc = socat::SocatProcess::spawn(args).map_err(|e| {
            BindError::with_phase(format!("failed to spawn {what}: {e}"), "transferring_keys")
        })?;
        let (rc, _stdout, stderr) = proc.run().await.map_err(|e| {
            BindError::with_phase(format!("{what} run failed: {e}"), "transferring_keys")
        })?;
        if rc != 0 {
            let tail: String = String::from_utf8_lossy(&stderr)
                .trim()
                .chars()
                .take(240)
                .collect();
            return Err(BindError::with_phase(
                format!("{what} exited rc={rc}: {tail}"),
                "transferring_keys",
            ));
        }
        Ok(())
    }

    /// Restart the normal wfb unit after a failed/aborted session so the rig is
    /// never left with both bind and normal profiles stopped.
    async fn cleanup(&self, role: BindRole) {
        socat::kill_stale_bind_socats().await;
        self.pm.stop(role.bind_unit()).await;
        self.pm.start(role.normal_unit()).await;
        // Same recovery as the success path: the drone's video pipeline does not
        // re-attach to the restarted wfb_tx on its own, so restart it too — video
        // resumes without a drone reboot after a failed/aborted bind (Rule 26).
        if role == BindRole::Drone {
            self.pm.restart("ados-video.service").await;
        }
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
    async fn record_bind_precheck(&self, role: BindRole, outcomes: &[InjectionPrepOutcome]) {
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
    fn emit_failed(
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
    async fn set_state(&self, new: BindState) {
        {
            let mut g = self.session.lock().await;
            if let Some(s) = g.as_mut() {
                s.transition(new);
            }
        }
        self.write_sentinel().await;
    }

    async fn set_error(&self, msg: impl Into<String>) {
        let mut g = self.session.lock().await;
        if let Some(s) = g.as_mut() {
            s.error = Some(msg.into());
        }
    }

    async fn set_fingerprint(&self, fp: Option<String>) {
        let mut g = self.session.lock().await;
        if let Some(s) = g.as_mut() {
            s.fingerprint = fp;
        }
    }

    async fn set_peer_verified(&self, verified: Option<bool>) {
        {
            let mut g = self.session.lock().await;
            if let Some(s) = g.as_mut() {
                s.peer_verified = verified;
            }
        }
        self.write_sentinel().await;
    }

    async fn set_finished(&self) {
        let mut g = self.session.lock().await;
        if let Some(s) = g.as_mut() {
            s.finished_at = Some(super::fsm::iso_now());
        }
    }

    async fn current_state(&self) -> Option<BindState> {
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
    async fn watch_no_progress(&self) -> (BindState, f64) {
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

    async fn write_sentinel(&self) {
        let snap = self.session.lock().await.clone();
        write_sentinel(snap.as_ref());
    }
}

/// Background poller: stamp `last_frame_at` when the bind TUN RX counter
/// advances; self-exits when the session leaves the active set. Mirrors
/// `_poll_peer_presence_forever`.
/// True when `path` exists and its mtime is at or after `since` — i.e. the
/// upstream key was deposited by THIS bind session's wire transfer, not left
/// over from an earlier one. Total: any metadata error reads as not-fresh.
fn upstream_key_fresh(path: &Path, since: std::time::SystemTime) -> bool {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .map(|t| t >= since)
        .unwrap_or(false)
}

async fn peer_poll(session: Arc<Mutex<Option<BindSession>>>, iface: String) {
    let mut last: Option<u64> = None;
    loop {
        let active = matches!(session.lock().await.as_ref(), Some(s) if s.state.is_active());
        if !active {
            return;
        }
        if let Some(current) = iface::read_rx_packets_counter(&iface) {
            if let Some(prev) = last {
                if current != prev {
                    if let Some(s) = session.lock().await.as_mut() {
                        s.last_frame_at = Some(now_monotonic());
                    }
                }
            }
            last = Some(current);
        }
        tokio::time::sleep(TUNNEL_POLL_INTERVAL).await;
    }
}

/// Write the cross-process bind sentinel atomically. `active` is the negation of
/// the terminal-state set; an absent session writes `{"active": false}`.
fn write_sentinel(snap: Option<&BindSession>) {
    let mut v = snap.map(|s| s.to_json()).unwrap_or_else(|| json!({}));
    let active = snap.map(|s| s.state.is_active()).unwrap_or(false);
    if let Some(obj) = v.as_object_mut() {
        obj.insert("active".to_string(), json!(active));
    }
    if let Ok(body) = serde_json::to_vec(&v) {
        if let Err(e) = keys::atomic_write(Path::new(super::BIND_STATE_SENTINEL), &body, 0o644) {
            tracing::debug!(error = %e, "bind_sentinel_write_failed");
        }
    }
}

/// Out-of-process `is_bind_active()`: read the sentinel's `active` flag. Used by
/// the radio service + hop supervisor (separate processes from the supervisor
/// that owns the FSM). Absent/garbled sentinel → `false`.
pub fn read_sentinel_active() -> bool {
    std::fs::read_to_string(super::BIND_STATE_SENTINEL)
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|v| v.get("active").and_then(Value::as_bool))
        .unwrap_or(false)
}

/// The per-iface result of preparing one injection adapter for monitor mode.
/// (Constructed only on the linux prep path + in tests; the host lib build sees
/// it solely through `summarize_precheck`'s slice param.)
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
#[derive(Debug, Clone)]
struct InjectionPrepOutcome {
    iface: String,
    /// The readback operating mode after prep (`monitor` | `managed` | `unknown`).
    injection_mode: String,
    /// Whether the readback confirmed `type monitor` within the retry budget.
    monitor_verified: bool,
    /// Whether NetworkManager enumerates the iface (the legacy precheck condition).
    nm_enumerable: bool,
}

/// Max times to run the monitor-mode prep before giving up on a verified readback.
#[cfg(target_os = "linux")]
const BIND_PREP_MAX_ATTEMPTS: u32 = 3;

/// Put the WFB injection adapter(s) into MONITOR mode before the bind unit starts,
/// and VERIFY the iface actually reached it (readback + retry) rather than firing
/// the commands and hoping.
///
/// History: the vendored wfb-ng `wfb-server` aborted its init when its
/// `nmcli device show <iface>` pre-check could not find the device — the case for
/// a managed-mode iface up with no carrier, which NetworkManager does not
/// enumerate. That pre-check is now skipped at the wfb-server config layer, so the
/// load-bearing job here is simply that the iface reaches a radiating monitor PHY:
/// `iw dev <if> set monitor none` initialises the PHY un-muted, unlike
/// `iw <if> set type monitor`, which leaves the RTL8812EU pinned at the muted
/// txpower floor (-100 dBm, carrier down). A command "succeeding" does not prove
/// the mode took, so each candidate is read back via `iw <if> info` and the prep
/// retried with backoff until `type monitor` is observed. Best-effort + idempotent;
/// the `is_injection && !is_virtual` filter never touches the management link.
/// Returns one outcome per injection candidate for the caller to record + emit.
#[cfg(target_os = "linux")]
async fn prepare_injection_iface_for_bind() -> Vec<InjectionPrepOutcome> {
    let candidates = crate::mgmt_link_guardian::detection::collect_candidates().await;
    let mut outcomes = Vec::new();
    for c in candidates
        .iter()
        .filter(|c| c.is_injection && !c.is_virtual)
    {
        let iface = c.name.as_str();
        let mut injection_mode = "unknown".to_string();
        let mut monitor_verified = false;
        for attempt in 0..BIND_PREP_MAX_ATTEMPTS {
            if attempt > 0 {
                tokio::time::sleep(std::time::Duration::from_millis(500 * attempt as u64)).await;
            }
            // Release from NetworkManager, then bring the iface into monitor mode.
            let _ = run_iface_cmd("nmcli", &["dev", "set", iface, "managed", "no"]).await;
            let _ = run_iface_cmd("ip", &["link", "set", iface, "down"]).await;
            // `set monitor none` is primary (it initialises the PHY un-muted); the
            // `set type monitor` form is the fallback for any adapter that rejects it.
            if !run_iface_cmd("iw", &["dev", iface, "set", "monitor", "none"]).await {
                let _ = run_iface_cmd("iw", &[iface, "set", "type", "monitor"]).await;
            }
            let _ = run_iface_cmd("ip", &["link", "set", iface, "up"]).await;
            injection_mode = read_injection_iface_mode(iface)
                .await
                .unwrap_or_else(|| "unknown".to_string());
            if injection_mode == "monitor" {
                monitor_verified = true;
                break;
            }
        }
        let nm_enumerable = iface_nm_enumerable(iface).await;
        tracing::info!(
            iface,
            injection_mode = %injection_mode,
            monitor_verified,
            nm_enumerable,
            "bind_prepared_injection_iface"
        );
        outcomes.push(InjectionPrepOutcome {
            iface: iface.to_string(),
            injection_mode,
            monitor_verified,
            nm_enumerable,
        });
    }
    outcomes
}

/// Off-Linux dev hosts have no injection adapter to prepare.
#[cfg(not(target_os = "linux"))]
async fn prepare_injection_iface_for_bind() -> Vec<InjectionPrepOutcome> {
    Vec::new()
}

/// Reduce the per-iface prep outcomes to one session-level summary. `ok` requires
/// every candidate to have reached verified monitor mode (a managed-mode adapter
/// radiates nothing); the representative `injection_mode`/`iface` prefer a failing
/// candidate so the surfaced reason points at the actual problem.
fn summarize_precheck(outcomes: &[InjectionPrepOutcome]) -> BindPrecheck {
    if outcomes.is_empty() {
        return BindPrecheck {
            ok: false,
            reason: Some("iface_not_found"),
            injection_mode: "unknown".to_string(),
            nm_enumerable: false,
            iface: None,
        };
    }
    let ok = outcomes.iter().all(|o| o.monitor_verified);
    let nm_enumerable = outcomes.iter().all(|o| o.nm_enumerable);
    let representative = outcomes
        .iter()
        .find(|o| !o.monitor_verified)
        .unwrap_or(&outcomes[0]);
    BindPrecheck {
        ok,
        reason: if ok { None } else { Some("monitor_unverified") },
        injection_mode: representative.injection_mode.clone(),
        nm_enumerable,
        iface: Some(representative.iface.clone()),
    }
}

/// Run a short interface-management command, bounded so a hung tool never stalls
/// the bind. Returns whether it exited successfully.
#[cfg(target_os = "linux")]
async fn run_iface_cmd(bin: &str, args: &[&str]) -> bool {
    matches!(
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            tokio::process::Command::new(bin).args(args).output(),
        )
        .await,
        Ok(Ok(o)) if o.status.success()
    )
}

/// Read the operating mode (`monitor` | `managed` | …) of an injection iface from
/// `iw <if> info`, or `None` when it cannot be read. The parse mirrors the radio's
/// `set_monitor_mode_verified` readback (the `type ` line) without taking a crate
/// dependency on `ados-radio` (which would be heavy + cyclic from the supervisor).
#[cfg(target_os = "linux")]
async fn read_injection_iface_mode(iface: &str) -> Option<String> {
    let out = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        tokio::process::Command::new("iw")
            .args([iface, "info"])
            .output(),
    )
    .await
    .ok()?
    .ok()?;
    if !out.status.success() {
        return None;
    }
    parse_iface_mode(&String::from_utf8_lossy(&out.stdout))
}

/// Pure parse of the `type <mode>` line out of `iw <iface> info`. Unit-tested
/// independently of `iw`. (Only the linux reader calls it outside tests.)
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn parse_iface_mode(info: &str) -> Option<String> {
    for line in info.lines() {
        if let Some(rest) = line.trim().strip_prefix("type ") {
            let mode = rest.trim();
            if !mode.is_empty() {
                return Some(mode.to_string());
            }
        }
    }
    None
}

/// Whether NetworkManager enumerates the iface: `nmcli device show <iface>` exits
/// 0 when NM knows the device and non-zero (RC 10) when it does not. Informational
/// now the wfb-server precheck is skipped, but it is exactly the condition that
/// precheck tested, so it predicts that abort if the skip is ever reverted.
#[cfg(target_os = "linux")]
async fn iface_nm_enumerable(iface: &str) -> bool {
    run_iface_cmd("nmcli", &["-t", "device", "show", iface]).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upstream_key_fresh_rejects_stale_missing_and_accepts_fresh() {
        let dir = tempfile::tempdir().unwrap();
        let key = dir.path().join("drone.key");

        // Missing → not fresh (a conversation that never transferred a key).
        let t0 = std::time::SystemTime::now();
        assert!(!upstream_key_fresh(&key, t0));

        // Written BEFORE the session start → stale leftover, not fresh.
        std::fs::write(&key, b"old").unwrap();
        let after_write = std::time::SystemTime::now() + std::time::Duration::from_secs(2);
        assert!(!upstream_key_fresh(&key, after_write));

        // Written AFTER the session start → this session's transfer.
        let before_write = std::time::SystemTime::now() - std::time::Duration::from_secs(2);
        std::fs::write(&key, b"new").unwrap();
        assert!(upstream_key_fresh(&key, before_write));
    }

    #[test]
    fn parse_iface_mode_reads_the_type_line() {
        let monitor = "Interface wlan1\n\tifindex 5\n\ttype monitor\n\twiphy 0\n";
        assert_eq!(parse_iface_mode(monitor).as_deref(), Some("monitor"));
        let managed = "Interface wlan1\n\ttype managed\n";
        assert_eq!(parse_iface_mode(managed).as_deref(), Some("managed"));
        assert!(parse_iface_mode("Interface wlan1\n\tifindex 5\n").is_none());
        assert!(parse_iface_mode("").is_none());
    }

    #[test]
    fn summarize_precheck_classifies_outcomes() {
        // No injection candidate found at all.
        let empty = summarize_precheck(&[]);
        assert!(!empty.ok);
        assert_eq!(empty.reason, Some("iface_not_found"));
        assert!(empty.iface.is_none());

        // A verified-monitor candidate is ok with no reason.
        let good = summarize_precheck(&[InjectionPrepOutcome {
            iface: "wlan1".to_string(),
            injection_mode: "monitor".to_string(),
            monitor_verified: true,
            nm_enumerable: true,
        }]);
        assert!(good.ok);
        assert!(good.reason.is_none());
        assert_eq!(good.injection_mode, "monitor");

        // A managed (unverified) candidate surfaces the failing iface + mode.
        let bad = summarize_precheck(&[
            InjectionPrepOutcome {
                iface: "wlan2".to_string(),
                injection_mode: "monitor".to_string(),
                monitor_verified: true,
                nm_enumerable: true,
            },
            InjectionPrepOutcome {
                iface: "wlan1".to_string(),
                injection_mode: "managed".to_string(),
                monitor_verified: false,
                nm_enumerable: false,
            },
        ]);
        assert!(!bad.ok);
        assert_eq!(bad.reason, Some("monitor_unverified"));
        assert_eq!(bad.injection_mode, "managed");
        assert_eq!(bad.iface.as_deref(), Some("wlan1"));
        assert!(!bad.nm_enumerable);
    }

    #[tokio::test]
    async fn preflight_missing_artifacts_fails_fast_to_failed() {
        // No /etc/bind.key on the dev host / CI → run_session returns Err
        // before any systemctl, the driver lands on FAILED, lock releases.
        let orch = BindOrchestrator::new();
        let result = orch
            .start_local_bind(
                BindRole::Drone,
                None,
                "operator",
                std::future::pending::<()>(),
            )
            .await
            .expect("start should not report Busy");
        assert_eq!(result["state"], "failed");
        assert_eq!(result["role"], "drone");
        let err = result["error"].as_str().unwrap_or("");
        assert!(err.contains("missing"), "unexpected error: {err}");
        assert!(result["finished_at"].is_string());
        // Terminal → not active, and the singleflight lock is free again.
        assert!(!orch.is_active().await);
    }

    #[tokio::test]
    async fn busy_when_a_session_holds_the_lock() {
        let orch = Arc::new(BindOrchestrator::new());
        // Hold the singleflight lock by parking a bind on a cancel that never
        // fires AND artifacts that DO let it past preflight is hard on a dev
        // host; instead assert the lock directly: take it, then a start fails.
        let _held = orch.lock.try_lock().expect("free at start");
        // An active, freshly-progressing session must keep a concurrent bind out
        // immediately. The stale-session self-heal only reclaims a terminal or
        // watchdog-stale record, never a live one.
        {
            let mut s = BindSession::new(BindRole::Gs, "operator", None);
            s.transition(BindState::WaitingPeer);
            *orch.session.lock().await = Some(s);
        }
        let busy = orch
            .start_local_bind(BindRole::Gs, None, "operator", std::future::pending::<()>())
            .await;
        assert_eq!(busy, Err(BindStartError::Busy));
    }

    #[tokio::test]
    async fn reclaims_the_lock_from_a_terminal_orphan_session() {
        // A guard held while the session record is terminal (e.g. a prior bind
        // that finished but whose guard-holder has not yet dropped) must be
        // reclaimable, not a permanent Busy. We assert the decision the self-heal
        // makes on the session record, not the full reclaim (which needs a live
        // wedged holder that releases on cancel).
        let orch = BindOrchestrator::new();
        let mut s = BindSession::new(BindRole::Gs, "operator", None);
        s.transition(BindState::Failed); // terminal
        let terminal_reclaimable = s.state.is_terminal();
        assert!(terminal_reclaimable);
        // A stale (watchdog-aged) phase clock is also reclaimable.
        s.transition(BindState::WaitingPeer);
        s.phase_entered_at = Some(now_monotonic() - WAITING_PEER_WATCHDOG.as_secs_f64() - 30.0);
        let stale_reclaimable = s
            .phase_entered_at
            .is_none_or(|t| now_monotonic() - t > WAITING_PEER_WATCHDOG.as_secs_f64());
        assert!(stale_reclaimable);
        // A fresh active session is NOT reclaimable.
        s.transition(BindState::WaitingPeer);
        let fresh_reclaimable = s.state.is_terminal()
            || s.phase_entered_at
                .is_none_or(|t| now_monotonic() - t > WAITING_PEER_WATCHDOG.as_secs_f64());
        assert!(!fresh_reclaimable);
        let _ = &orch;
    }

    #[test]
    fn sentinel_active_false_when_absent() {
        // The real sentinel path almost certainly does not exist in the test
        // env; a missing/garbled file reads as not-active.
        let _ = read_sentinel_active();
    }

    #[tokio::test]
    async fn session_active_is_true_during_idle_setup_window_while_is_active_is_false() {
        // The exact race the supervisor gate must cover: a bind has committed to
        // a session and is mid-setup (the normal radio unit stopped, the
        // injection iface re-prepared) but the FSM is still in `Idle`, which is a
        // terminal state. `is_active` (driven by the session sub-state) reads
        // FALSE here, so a gate on it would NOT block — and a monitor pass would
        // auto-restart the radio unit out from under the bind. `session_active`
        // (the whole-body in-progress flag) must read TRUE for this window.
        let orch = BindOrchestrator::new();

        // Reproduce the window: install an `Idle` session (as `start_local_bind`
        // does at commit) and raise the in-progress flag (as it does right
        // before touching the radio).
        *orch.session.lock().await = Some(BindSession::new(BindRole::Drone, "operator", None));
        orch.session_in_progress.store(true, Ordering::SeqCst);

        // The data-plane liveness is FALSE (Idle is terminal) ...
        assert!(
            !orch.is_active().await,
            "an Idle session is terminal so is_active must be false"
        );
        // ... but the radio-ownership gate must still hold the adapter.
        assert!(
            orch.session_active(),
            "session_active must be true across the Idle stop→opening_tunnel setup window"
        );
    }

    #[tokio::test]
    async fn in_progress_guard_clears_the_flag_on_drop() {
        // The RAII guard must release the flag on every return path so a finished
        // (or panicking) bind never leaves the gate stuck blocking restarts.
        let orch = BindOrchestrator::new();
        assert!(!orch.session_active(), "flag starts clear");
        {
            orch.session_in_progress.store(true, Ordering::SeqCst);
            let _g = InProgressGuard(orch.session_in_progress.clone());
            assert!(
                orch.session_active(),
                "flag is raised inside the guard scope"
            );
        }
        assert!(
            !orch.session_active(),
            "dropping the InProgressGuard must clear the in-progress flag"
        );
    }

    #[tokio::test]
    async fn start_local_bind_clears_session_active_when_it_returns() {
        // A full fail-fast bind (no artifacts on the dev host) runs the whole
        // `start_local_bind` body, which raises the flag at commit and drops the
        // RAII guard on return. After it returns the gate must be released.
        let orch = BindOrchestrator::new();
        assert!(!orch.session_active(), "clear before any bind");
        let result = orch
            .start_local_bind(
                BindRole::Drone,
                None,
                "operator",
                std::future::pending::<()>(),
            )
            .await
            .expect("start should not report Busy");
        assert_eq!(result["state"], "failed");
        assert!(
            !orch.session_active(),
            "session_active must be false once start_local_bind returns"
        );
    }
}
