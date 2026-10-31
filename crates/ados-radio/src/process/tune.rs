//! Live data-plane retunes: FEC, MCS, manual tier and TX power.

use super::*;

impl RadioProcesses {
    /// Push the retained data-plane trio onto the RUNNING `wfb_tx` over its
    /// wfb-ng 24.08 management socket. No process restart, so no video gap.
    ///
    /// Only ever sends the MCS index and the FEC pair. `set_radio` replaces the
    /// whole injected radiotap header, so [`crate::tx_cmd::RadioSettings::with_mcs`]
    /// re-states the pinned 20 MHz width and the spawn-time GI / STBC / LDPC /
    /// VHT values — a rate change can never silently retune the channel width.
    pub(super) async fn tx_cmd_apply(&self, what: Retune) -> Result<(), crate::tx_cmd::TxCmdError> {
        let client = crate::tx_cmd::TxCmdClient::for_radio_port(DATA_RADIO_PORT);
        if matches!(what, Retune::Fec | Retune::Both) {
            client.set_fec(self.data_fec_k, self.data_fec_n).await?;
        }
        if matches!(what, Retune::Modulation | Retune::Both) {
            client
                .set_radio(&crate::tx_cmd::RadioSettings::with_mcs(self.data_mcs_index))
                .await?;
        }
        Ok(())
    }

    /// Apply the already-updated retained trio to the live data plane, preferring
    /// the management socket and falling back to a respawn.
    ///
    /// The fallback is not optional: a `wfb_tx` too old for `-C`, a control socket
    /// that failed to bind, or a transmitter that has died all land here, and the
    /// operator's or the ladder's intent must still take effect. The respawn is
    /// the historical path and carries its historical cost (a 1-2 s video gap),
    /// which is why the two paths are counted separately.
    pub(super) async fn apply_retained(&mut self, what: Retune) -> bool {
        match self.tx_cmd_apply(what).await {
            Ok(()) => {
                self.applies.tx_cmd += 1;
                true
            }
            Err(e) => {
                self.applies.tx_cmd_failed += 1;
                tracing::warn!(
                    error = %e,
                    port = crate::tx_cmd::control_port(DATA_RADIO_PORT),
                    "data_plane_tx_cmd_failed_falling_back_to_respawn"
                );
                if self.respawn_data_tx().await {
                    self.applies.respawn += 1;
                    true
                } else {
                    false
                }
            }
        }
    }

    /// Apply a new Reed-Solomon `(k, n)` ratio to the live data plane.
    ///
    /// Applied on the RUNNING transmitter through the `wfb_tx` management socket
    /// (`set_fec`, which restarts only the FEC session — no video gap), falling
    /// back to kill-and-respawn when that socket does not answer. The two control
    /// planes and the stats RX carry their own fixed FEC and are never touched, so
    /// an FEC change does not interrupt HopAnnounce/HopAck or the link-quality
    /// stream. Returns `false` on an invalid ratio, or when both the live path and
    /// the respawn fallback failed; in that case the previous data tunables are
    /// restored in the retained state so a later respawn does not silently keep
    /// the rejected values, and the data plane is left dead for the supervisor to
    /// restart the whole group (the same fail-safe path the watchdog kills take).
    /// A no-op when the ratio already matches the running data plane.
    pub async fn set_fec(&mut self, fec_k: u8, fec_n: u8) -> bool {
        if !fec_ratio_valid(fec_k, fec_n) {
            tracing::warn!(k = fec_k, n = fec_n, "set_fec_invalid");
            return false;
        }
        if fec_k == self.data_fec_k && fec_n == self.data_fec_n {
            return true;
        }
        let old_k = self.data_fec_k;
        let old_n = self.data_fec_n;
        self.data_fec_k = fec_k;
        self.data_fec_n = fec_n;
        if self.apply_retained(Retune::Fec).await {
            tracing::info!(k = fec_k, n = fec_n, "set_fec_applied");
            true
        } else {
            self.data_fec_k = old_k;
            self.data_fec_n = old_n;
            tracing::warn!(k = fec_k, n = fec_n, "set_fec_apply_failed");
            false
        }
    }

