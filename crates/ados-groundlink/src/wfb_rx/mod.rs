//! Ground-side `WfbRxManager`: the receive run loop.
//!
//! Spawns the three radio C subprocesses the receive side needs and drives the
//! liveness machinery from chunk 2:
//!   - **data RX** `wfb_rx -p 0 -c 127.0.0.1 -u 5599 -K <rx.key> -l 1000 <iface>`
//!     decodes the FEC video stream to the internal fan-out port; stdout carries
//!     the per-second `PKT`/`RX_ANT` stats lines the link monitor parses.
//!   - **rx control** `wfb_rx -p 1 -c 127.0.0.1 -u 5803 -K <rx.key> -l 1000
//!     <iface>` decodes inbound HopAnnounce/Presence frames onto the listener's
//!     port. NOTE: the GS uses **5803** here, not the drone side's 5810. The
//!     ports are mirrored between rigs, so the drone-side `ados_radio::process`
//!     arg builders are NOT reused verbatim.
//!   - **tx control** `wfb_tx -p 1 -u 5810 -K <rx.key> -k 1 -n 2 -B 20 -M <mcs>
//!     <iface>` transmits HopAck/Presence back over the air from the loopback
//!     ingress 5810.
//!
//! Process-group isolation (`setsid`/`killpg`) follows the same discipline as
//! `ados_radio::process::WfbProcess` so the orphan-child bug class is
//! structurally impossible here too; the GS-specific spawn lives in
//! `process_spawn::GsWfbProcess` because the drone crate exposes no generic
//! spawn entry point and the GS arg sets differ. The run loop wires the stats
//! stream into the valid-packet watchdog (its counter/presence/acquirer seams)
//! and the acquirer, writes the ground `wfb-stats.json` sidecar, and runs the
//! stdout-silence zombie watchdog.
//!
//! Module layout:
//! - `args`: the receive-chain arg builders, ports, lifecycle-state strings,
//!   `DEFAULT_REG_DOMAIN`, and the shared rx-key path.
//! - `seams`: the shared counter / channel setter / clock / data-RX handle that
//!   implement the watchdog + acquirer seams, plus the live-channel read.
//! - `stats`: `build_gs_stats` + the reg-blocked sidecar writer.
//! - `loops`: the stdout stats reader + the stdout-silence zombie watchdog.
//! - this module root: the `WfbRxManager` (config + interface bring-up + chain
//!   spawn + watchdog assembly).

use std::collections::BTreeSet;
use std::sync::Arc;

use ados_radio::config::WfbConfig;

use crate::acquire::{ChannelAcquirer, ChannelSetter};
use crate::presence::GsPresenceCache;
use crate::process_spawn::{GsWfbProcess, Stdout};
use crate::watchdog::{Clock, FileLockedChannelHint, LockedChannelHint, ValidPacketWatchdog};

pub mod args;
pub mod loops;
pub mod seams;
pub mod stats;

pub use args::{
    data_rx_args, gs_rx_control_args, gs_tx_control_args, DATA_RX_PORT, DEFAULT_REG_DOMAIN,
    RX_CONTROL_PORT, RX_HEALTH_POLL_INTERVAL_S, STATE_ACTIVE, STATE_REG_BLOCKED, STATE_SEARCHING,
    TX_CONTROL_PORT,
};
pub use loops::{stats_reader_loop, zombie_watchdog};
pub use seams::{DataRxHandle, IwChannelSetter, SharedValidCounter, SystemClock};
pub use stats::{build_gs_stats, write_reg_blocked_sidecar, GsChannelTruth, GsRegSnapshot};

// Re-exported so the run loop can build the shared receive-health seam through
// the same module that owns the stats reader.
pub use crate::watchdog::SharedRxHealth;

use args::rx_key_path;

/// The receive manager. Holds the config + the shared liveness state the run
/// loop wires together.
pub struct WfbRxManager {
    config: WfbConfig,
    interface: String,
    channel: u8,
    selected_chipset: Option<String>,
    adapter_injection_ok: bool,
    /// The regulatory-permitted channel set for the receive interface, resolved
    /// once `prepare_interface` has applied the domain + read the wiphy back.
    /// Empty until then (and on a board where the set cannot be determined); the
    /// acquirer treats an empty set as "do not restrict".
    enabled_channels: BTreeSet<u8>,
    /// The regulatory picture (live domain + verified + permitted set) resolved
    /// by `prepare_interface`, surfaced on every sidecar write so the panel sees
    /// the same regulatory truth as the drone. Default (unknown) until prepared.
    reg_snapshot: GsRegSnapshot,
}

impl WfbRxManager {
    pub fn new(config: WfbConfig) -> Self {
        let channel = config.channel;
        let interface = config.interface.clone();
        Self {
            config,
            interface,
            channel,
            selected_chipset: None,
            adapter_injection_ok: false,
            enabled_channels: BTreeSet::new(),
            reg_snapshot: GsRegSnapshot::default(),
        }
    }

