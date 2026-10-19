//! Receive-plane interface bring-up: the regulatory gate, then monitor mode,
//! TX power and the rendezvous channel on every WFB adapter.

use ados_radio::reg_gate::Posture;

use crate::acquire::ChannelSetter;

use super::seams::IwChannelSetter;
use super::stats::GsRegSnapshot;
use super::WfbRxManager;

impl WfbRxManager {
    /// Bring every WFB adapter to the receive-ready state BEFORE the wfb_rx
    /// spawn, in the order the kernel requires:
    ///
    ///  1. **Regulatory gate first** (`iw reg set` + verify) under the shared
    ///     [`Posture`]: the same resolver the drone uses, so both ends set the
    ///     same domain and apply the same strictness (the unrestricted default
    ///     forces no domain and never blocks; a pinned region sets it and the
    ///     raw `reg_gate_strict` knob applies). The kernel maps the permitted
    ///     channel set and the per-channel TX-power ceiling when the driver
    ///     initialises, so a domain set after monitor-mode bring-up is too late.
    ///     A strict verify failure returns `Err` and the run loop parks in
    ///     `reg_blocked` rather than spawning the receive chain on a capped radio.
    ///  2. **Channel readiness assertion** (pinned region only): once the wiphy
    ///     is known, assert the rendezvous channel is in the enabled set and is
    ///     non-DFS. A mismatch returns `Err` under the strict gate. The
    ///     unrestricted posture skips it, as the drone does.
    ///  3. **Monitor mode** on every WFB adapter the run loop resolved.
    ///     `set_monitor_mode_verified` re-asserts it with the 4× verify retry and
    ///     guards the operator's control path so it can never sever the
    ///     management link.
    ///  4. **TX power** on each monitor interface, ramped only inside
    ///     `tx_power_max_dbm`. Without it the dongle runs at the driver default
    ///     (~17-20 dBm) and risks brownout on a host-VBUS USB topology.
    ///  5. **Channel set** to the rendezvous home on each adapter. wfb_rx
    ///     receives on whatever channel the netdevs are set to and combines
    ///     frames only from adapters on the same channel; it never retunes.
    ///
    /// The regulatory read-back (steps 1-2) runs against the transmit adapter;
    /// the domain is global, so it covers the receive-only adapters too.
    ///
    /// As a side effect it reads the wiphy's enabled channel set back so the
    /// acquirer can intersect its sweep candidates with what this domain permits.
    /// Returns `Err(RegError)` only when the regulatory gate fails under the
    /// strict gate; the monitor / TX-power / channel sub-steps remain best-effort
    /// (a failed sub-step is logged and the chain still spawns).
    pub async fn prepare_interfaces(&mut self) -> Result<(), ados_radio::adapter::RegError> {
        let tx_iface = self.interface.clone();
        let iface = tx_iface.as_str();
        // 1. Regulatory domain set + verify, before anything touches monitor
        // mode. A global per-phy call; needs no interface.
        let posture = Posture::load(std::path::Path::new(crate::paths::CONFIG_YAML));
        self.posture = Some(posture.clone());
        let strict = posture.strict;
        let unrestricted = posture.is_unrestricted();
        if let Err(e) = ados_radio::adapter::set_reg_domain(&posture.domain_to_set).await {
            if strict {
                tracing::error!(
                    interface = iface,
                    domain = %posture.domain_to_set,
                    reason = e.reason_code(),
                    "ground_wfb_reg_gate_blocked"
                );
                return Err(e);
            }
            tracing::warn!(
                interface = iface,
                domain = %posture.domain_to_set,
                reason = e.reason_code(),
                "ground_wfb_reg_gate_proceeding_best_effort"
            );
        }

        // Read back the regulatory-permitted channel set + the DFS set so the
        // acquirer can intersect its sweep and the gate can assert the home
        // channel. An empty enabled set means "could not determine".
        self.enabled_channels = ados_radio::adapter::enabled_channels(iface).await;
        let dfs = ados_radio::adapter::dfs_channels(iface).await;

        // 2. Assert the rendezvous channel is enabled + non-DFS now the wiphy is
        // known, before any monitor / channel operation. Skipped under the
        // unrestricted posture (the operator owns local compliance).
        let rendezvous_ch = self.config.rendezvous_channel();
        let ready = if unrestricted {
            Ok(())
        } else {
            ados_radio::adapter::assert_reg_ready(
                rendezvous_ch,
                &self.enabled_channels,
                &dfs,
                self.config.dfs_allowed,
            )
        };
        if let Err(e) = ready {
            if strict {
                tracing::error!(
                    interface = iface,
                    channel = rendezvous_ch,
                    reason = e.reason_code(),
                    "ground_wfb_reg_gate_channel_blocked"
                );
                return Err(e);
            }
            tracing::warn!(
                interface = iface,
                channel = rendezvous_ch,
                reason = e.reason_code(),
                "ground_wfb_reg_gate_channel_proceeding_best_effort"
            );
        }

        if self.enabled_channels.is_empty() {
            tracing::info!(interface = iface, "ground_wfb_enabled_channels_unknown");
        } else {
            tracing::info!(
                interface = iface,
                channels = ?self.enabled_channels,
                "ground_wfb_enabled_channels"
            );
        }

        // Capture the regulatory picture (live domain + verified + permitted set
        // + posture) so every sidecar write surfaces the same truth as the drone
        // side. The live read is a cheap `iw reg get` against the posture's
        // wanted domain.
        let reg_status = ados_radio::adapter::read_reg_status(&posture.wanted_domain).await;
        self.reg_snapshot = GsRegSnapshot {
            domain: reg_status.domain,
            verified: reg_status.verified,
            enabled_channels: self.enabled_channels.iter().copied().collect(),
            posture: Some(posture),
        };

        // Monitor mode, TX power and the channel land on every WFB adapter, so
        // no receive adapter is left managed or on the wrong channel.
        for iface in self.radio_ifaces.clone() {
            self.bring_up_iface(&iface, unrestricted).await;
        }
        Ok(())
    }

