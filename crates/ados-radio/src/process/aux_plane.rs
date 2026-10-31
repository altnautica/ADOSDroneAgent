//! The auxiliary application stream planes.

use super::*;

impl RadioProcesses {
    /// True while the auxiliary application stream is open (both halves running).
    pub fn aux_active(&self) -> bool {
        self.aux_tx.is_some() && self.aux_rx.is_some()
    }

    /// The auxiliary tx PID, for the delta-counter TX-liveness watchdog.
    /// `None` while the aux stream is closed.
    pub fn aux_tx_pid(&self) -> Option<u32> {
        self.aux_tx.as_ref().and_then(|p| p.pid())
    }

    /// The auxiliary rx PID, for the aux receive-side watchdog. `None` while the
    /// aux stream is closed.
    pub fn aux_rx_pid(&self) -> Option<u32> {
        self.aux_rx.as_ref().and_then(|p| p.pid())
    }

    /// True while a plugin still wants the aux stream: it was opened and not
    /// closed. The retained settings outlive a failed restart, so a pair that is
    /// wanted but down is exactly the case the watchdog must keep retrying.
    pub fn aux_wanted(&self) -> bool {
        self.aux_settings.is_some()
    }

    /// True when either half of an open aux pair has exited. A cheap `try_wait`
    /// reap: an exited child keeps its PID until reaped, so the PID alone never
    /// reveals a crash.
    pub fn aux_exited(&mut self) -> bool {
        let tx_dead = self.aux_tx.as_mut().is_some_and(|p| !p.is_running());
        let rx_dead = self.aux_rx.as_mut().is_some_and(|p| !p.is_running());
        tx_dead || rx_dead
    }

    /// Open the auxiliary application stream: spawn the aux tx (radio_id 2) and
    /// aux rx (radio_id 3) pair on the SAME injection interface as the data and
    /// control planes, using the aux settings from `cfg`. ADDITIVE — it never
    /// stops or restarts the data plane or the control planes, so opening the aux
    /// stream cannot interrupt video or the FHSS HopAnnounce/HopAck transport.
    ///
    /// Idempotent: a second open while the stream is already up is a no-op
    /// (returns `true`) rather than spawning a duplicate pair. On a tx-spawn or
    /// rx-spawn failure the partially-spawned half is torn down and the stream is
    /// left closed (returns `false`), so a failed open never leaves a half-open
    /// stream. The retained aux settings are captured on success so a later
    /// whole-group respawn (a channel hop) re-opens the pair on the new channel.
    ///
    /// SAFE: nothing here ever runs at boot — this is reached only from an
    /// explicit open request.
    ///
    /// The operator dead-switch is honoured structurally: when `cfg.aux_enable`
    /// is false the stream is REFUSED (returns `false`) and no process is
    /// spawned, so the config-level disable is a real kill-switch rather than a
    /// false affordance. Safe-by-default is preserved: the flag is off unless an
    /// operator opts in.
    pub async fn open_aux_stream(&mut self, cfg: &WfbConfig) -> bool {
        if !cfg.aux_enable {
            tracing::warn!("aux_stream_open_refused: aux_enable is false");
            return false;
        }
        if self.aux_active() {
            return true;
        }
        let key_path = self.tx_key_path.clone();
        let aux_tx = match WfbProcess::spawn_aux_tx(
            &self.iface,
            cfg,
            &key_path,
            self.own_link_id,
            self.aux_stats.clone(),
        )
        .await
        {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(error = %e, "aux_tx_spawn_failed");
                return false;
            }
        };
        let aux_rx = match WfbProcess::spawn_aux_rx(
            &self.iface,
            cfg,
            &key_path,
            self.uplink_link_id,
            self.uplink.clone(),
        )
        .await
        {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(error = %e, "aux_rx_spawn_failed");
                // Tear down the tx half so a failed open never leaves a half-open
                // stream. `aux_tx` drops here, whose Drop killpg's the group.
                drop(aux_tx);
                return false;
            }
        };
        self.aux_tx = Some(aux_tx);
        self.aux_rx = Some(aux_rx);
        self.aux_settings = Some(AuxSettings::from_cfg(cfg));
        tracing::info!(
            tx_port = cfg.aux_tx_port,
            rx_port = cfg.aux_rx_port,
            "aux_stream_opened"
        );
        true
    }

    /// Close the auxiliary application stream: kill the aux tx + aux rx pair and
    /// drop the retained settings so a later respawn does NOT re-open it. ADDITIVE
    /// — it never touches the data plane or the control planes. Idempotent: a
    /// close while the stream is already down is a quiet no-op.
    pub async fn close_aux_stream(&mut self) {
        let was_open = self.aux_active();
        if let Some(mut tx) = self.aux_tx.take() {
            tx.kill().await;
        }
        if let Some(mut rx) = self.aux_rx.take() {
            rx.kill().await;
        }
        self.aux_settings = None;
        if was_open {
            tracing::info!("aux_stream_closed");
        }
    }

    /// Restart the auxiliary pair in place, reusing the retained aux settings.
    ///
    /// This is the liveness-watchdog recovery path: when the aux tx counter is
    /// flat (a silently-stalled transmitter) the watchdog asks for a restart, NOT
    /// a close — the plugin still wants the stream, so the retained settings are
    /// kept and the pair is re-spawned on the same ports/FEC/MCS. ADDITIVE: it
    /// only touches the aux pair, never the data plane or the control planes.
    ///
    /// A no-op (`false`) when the stream is not open (nothing to restart). On a
    /// re-spawn failure the aux pair is left down but the retained settings are
    /// KEPT, so the aux watchdog retries it on its next poll — the primary link is
    /// unaffected regardless of the additive aux pair's health.
    pub async fn restart_aux_stream(&mut self) -> bool {
        let Some(settings) = self.aux_settings else {
            return false;
        };
        // Kill the current pair without clearing the settings.
        if let Some(mut tx) = self.aux_tx.take() {
            tx.kill().await;
        }
        if let Some(mut rx) = self.aux_rx.take() {
            rx.kill().await;
        }
        let aux_cfg = settings.to_cfg();
        let key_path = self.tx_key_path.clone();
        let aux_tx = WfbProcess::spawn_aux_tx(
            &self.iface,
            &aux_cfg,
            &key_path,
            self.own_link_id,
            self.aux_stats.clone(),
        )
        .await;
        let aux_rx = WfbProcess::spawn_aux_rx(
            &self.iface,
            &aux_cfg,
            &key_path,
            self.uplink_link_id,
            self.uplink.clone(),
        )
        .await;
        match (aux_tx, aux_rx) {
            (Ok(tx), Ok(rx)) => {
                self.aux_tx = Some(tx);
                self.aux_rx = Some(rx);
                tracing::info!("aux_stream_restarted");
                true
            }
            (tx, rx) => {
                if let Err(e) = &tx {
                    tracing::warn!(error = %e, "restart_aux_tx_failed");
                }
                if let Err(e) = &rx {
                    tracing::warn!(error = %e, "restart_aux_rx_failed");
                }
                // Drop any half that spawned so a partial restart never leaves one
                // process running; keep the settings so a channel hop can retry.
                drop(tx);
                drop(rx);
                self.aux_tx = None;
                self.aux_rx = None;
                false
            }
        }
    }
}
