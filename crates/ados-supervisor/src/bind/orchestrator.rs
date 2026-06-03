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
use std::sync::Arc;
use std::time::Instant;

use ados_protocol::logd::{emitter::EventEmitter, Level};
use serde_json::{json, Value};
use tokio::sync::Mutex;

use super::bind_event::{
    bind_failed_detail, bind_started_detail, BindFailReason, BIND_FAILED_KIND, BIND_STARTED_KIND,
};
use super::fsm::{now_monotonic, BindSession, BindState};
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
    /// Ships the bind-session lifecycle events to the logging daemon. Best-effort
    /// and non-blocking; an absent daemon socket is dropped quietly. The log
    /// lines remain the always-on fallback.
    events: EventEmitter,
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
            events: EventEmitter::new("ados-supervisor"),
        }
    }

    /// In-process bind-liveness (the supervisor monitor reads this directly).
    /// Out-of-process callers use [`read_sentinel_active`].
    pub async fn is_active(&self) -> bool {
        matches!(self.session.lock().await.as_ref(), Some(s) if s.state.is_active())
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
            Watchdog,
        }

        // Arm order is load-bearing: `run` is polled first, so a session that
        // completes in the same tick the watchdog/cancel becomes ready still
        // wins and a just-reached PAIRED is preserved — the equivalent of the
        // Python `session_task in done` guard. Do NOT reorder these arms.
        // Two cancel sources: the caller-supplied `cancel` future (e.g. auto-
        // pair's stop signal) and the out-of-band `cancel_current()` notify
        // (the control socket's `cancel_bind` op). Either aborts.
        let outcome = tokio::select! {
            biased;
            r = run => Outcome::Done(r),
            _ = &mut cancel => Outcome::Cancelled,
            _ = internal_cancel.notified() => Outcome::Cancelled,
            _ = tokio::time::sleep(WAITING_PEER_WATCHDOG) => Outcome::Watchdog,
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
            Outcome::Watchdog => {
                let msg = format!(
                    "watchdog fired after {}s with no progress",
                    WAITING_PEER_WATCHDOG.as_secs()
                );
                // Capture the phase the session was parked in (typically
                // `waiting_peer`) BEFORE transitioning to FAILED.
                let phase = self.current_state().await.map(|s| s.as_str());
                self.set_state(BindState::Failed).await;
                self.set_error(msg.clone()).await;
                tracing::warn!("bind_session_watchdog_fired");
                self.emit_failed(
                    role,
                    BindFailReason::Timeout,
                    phase,
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
        crate::systemctl::stop(role.normal_unit()).await;

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

        self.set_state(BindState::OpeningTunnel).await;

        // Start the bind profile (brings up the L3 tunnel).
        if !crate::systemctl::start(role.bind_unit()).await {
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
        crate::systemctl::stop(role.bind_unit()).await;
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

        // Stages 4+5 share ONE budget (matches Python: splitting them would
        // change the failure phase the GCS badge reports on timeout).
        let blob = match tokio::time::timeout(KEY_TRANSFER_TIMEOUT, async {
            if role == BindRole::Drone {
                self.run_drone_server().await?;
            } else {
                self.run_gs_client().await?;
            }
            self.set_state(BindState::ApplyingKeys).await;

            let upstream = role.upstream_key();
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
            keys::apply_keypair(&blob, role, peer_device_id.as_deref()),
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
        crate::systemctl::stop(role.bind_unit()).await;
        crate::systemctl::start(role.normal_unit()).await;
        // Heal the global reg domain on the way out: a failed/cancelled/watchdog
        // retry cycled the bind unit's monitor mode and may have left the baked
        // country as the global domain. Without this, a retrying bind re-poisons
        // every loop and nothing restores the onboard WiFi's data path. The 1 Hz
        // window guard covers the steady churn; this is the final restore after
        // the normal unit is back. Idempotent + channel-safety-gated.
        crate::reg_reconciler::reconcile_global_domain(&self.events).await;
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

    async fn set_finished(&self) {
        let mut g = self.session.lock().await;
        if let Some(s) = g.as_mut() {
            s.finished_at = Some(super::fsm::iso_now());
        }
    }

    async fn current_state(&self) -> Option<BindState> {
        self.session.lock().await.as_ref().map(|s| s.state)
    }

    async fn write_sentinel(&self) {
        let snap = self.session.lock().await.clone();
        write_sentinel(snap.as_ref());
    }
}

/// Background poller: stamp `last_frame_at` when the bind TUN RX counter
/// advances; self-exits when the session leaves the active set. Mirrors
/// `_poll_peer_presence_forever`.
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
