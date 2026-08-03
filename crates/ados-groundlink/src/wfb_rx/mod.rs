//! Ground-side `WfbRxManager`: the receive run loop.
//!
//! Spawns the radio C subprocesses the receive side needs and drives the
//! liveness machinery from chunk 2. The receive half is PER FLEET SLOT: the
//! ground station runs one video / aux / control receiver for every registered
//! drone, all bound to the same interface. That is legal because `wfb_rx` opens
//! a promiscuous, non-exclusive pcap handle and compiles a per-instance kernel
//! BPF on `channel_id` (`vendor/wfb-ng/src/rx.cpp:70,84`), so N drones need N
//! receivers, not N radios. The transmit half is SINGULAR: one control TX and
//! one aux TX keyed to the ground station's own slot 0, which every drone
//! receives, so a fleet-wide command is one transmission rather than N.
//!
//! Per slot S (`link_id(fleet_id, S)`):
//!   - **data RX** `wfb_rx -p 0 -i <link> -c 127.0.0.1 -u <VIDEO_RX_PORT_BASE+S>
//!     -K <rx.key> -l 1000 <iface>` decodes the FEC video stream. Only the
//!     PRIMARY slot's instance pipes stdout: its per-second `PKT`/`RX_ANT`
//!     stats lines drive the link monitor and its liveness anchors the
//!     generation.
//!   - **rx control** `wfb_rx -p 1 -i <link> -c 127.0.0.1 -u
//!     <CONTROL_RX_PORT_BASE+S> …` decodes inbound HopAnnounce/Presence frames
//!     onto the port the presence listener binds for that slot.
//!   - **aux RX** `wfb_rx -p 2 -i <link> -c 127.0.0.1 -u <AUX_RX_PORT_BASE+S> …`
//!     decodes the general-purpose application lane the drone radiates on
//!     `-p 2`, delivering to the port that slot's aux consumer binds. This is
//!     the lane anything other than video and the tiny control frames rides, so
//!     it is spawned unconditionally: a linked ground station has to be able to
//!     receive whatever the drone sends it.
//!
//! Ground-station transmit, once per fleet (`link_id(fleet_id, 0)`):
//!   - **tx control** `wfb_tx -p 1 -i <link> -u 5810 -K <rx.key> -k 1 -n 2
//!     -B 20 -M <mcs> <iface>` transmits HopAck/Presence back over the air from
//!     the loopback ingress 5810.
//!   - **aux TX** `wfb_tx -p 3 -i <link> -u 5602 …` carries the uplink half of
//!     the aux pair.
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
    aux_rx_port, control_rx_port, data_rx_args, gs_atlas_rx_args, gs_aux_tx_args,
    gs_rx_control_args, gs_tx_control_args, video_rx_port, AUX_TX_PORT, DEFAULT_REG_DOMAIN,
    RX_HEALTH_POLL_INTERVAL_S, STATE_ACTIVE, STATE_BLOCKED_UNPAIRED, STATE_NO_INJECTION,
    STATE_REG_BLOCKED, STATE_SEARCHING, TX_CONTROL_PORT,
};
pub use loops::{stats_reader_loop, zombie_watchdog};
pub use seams::{DataRxHandle, IwChannelSetter, SharedValidCounter, SystemClock};
pub use stats::{
    build_gs_stats, write_blocked_unpaired_sidecar, write_no_injection_sidecar,
    write_reg_blocked_sidecar, GsAdapterInfo, GsChannelTruth, GsRegSnapshot,
};

// Re-exported so the run loop can build the shared receive-health seam through
// the same module that owns the stats reader.
pub use crate::watchdog::SharedRxHealth;

use args::rx_key_path;

/// One generation's primary receive chain: the anchor slot's three receivers
/// plus the fleet's two single ground transmitters.
///
/// `video`'s stdout is piped — it is both the stats stream and the liveness
/// anchor, so the generation ends when it exits. Every other field is held only
/// so its `Drop` (killpg) tears the process group down with the generation.
pub struct PrimaryChain {
    /// The fleet slot this chain is anchored on (the lowest registered slot).
    pub slot: u8,
    pub video: GsWfbProcess,
    pub aux: GsWfbProcess,
    pub control: GsWfbProcess,
    pub tx_control: GsWfbProcess,
    pub aux_tx: GsWfbProcess,
}