    /// Steps 3-5 of [`Self::prepare_interfaces`] on one adapter. Best-effort:
    /// a failed sub-step is logged and the chain still spawns.
    async fn bring_up_iface(&self, iface: &str, unrestricted: bool) {
        // 3. Monitor mode. 4× verify retry + the control-iface guard live in
        // the helper.
        if !ados_radio::adapter::set_monitor_mode_verified(iface, 4).await {
            tracing::warn!(interface = iface, "ground_wfb_monitor_not_verified");
        }

        // 4. TX power, before the wfb_rx spawn, held inside the ceiling.
        if ados_radio::adapter::set_tx_power_modal(
            iface,
            self.config.tx_power_dbm,
            self.config.tx_power_max_dbm,
            unrestricted,
        )
        .await
        .is_none()
        {
            tracing::warn!(
                interface = iface,
                requested = self.config.tx_power_dbm,
                max_dbm = self.config.tx_power_max_dbm,
                "ground_wfb_txpower_not_applied"
            );
        }

        // 5. Tune the netdev to the rendezvous home channel.
        if IwChannelSetter.set_channel(iface, self.channel).await {
            tracing::info!(
                interface = iface,
                channel = self.channel,
                "ground_wfb_channel_set"
            );
        } else {
            tracing::warn!(
                interface = iface,
                channel = self.channel,
                "ground_wfb_channel_set_failed"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bring_up_covers_every_wfb_adapter_and_skips_the_mesh_one() {
        use ados_radio::adapter::{resolve_roles, AdapterPlan, DetectedAdapter};
        use ados_radio::config::{RadioAdaptersConfig, WfbConfig};
        let adapters: Vec<DetectedAdapter> = (1..=3)
            .map(|i| DetectedAdapter {
                ifname: format!("wlan{i}"),
                mac: None,
                usb_path: None,
                driver: "rtl88x2eu".into(),
                chipset: "RTL8812EU".into(),
                verified: true,
                usb_speed_mbps: Some(480),
                usb_degraded: false,
            })
            .collect();
        // The mesh adapter (wlan3) is left to the mesh carrier.
        let plan = AdapterPlan::new(resolve_roles(
            &adapters,
            &RadioAdaptersConfig::default(),
            true,
        ))
        .unwrap();
        let mut m = WfbRxManager::new(WfbConfig::default());
        m.set_plan(&plan);
        assert_eq!(m.interface(), "wlan1");
        assert_eq!(m.rx_ifaces(), ["wlan1", "wlan2"]);
        assert_eq!(m.radio_ifaces(), ["wlan1", "wlan2"]);

        // A tx-only adapter is brought up as well as every receiver.
        let mut tx_only = resolve_roles(&adapters, &RadioAdaptersConfig::default(), false);
        tx_only[2].role = ados_radio::config::AdapterRole::Tx;
        m.set_plan(&AdapterPlan::new(tx_only).unwrap());
        assert_eq!(m.interface(), "wlan3");
        assert_eq!(m.rx_ifaces(), ["wlan1", "wlan2"]);
        assert_eq!(m.radio_ifaces(), ["wlan3", "wlan1", "wlan2"]);
    }
}
