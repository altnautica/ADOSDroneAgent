//! The bind stages proper: pre-flight, radio prep, tunnel bring-up, the wire
//! key exchange behind the peer-evidence gate, and key application.

use super::peer::{counter_advanced, peer_evidence_verdict, peer_poll};
use super::{AbortOnDrop, BindError, BindOrchestrator, PeerEvidence, SocatFailure};
use crate::bind::bind_event::BindFailReason;
use crate::bind::fsm::{BindState, PendingApply};
use crate::bind::{
    keys, socat, BindRole, KEY_TRANSFER_TIMEOUT, TUNNEL_WAIT_TIMEOUT, UPSTREAM_BIND_KEY,
    UPSTREAM_BIND_YAML, WFB_BIND_CLIENT_SH, WFB_BIND_SERVER_SH,
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
            if !self.host.file_present(path) {
                return Err(BindError::new(format!(
                    "upstream wfb-ng artifact missing: {path}. Reinstall via \
                     install.sh to provision /etc/bind.key and /etc/bind.yaml."
                )));
            }
        }
        for script in [WFB_BIND_SERVER_SH, WFB_BIND_CLIENT_SH] {
            if !self.host.file_present(script) {
                return Err(BindError::new(format!(
                    "upstream wfb-ng helper missing: {script}. wfb-ng package \
                     must be installed via install.sh."
                )));
            }
        }
        if !self.host.on_path("socat") {
            return Err(BindError::new(
                "socat binary not found on PATH. Install via `apt install \
                 socat` or rerun install.sh.",
            ));
        }

        // GS-only: generate the fresh keypair BEFORE stopping the wfb unit so a
        // keygen failure leaves the rig with its normal service still running.
        if role == BindRole::Gs {
            self.host.generate_keypair().await.map_err(BindError::new)?;
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
        self.host.reconcile_injection_driver().await;

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
        let prep = self.host.prepare_injection_iface().await;
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
        self.host.reconcile_reg_domain(&self.events).await;

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
        if !self
            .host
            .wait_for_iface(bind_iface, TUNNEL_WAIT_TIMEOUT)
            .await
        {
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
        self.host.reconcile_reg_domain(&self.events).await;

        self.set_state(BindState::WaitingPeer).await;

        // The counter as the tunnel opens: the gate compares against it, so a
        // frame that lands after the poller's last sample still counts.
        let rx_at_open = self.host.rx_packets(bind_iface);

        // Background poller: stamps last_frame_at when the bind TUN RX counter
        // advances. Aborted on return via the guard's Drop.
        let _poll_guard = AbortOnDrop(tokio::spawn(peer_poll(
            self.session.clone(),
            self.host.clone(),
            bind_iface.to_string(),
        )));

        // Reference point for the drone-side key-freshness gate: the upstream
        // key must have been deposited AFTER this instant to count as this
        // session's transfer (a leftover from any earlier bind has an older
        // mtime and must not satisfy the success check).
        let session_start = self.host.wall_now();

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
            //    The poller samples once a second, and a fast exchange can
            //    start and finish inside one sample period, so the counter is
            //    also read here and compared with its value at tunnel open.
            let polled = {
                let g = self.session.lock().await;
                g.as_ref().and_then(|s| s.last_frame_at).is_some()
            };
            let frames_seen =
                polled || counter_advanced(rx_at_open, self.host.rx_packets(bind_iface));
            // 2. Drone only: the upstream key file must have been deposited by
            //    THIS session's transfer (the wire protocol copies it fresh on
            //    a real exchange). A pre-existing file from an earlier bind
            //    means the conversation ended without a key transfer. A GS never
            //    reads this file, so short-circuit the stat off the drone path.
            let upstream = role.upstream_key();
            let key_fresh =
                role != BindRole::Drone || self.host.file_fresh(upstream, session_start);

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

            if !self.host.file_present(upstream) {
                return Err(BindError::with_phase(
                    format!(
                        "bind protocol completed but {upstream} not present. \
                         Upstream may have failed silently."
                    ),
                    "applying_keys",
                ));
            }
            self.host.read_file(upstream).map_err(|e| {
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

        // Stage 6: apply the key + restart the normal wfb unit. The apply is
        // the commit point: it runs to completion in its own task (every
        // service-manager call in it is bounded by the backend), so it is
        // awaited to the end rather than raced against a timer that would
        // report a failure while the new key still goes live. The apply it
        // commits to goes into the session record (and the sentinel, with the
        // transition below) first, so a supervisor killed from here on finishes
        // it at restart instead of leaving the peer paired alone.
        if let Some(fingerprint) = keys::blob_fingerprint(&blob) {
            self.set_pending_apply(PendingApply { fingerprint, fleet })
                .await;
        }
        self.set_state(BindState::RestartingServices).await;
        if let Err(e) = keys::apply_keypair(
            self.pm.clone(),
            self.host.clone(),
            &blob,
            role,
            peer_device_id.as_deref(),
            fleet,
        )
        .await
        {
            return Err(BindError::with_phase(e, "restarting_services"));
        }

        // Best-effort fingerprint of the freshly applied key.
        let fp = self.host.read_public_fingerprint(role.key_path()).ok();
        self.set_fingerprint(fp).await;

        self.set_state(BindState::Paired).await;
        tracing::info!(role = role.as_str(), "bind_session_paired");
        Ok(())
    }

    /// Drone side: listen on the tunnel rendezvous for the gs to connect.
    async fn run_drone_server(&self) -> Result<(), BindError> {
        if !self.host.file_present(WFB_BIND_SERVER_SH) {
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
        if !self.host.file_present(WFB_BIND_CLIENT_SH) {
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
        let (rc, stderr) = self.host.run_socat(args).await.map_err(|e| {
            let message = match e {
                SocatFailure::Spawn(e) => format!("failed to spawn {what}: {e}"),
                SocatFailure::Run(e) => format!("{what} run failed: {e}"),
            };
            BindError::with_phase(message, "transferring_keys")
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