/// The receive trio for one non-primary fleet slot. Additive to the primary
/// chain: spawned when the slot registers, dropped (killpg) when it releases or
/// when the generation ends.
///
/// `video` is OPTIONAL and spawned apart from `aux`/`control`: only the HERO
/// slot's video egress is read (every other slot's video port has no consumer),
/// so a non-hero secondary slot decodes a full-FEC stream nothing reads — pure
/// waste on the ground adapter. `aux` (the general-purpose application lane) and
/// `control` (HopAnnounce/Presence) stay UNCONDITIONAL: a linked ground station
/// must receive whatever a drone sends it on those lanes regardless of whether it
/// is the hero. Video is brought back when the slot becomes the hero, driven off
/// the 1 s hero poll — see `main.rs`'s video reconcile.
pub struct SlotReceivers {
    pub slot: u8,
    /// The video receiver (p0), or `None` when this slot is not the hero and its
    /// video port is therefore unwatched. Dropping a set receiver killpg's it.
    pub video: Option<GsWfbProcess>,
    pub aux: GsWfbProcess,
    pub control: GsWfbProcess,
}

impl SlotReceivers {
    /// Whether this slot currently decodes video.
    pub fn decoding_video(&self) -> bool {
        self.video.is_some()
    }
}

/// The fleet slots this ground station receives on, ascending.
///
/// An EMPTY registry resolves to the single default drone slot rather than the
/// empty set: a ground station with no registered drone must still hear the
/// first drone provisioned into the fleet, and `FleetRegistry::allocate` issues
/// slot 1 first, so slot 1 is where that drone will be. Returning nothing would
/// leave a freshly-imaged ground station structurally deaf with no error.
pub fn fleet_slots(registry: &crate::fleet::FleetRegistry) -> Vec<u8> {
    let slots: Vec<u8> = registry.slots().map(|s| s.slot).collect();
    if slots.is_empty() {
        vec![FIRST_DRONE_SLOT]
    } else {
        slots
    }
}

/// The slot `FleetRegistry::allocate` issues first, and the slot an empty
/// registry falls back to.
pub const FIRST_DRONE_SLOT: u8 = 1;

/// The receive manager. Holds the config + the shared liveness state the run
/// loop wires together.
pub struct WfbRxManager {
    config: WfbConfig,
    interface: String,
    channel: u8,
    /// The selected receive adapter's facts, surfaced on every sidecar write.
    adapter: GsAdapterInfo,
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
            adapter: GsAdapterInfo::default(),
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

    /// Set the selected-adapter facts (chipset, injection verdict, USB link
    /// health) so every sidecar write carries the same stranded-link signal the
    /// manager holds.
    pub fn set_adapter(&mut self, adapter: GsAdapterInfo) {
        self.adapter = adapter;
    }