    /// Apply a new MCS index to the live data plane (same live-then-respawn path
    /// as [`set_fec`](Self::set_fec), via `set_radio`). The accepted range is
    /// 0..=7 (the RTL8812EU range); `wfb_tx` rejects anything wider. A no-op when
    /// the index already matches the running data plane.
    ///
    /// This is the only radio knob varied at runtime. The channel width rides
    /// along pinned at 20 MHz and the TX power is left to the adapter's own
    /// ramp-until-accepted path — see [`crate::tx_cmd`].
    pub async fn set_mcs(&mut self, mcs: u8) -> bool {
        if !mcs_index_valid(mcs) {
            tracing::warn!(mcs, "set_mcs_out_of_range");
            return false;
        }
        if mcs == self.data_mcs_index {
            return true;
        }
        let old_mcs = self.data_mcs_index;
        self.data_mcs_index = mcs;
        if self.apply_retained(Retune::Modulation).await {
            tracing::info!(mcs, "set_mcs_applied");
            true
        } else {
            self.data_mcs_index = old_mcs;
            tracing::warn!(mcs, "set_mcs_apply_failed");
            false
        }
    }

    /// Pin a full manual link tier — the `(mcs_index, fec_k, fec_n)` trio — onto
    /// the live data plane in one operation.
    ///
    /// This is the manual half of the auto/manual tier control: the operator
    /// fixes the radio rate + redundancy and the adaptive controller is held off
    /// (the caller disables it). Only the halves that actually changed are pushed,
    /// so pinning a tier that differs in MCS alone does not needlessly restart the
    /// receiver's FEC session. Validates the FEC ratio and the MCS range up front;
    /// on an invalid input it changes nothing and returns `false`. A no-op
    /// (returns `true` without touching the radio) when the trio already matches
    /// the running data plane. When both the live path and the respawn fallback
    /// fail, the previous trio is restored in the retained state and the data
    /// plane is left dead for the supervisor to restart the whole group.
    pub async fn set_manual_tier(&mut self, mcs: u8, fec_k: u8, fec_n: u8) -> bool {
        if !mcs_index_valid(mcs) {
            tracing::warn!(mcs, "set_manual_tier_mcs_out_of_range");
            return false;
        }
        if !fec_ratio_valid(fec_k, fec_n) {
            tracing::warn!(k = fec_k, n = fec_n, "set_manual_tier_fec_invalid");
            return false;
        }
        let fec_changed = fec_k != self.data_fec_k || fec_n != self.data_fec_n;
        let mcs_changed = mcs != self.data_mcs_index;
        if !fec_changed && !mcs_changed {
            return true;
        }
        // At least one half changed, so this covers every reachable case.
        let what = match (fec_changed, mcs_changed) {
            (true, true) => Retune::Both,
            (true, false) => Retune::Fec,
            _ => Retune::Modulation,
        };
        let (old_mcs, old_k, old_n) = (self.data_mcs_index, self.data_fec_k, self.data_fec_n);
        self.data_mcs_index = mcs;
        self.data_fec_k = fec_k;
        self.data_fec_n = fec_n;
        if self.apply_retained(what).await {
            tracing::info!(mcs, k = fec_k, n = fec_n, "set_manual_tier_applied");
            true
        } else {
            self.data_mcs_index = old_mcs;
            self.data_fec_k = old_k;
            self.data_fec_n = old_n;
            tracing::warn!(mcs, k = fec_k, n = fec_n, "set_manual_tier_apply_failed");
            false
        }
    }

    /// Apply a TX power (dBm) to the live adapter via the kernel without a
    /// respawn — `iw dev <iface> set txpower` retunes the running radio in place.
    /// Returns the effective dBm the driver accepted (it can ramp UP from a
    /// rejected low request, never past `max_dbm`), or `None` when no in-budget
    /// step was accepted. `unrestricted` is the operating posture (it decides the
    /// muted-readback handling). The retained iface is the same one the control
    /// planes are injecting on, so the power change reaches the whole radio group
    /// at once.
    pub async fn apply_tx_power(&self, dbm: i8, max_dbm: i8, unrestricted: bool) -> Option<i8> {
        crate::adapter::set_tx_power_modal(&self.iface, dbm, max_dbm, unrestricted).await
    }
}
