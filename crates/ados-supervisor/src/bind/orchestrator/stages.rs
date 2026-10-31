//! The bind stages proper: pre-flight, radio prep, tunnel bring-up, the wire
//! key exchange behind the peer-evidence gate, and key application.

use std::path::Path;

use super::peer::{peer_evidence_verdict, peer_poll, upstream_key_fresh};
use super::prep::prepare_injection_iface_for_bind;
use super::{AbortOnDrop, BindError, BindOrchestrator, PeerEvidence};
use crate::bind::bind_event::BindFailReason;
use crate::bind::fsm::BindState;
use crate::bind::{
    iface, keys, socat, BindRole, KEY_TRANSFER_TIMEOUT, RESTART_TIMEOUT, TUNNEL_WAIT_TIMEOUT,
    UPSTREAM_BIND_KEY, UPSTREAM_BIND_YAML, UPSTREAM_DRONE_KEY, UPSTREAM_GS_KEY, WFB_BIND_CLIENT_SH,
    WFB_BIND_SERVER_SH,
};

impl BindOrchestrator {
    /// End-to-end orchestration. Stages mirror the BindState transitions.
    pub(super) async fn run_session(
        &self,
        role: BindRole,
        peer_device_id: Option<String>,
        fleet: Option<keys::FleetIdentity>,
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
        if !crate::bind::on_path("socat") {
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
        let result = self.tunnel_and_transfer(role, peer_device_id, fleet).await;
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
        fleet: Option<keys::FleetIdentity>,
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
            // (setting a state is not proof of the state). Two
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
            // 2. Drone only: the upstream key file must have been deposited by
            //    THIS session's transfer (the wire protocol copies it fresh on
            //    a real exchange). A pre-existing file from an earlier bind
            //    means the conversation ended without a key transfer. A GS never
            //    reads this file, so short-circuit the stat off the drone path.
            let upstream = role.upstream_key();
            let key_fresh =
                role != BindRole::Drone || upstream_key_fresh(Path::new(upstream), session_start);

            // Both proofs run through one pure verdict so the invariant "never
            // declare Paired without a real peer" is unit-testable off-rig.
            match peer_evidence_verdict(role, frames_seen, key_fresh) {
                PeerEvidence::Verified => {}
                PeerEvidence::NoFrames => {
                    self.set_peer_verified(Some(false)).await;
                    return Err(BindError::with_reason(
                        "bind protocol completed but no peer traffic was decoded on \
                         the bind tunnel — refusing to pair without a real peer",
                        "transferring_keys",
                        BindFailReason::NoPeerProof,
                    ));
                }
                PeerEvidence::StaleKey => {
                    self.set_peer_verified(Some(false)).await;
                    return Err(BindError::with_reason(
                        format!(
                            "bind protocol completed but {upstream} was not \
                             refreshed by this session — no key was transferred"
                        ),
                        "transferring_keys",
                        BindFailReason::StaleKey,
                    ));
                }
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
                    reason: Some(BindFailReason::Timeout),
                });
            }
        };

        // Stage 6: apply the key + restart the normal wfb unit, own budget.
        self.set_state(BindState::RestartingServices).await;
        match tokio::time::timeout(
            RESTART_TIMEOUT,
            keys::apply_keypair(
                self.pm.clone(),
                &blob,
                role,
                peer_device_id.as_deref(),
                fleet,
            ),
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
        if !crate::bind::on_path("wfb_keygen") {
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
}