    /// The selected-adapter facts resolved by the run loop. Default (unknown
    /// chipset, no injection claim, no USB reading) until an adapter is chosen.
    pub fn adapter(&self) -> &GsAdapterInfo {
        &self.adapter
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

    /// The fleet this receive plane belongs to. Every `link_id` the chain is
    /// keyed with is derived from it.
    pub fn fleet_id(&self) -> u16 {
        self.config.fleet_id
    }

    /// Spawn the receive chain anchored on `primary_slot`, plus the two single
    /// ground transmitters.
    ///
    /// The primary slot's video RX has its stdout piped: it is the generation's
    /// stats stream AND its liveness anchor (the channel acquirer sweeps for
    /// this link, and the generation ends when it exits). Every other process —
    /// the primary's aux + control receivers and the two transmitters — is
    /// additive and torn down with the generation.
    ///
    /// The two transmitters are keyed to the ground station's own slot 0 and
    /// there is exactly ONE of each regardless of fleet size: every drone's
    /// uplink receiver listens on that same `channel_id`, so a fleet-wide
    /// command is one transmission, not N.
    ///
    /// Adapter detection and monitor-mode setup are assumed already applied to
    /// `iface`.
    pub async fn spawn_receive_chain(
        &self,
        iface: &str,
        primary_slot: u8,
    ) -> std::io::Result<PrimaryChain> {
        let rx_key = rx_key_path();
        let fleet = self.config.fleet_id;
        let ground = ados_radio::config::link_id(fleet, ados_radio::config::SLOT_GROUND);
        let drone = ados_radio::config::link_id(fleet, primary_slot);

        // Data RX: stdout piped for the stats reader, in its own process group.
        let video = GsWfbProcess::spawn(
            "wfb_rx",
            &data_rx_args(iface, &rx_key, video_rx_port(primary_slot), drone),
            Stdout::Piped,
            None,
        )
        .await?;
        let control = GsWfbProcess::spawn(
            "wfb_rx",
            &gs_rx_control_args(iface, &rx_key, control_rx_port(primary_slot), drone),
            Stdout::Null,
            Some("/run/ados/wfb-gs-rx-control.log"),
        )
        .await?;
        // Aux application lane. The drone half has always been complete
        // (`wfb_tx -p 2` behind the radio-aux socket) but this receiver was
        // written, unit-tested, and then never spawned, so nothing on the
        // ground ever decoded p2 and the lane carried no traffic in either
        // direction. It is spawned unconditionally rather than behind the aux
        // consumer's enable flag: a linked ground station must be able to
        // receive whatever the drone chooses to send, and an idle wfb_rx with
        // no inbound frames costs essentially nothing.
        let aux = GsWfbProcess::spawn(
            "wfb_rx",
            &gs_atlas_rx_args(iface, &rx_key, aux_rx_port(primary_slot), drone),
            Stdout::Null,
            Some("/run/ados/wfb-gs-aux-rx.log"),
        )
        .await?;
        let tx_control = GsWfbProcess::spawn(
            "wfb_tx",
            &gs_tx_control_args(iface, &rx_key, self.config.mcs_index, ground),
            Stdout::Null,
            Some("/run/ados/wfb-gs-tx-control.log"),
        )
        .await?;
        // Aux UPLINK. The counterpart to the receiver above, and the half that
        // made the lane one-directional: the drone has always listened on
        // radio_id 3, but nothing on the ground ever transmitted there, so a
        // client's request could reach the ground station and go no further.
        // Spawned unconditionally for the same reason as the receiver — a
        // linked ground station must be able to answer the drone it can hear,
        // and an idle wfb_tx with no ingress traffic costs essentially nothing.
        let aux_tx = GsWfbProcess::spawn(
            "wfb_tx",
            &gs_aux_tx_args(
                iface,
                &rx_key,
                self.config.mcs_index,
                ground,
                self.config.aux_tx_port,
            ),
            Stdout::Null,
            Some("/run/ados/wfb-gs-aux-tx.log"),
        )
        .await?;
        Ok(PrimaryChain {
            slot: primary_slot,
            video,
            aux,
            control,
            tx_control,
            aux_tx,
        })
    }

    /// Spawn the receive trio for one NON-primary fleet slot: aux (p2) and
    /// control (p1) unconditionally, plus the video receiver (p0) only when
    /// `with_video` is true. Each is keyed to that slot's `link_id` and decodes
    /// to that slot's loopback egress ports.
    ///
    /// The split is the unwatched-video-slots fix: a non-hero slot's video port
    /// is never read (only the hero's egress is fanned out), so decoding its
    /// full-FEC stream is pure adapter waste. `aux` + `control` stay
    /// unconditional — a linked ground station must hear whatever a drone sends
    /// on those lanes. Video is brought back via [`Self::spawn_slot_video`] when
    /// the slot becomes the hero, driven off the 1 s hero poll.
    ///
    /// All instances bind the SAME `iface` as the primary chain. That is legal,
    /// not a workaround: `wfb_rx` opens a promiscuous, non-exclusive pcap handle
    /// and compiles a per-instance kernel BPF on `channel_id`
    /// (`vendor/wfb-ng/src/rx.cpp:70,84`), so each receiver sees only its own
    /// drone's frames and N drones need N receivers, not N radios.
    ///
    /// stdout is discarded: only the primary slot's video RX drives the stats
    /// reader and the channel acquirer.
    pub async fn spawn_slot_receivers(
        &self,
        iface: &str,
        slot: u8,
        with_video: bool,
    ) -> std::io::Result<SlotReceivers> {
        let rx_key = rx_key_path();
        let drone = ados_radio::config::link_id(self.config.fleet_id, slot);
        let video = if with_video {
            Some(
                GsWfbProcess::spawn(
                    "wfb_rx",
                    &data_rx_args(iface, &rx_key, video_rx_port(slot), drone),
                    Stdout::Null,
                    Some(&format!("/run/ados/wfb-gs-video-rx-{slot}.log")),
                )
                .await?,
            )
        } else {
            None
        };
        let aux = GsWfbProcess::spawn(
            "wfb_rx",
            &gs_atlas_rx_args(iface, &rx_key, aux_rx_port(slot), drone),
            Stdout::Null,
            Some(&format!("/run/ados/wfb-gs-aux-rx-{slot}.log")),
        )
        .await?;
        let control = GsWfbProcess::spawn(
            "wfb_rx",
            &gs_rx_control_args(iface, &rx_key, control_rx_port(slot), drone),
            Stdout::Null,
            Some(&format!("/run/ados/wfb-gs-rx-control-{slot}.log")),
        )
        .await?;
        Ok(SlotReceivers {
            slot,
            video,
            aux,
            control,
        })
    }

    /// Bring the video receiver (p0) up for a slot, the bring-back half of the
    /// unwatched-video-slots split. Called when the slot becomes the hero, off the
    /// 1 s hero poll. The process is created anew because `GsWfbProcess` is not
    /// `Clone` (it owns a process group); the previous one, if any, was dropped
    /// (killpg'd) when it was unpromoted.
    pub async fn spawn_slot_video(&self, iface: &str, slot: u8) -> std::io::Result<GsWfbProcess> {
        let rx_key = rx_key_path();
        let drone = ados_radio::config::link_id(self.config.fleet_id, slot);
        GsWfbProcess::spawn(
            "wfb_rx",
            &data_rx_args(iface, &rx_key, video_rx_port(slot), drone),
            Stdout::Null,
            Some(&format!("/run/ados/wfb-gs-video-rx-{slot}.log")),
        )
        .await
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

    #[test]
    fn an_empty_registry_still_yields_one_receivable_slot() {
        // A freshly-imaged ground station has no registry. Returning the empty
        // set would spawn ZERO receivers and leave the rig structurally deaf
        // with nothing logged as wrong, so it falls back to the slot
        // `FleetRegistry::allocate` issues first — where the first provisioned
        // drone will be.
        let slots = fleet_slots(&crate::fleet::FleetRegistry::default());
        assert_eq!(slots, vec![FIRST_DRONE_SLOT]);

        let mut registry = crate::fleet::FleetRegistry::default();
        assert_eq!(registry.allocate("first-drone"), Some(FIRST_DRONE_SLOT));
    }

    #[test]
    fn registered_slots_drive_the_receive_chain_in_ascending_order() {
        // The lowest slot is the PRIMARY (its video RX anchors the generation),
        // so the order is load-bearing, not cosmetic.
        let mut registry = crate::fleet::FleetRegistry::default();
        registry.allocate("drone-a").unwrap();
        registry.allocate("drone-b").unwrap();
        registry.allocate("drone-c").unwrap();
        registry.release("drone-a");
        // Slot 1 is free again, so the survivors are 2 and 3 and the primary
        // becomes 2 — never a stale 1 pointing at a released drone.
        assert_eq!(fleet_slots(&registry), vec![2, 3]);
    }

    #[test]
    fn each_slot_decodes_to_its_own_three_ports() {
        // A shared egress would cross two drones' lanes: N H.264 streams on one
        // port is an undecodable mux, and one aux port would blend two vehicles'
        // MAVLink into one ingest.
        let mut seen = std::collections::BTreeSet::new();
        for slot in 1..=ados_radio::config::FLEET_MAX_SLOTS {
            assert!(seen.insert(video_rx_port(slot)));
            assert!(seen.insert(aux_rx_port(slot)));
            assert!(seen.insert(control_rx_port(slot)));
        }
        assert_eq!(seen.len(), ados_radio::config::FLEET_MAX_SLOTS as usize * 3);
    }
}