    /// The regulatory picture resolved by `prepare_interface`. Default (unknown)
    /// until that runs.
    pub fn reg_snapshot(&self) -> &GsRegSnapshot {
        &self.reg_snapshot
    }

    /// The rendezvous (meeting) channel for this receive plane — the operator's
    /// home, or the optional rendezvous pin. Both rigs derive it identically.
    pub fn rendezvous_channel(&self) -> u8 {
        self.config.rendezvous_channel()
    }

    pub fn interface(&self) -> &str {
        &self.interface
    }

    pub fn channel(&self) -> u8 {
        self.channel
    }

    /// The regulatory-permitted channel set resolved by `prepare_interface`.
    /// Empty until that runs, or on a board where the wiphy channel list could
    /// not be read.
    pub fn enabled_channels(&self) -> &BTreeSet<u8> {
        &self.enabled_channels
    }

    /// Set the selected-adapter identity (the HAL detect path stays in Python;
    /// the run loop sets these when an adapter is chosen so the sidecar carries
    /// the same stranded-link signal the manager holds).
    pub fn set_adapter(&mut self, chipset: Option<String>, injection_ok: bool) {
        self.selected_chipset = chipset;
        self.adapter_injection_ok = injection_ok;
    }

    /// Adopt the receive interface the run loop resolved (the auto-detected RTL
    /// injection adapter, or the operator's `video.wfb.interface` override). The
    /// receive chain and the sidecar both read `self.interface`, so the run loop
    /// sets it here once an adapter is chosen instead of relying on a config
    /// value that is empty when no external detector populated it.
    pub fn set_interface(&mut self, iface: String) {
        self.interface = iface;
    }

    /// Bring `iface` to the receive-ready state BEFORE the wfb_rx spawn, in the
    /// order the kernel requires:
    ///
    ///  1. **Regulatory gate first** (`iw reg set` + verify). The kernel maps the
    ///     permitted channel set and the per-channel TX-power ceiling when the
    ///     driver initialises, so a domain set after monitor-mode bring-up is too
    ///     late and leaves the home channel (149 / 5745 MHz) capped to the
    ///     startup domain's limits — zero injected frames, the -100 dBm "not
    ///     permitted" sentinel. This is a global per-phy call, so it needs no
    ///     interface; an empty/None config value falls back to the safe default.
    ///     Under the strict (default) gate a verify failure returns `Err` and the
    ///     run loop parks in `reg_blocked` rather than spawning the receive chain
    ///     on a capped radio; the lab escape hatch proceeds best-effort.
    ///  2. **Channel readiness assertion**: once the wiphy is known, assert the
    ///     rendezvous channel is in the enabled set and is non-DFS. A mismatch
    ///     returns `Err` under the strict gate.
    ///  3. **Monitor mode** on the interface the run loop resolved (the
    ///     auto-detected RTL injection adapter or the operator's config
    ///     override). `set_monitor_mode_verified` re-asserts it with the 4×
    ///     verify retry and guards the operator's control path so it can never
    ///     sever the management link.
    ///  4. **TX power** on the monitor interface. Without it the dongle runs at
    ///     the driver default (~17-20 dBm) and risks brownout on a host-VBUS USB
    ///     topology — the same guard the air side applies.
    ///  5. **Channel set** to the rendezvous home. wfb_rx receives on whatever
    ///     channel the netdev is set to; it does not retune itself.
    ///
    /// As a side effect it reads the wiphy's enabled channel set back so the
    /// acquirer can intersect its sweep candidates with what this domain permits.
    /// Returns `Err(RegError)` only when the regulatory gate fails under the
    /// strict gate; the monitor / TX-power / channel sub-steps remain best-effort
    /// (a failed sub-step is logged and the chain still spawns).
    pub async fn prepare_interface(
        &mut self,
        iface: &str,
    ) -> Result<(), ados_radio::adapter::RegError> {
        // 1. Regulatory domain set + verify, before anything touches monitor
        // mode. A global per-phy call; needs no interface.
        let reg_domain = self
            .config
            .reg_domain
            .clone()
            .filter(|d| !d.is_empty())
            .unwrap_or_else(|| DEFAULT_REG_DOMAIN.to_string());
        let strict = self.config.reg_gate_strict;
        if let Err(e) = ados_radio::adapter::set_reg_domain(&reg_domain).await {
            if strict {
                tracing::error!(
                    interface = iface,
                    domain = %reg_domain,
                    reason = e.reason_code(),
                    "ground_wfb_reg_gate_blocked"
                );
                return Err(e);
            }
            tracing::warn!(
                interface = iface,
                domain = %reg_domain,
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
        // known, before any monitor / channel operation. Both rigs derive the
        // rendezvous channel identically from the operator config.
        let rendezvous_ch = self.config.rendezvous_channel();
        if let Err(e) = ados_radio::adapter::assert_reg_ready(
            rendezvous_ch,
            &self.enabled_channels,
            &dfs,
            self.config.dfs_allowed,
        ) {
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

        // Capture the regulatory picture (live domain + verified + permitted set)
        // so every sidecar write surfaces the same truth as the drone side. The
        // live read is a cheap `iw reg get`; the wanted domain is the resolved
        // `reg_domain` the gate just applied.
        let reg_status = ados_radio::adapter::read_reg_status(&reg_domain).await;
        self.reg_snapshot = GsRegSnapshot {
            domain: reg_status.domain,
            verified: reg_status.verified,
            enabled_channels: self.enabled_channels.iter().copied().collect(),
        };

        // 3. Monitor mode on the config-supplied interface (the Python override
        // path). 4× verify retry + the control-iface guard live in the helper.
        if !ados_radio::adapter::set_monitor_mode_verified(iface, 4).await {
            tracing::warn!(interface = iface, "ground_wfb_monitor_not_verified");
        }

        // 4. TX power, before the wfb_rx spawn.
        if ados_radio::adapter::set_tx_power(iface, self.config.tx_power_dbm)
            .await
            .is_none()
        {
            tracing::warn!(
                interface = iface,
                requested = self.config.tx_power_dbm,
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

        Ok(())
    }

    /// Spawn the receive subprocesses for `iface` and return their handles. The
    /// data RX has its stdout piped for the stats reader; both control planes
    /// log stderr to truncated files via `WfbProcess`. Adapter detection and
    /// monitor-mode setup stay in Python and are assumed already applied to
    /// `iface`.
    pub async fn spawn_receive_chain(
        &self,
        iface: &str,
    ) -> std::io::Result<(GsWfbProcess, GsWfbProcess, GsWfbProcess)> {
        let rx_key = rx_key_path();
        // Data RX: stdout piped for the stats reader, in its own process group.
        let data_rx = GsWfbProcess::spawn(
            "wfb_rx",
            &data_rx_args(iface, &rx_key, DATA_RX_PORT),
            Stdout::Piped,
            None,
        )
        .await?;
        let rx_control = GsWfbProcess::spawn(
            "wfb_rx",
            &gs_rx_control_args(iface, &rx_key),
            Stdout::Null,
            Some("/run/ados/wfb-gs-rx-control.log"),
        )
        .await?;
        let tx_control = GsWfbProcess::spawn(
            "wfb_tx",
            &gs_tx_control_args(iface, &rx_key, self.config.mcs_index),
            Stdout::Null,
            Some("/run/ados/wfb-gs-tx-control.log"),
        )
        .await?;
        Ok((data_rx, rx_control, tx_control))
    }

    /// Build the chunk-2 watchdog for this receive generation, wired to the
    /// shared counter + presence cache + a fresh acquirer over the supplied
    /// channel setter. The caller owns the lifecycle (one generation per
    /// `wfb_rx` spawn, matching the Python "fresh acquirer each run").
    pub fn build_watchdog(
        &self,
        counter: SharedValidCounter,
        presence: GsPresenceCache,
        rx: Arc<DataRxHandle>,
        clock: Arc<dyn Clock>,
        setter: Arc<dyn ChannelSetter>,
        hint: Arc<dyn LockedChannelHint>,
    ) -> ValidPacketWatchdog {
        // Feed the regulatory-permitted channel set resolved by
        // `prepare_interface` so the sweep skips channels this domain forbids
        // (those fail `iw set channel` with -22 and waste a dwell). An empty set
        // (board where it could not be read) is passed through as "do not
        // restrict" by the acquirer.
        let enabled = if self.enabled_channels.is_empty() {
            None
        } else {
            Some(self.enabled_channels.clone())
        };
        let acquirer = ChannelAcquirer::new(
            &self.interface,
            &self.config.band,
            Arc::new(counter),
            setter,
            crate::acquire::DWELL_SECONDS,
            crate::acquire::MAX_SWEEP_ROUNDS,
            enabled,
        );
        ValidPacketWatchdog::new(
            &self.interface,
            self.channel,
            self.config.channel, // immutable home
            clock,
            rx,
            Arc::new(presence),
            hint,
            acquirer,
        )
    }
}

/// The production locked-channel hint sink (atomic single-int tmpfs write).
pub fn default_hint() -> Arc<dyn LockedChannelHint> {
    Arc::new(FileLockedChannelHint)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manager_enabled_channels_default_empty() {
        // Until prepare_interface runs, the permitted set is empty (the acquirer
        // reads empty as "do not restrict").
        let m = WfbRxManager::new(WfbConfig::default());
        assert!(m.enabled_channels().is_empty());
    }
}
