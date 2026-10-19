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
//! - `seams`: the shared counter / channel setters / clock / data-RX handle
//!   that implement the watchdog + acquirer seams, plus the live-channel read.
//! - `stats`: `build_gs_stats` + the reg-blocked sidecar writer.
//! - `adapter_stats`: the per-adapter receive counters and the diversity block.
//! - `loops`: the stdout stats reader + the stdout-silence zombie watchdog.
//! - `prepare`: interface bring-up on every WFB adapter.
//! - `spawn`: the primary chain + per-slot receiver spawns.
//! - this module root: the `WfbRxManager` (config + resolved adapters +
//!   watchdog assembly).
//!
//! Every receiver listens on all receiving adapters at once (`wfb_rx` combines
//! their frames before FEC decode); the transmitters run on the one transmit
//! adapter the role resolver selects.

use std::collections::BTreeSet;
use std::sync::Arc;

use ados_radio::adapter::AdapterPlan;
use ados_radio::config::WfbConfig;
use ados_radio::reg_gate::Posture;

use crate::acquire::{ChannelAcquirer, ChannelSetter};
use crate::presence::GsPresenceCache;
use crate::watchdog::{Clock, FileLockedChannelHint, LockedChannelHint, ValidPacketWatchdog};

pub mod adapter_stats;
pub mod args;
pub mod loops;
mod prepare;
pub mod seams;
mod spawn;
pub mod stats;

pub use adapter_stats::AdapterStatsTracker;
pub use args::{
    aux_rx_port, control_rx_port, data_rx_args, gs_aux_rx_args, gs_aux_tx_args, gs_rx_control_args,
    gs_tx_control_args, video_rx_port, AUX_TX_PORT, DEFAULT_REG_DOMAIN, RX_HEALTH_POLL_INTERVAL_S,
    STATE_ACTIVE, STATE_BLOCKED_UNPAIRED, STATE_NO_ADAPTER, STATE_NO_INJECTION, STATE_REG_BLOCKED,
    STATE_SEARCHING, TX_CONTROL_PORT,
};
pub use loops::{stats_reader_loop, zombie_watchdog};
pub use seams::{
    DataRxHandle, GroupChannelSetter, IwChannelSetter, RadioGroup, SharedValidCounter, SystemClock,
};
pub use spawn::{PrimaryChain, SlotReceivers};
pub use stats::{
    build_gs_stats, write_blocked_unpaired_sidecar, write_no_adapter_sidecar,
    write_no_injection_sidecar, write_reg_blocked_sidecar, GsAdapterInfo, GsChannelTruth,
    GsRegSnapshot,
};

// Re-exported so the run loop can build the shared receive-health seam through
// the same module that owns the stats reader.
pub use crate::watchdog::SharedRxHealth;

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
    /// The transmit adapter (the primary interface the sidecar reports and
    /// the channel acquirer retunes through the group setter).
    interface: String,
    /// The interfaces every receiver listens on, in `wfb_rx` order.
    rx_ifaces: Vec<String>,
    /// Every WFB interface (transmit first), the set bring-up configures.
    radio_ifaces: Vec<String>,
    /// The resolved adapter layout, `None` until the run loop resolves one.
    plan: Option<AdapterPlan>,
    channel: u8,
    /// The selected receive adapter's facts, surfaced on every sidecar write.
    adapter: GsAdapterInfo,
    /// The regulatory-permitted channel set for the receive interface, resolved
    /// once `prepare_interfaces` has applied the domain + read the wiphy back.
    /// Empty until then (and on a board where the set cannot be determined); the
    /// acquirer treats an empty set as "do not restrict".
    enabled_channels: BTreeSet<u8>,
    /// The regulatory picture (live domain + verified + permitted set) resolved
    /// by `prepare_interfaces`, surfaced on every sidecar write so the panel
    /// sees the same regulatory truth as the drone. Default (unknown) until
    /// prepared.
    reg_snapshot: GsRegSnapshot,
    /// The regulatory posture `prepare_interfaces` brought the interfaces up
    /// under. `None` until it first runs.
    posture: Option<Posture>,
}

impl WfbRxManager {
    pub fn new(config: WfbConfig) -> Self {
        // Tune the channel the regulatory gate verifies: the rendezvous channel
        // (the operator's home, or the optional pin). The drone tunes the same.
        let channel = config.rendezvous_channel();
        let interface = config.interface.clone();
        Self {
            config,
            rx_ifaces: vec![interface.clone()],
            radio_ifaces: vec![interface.clone()],
            interface,
            plan: None,
            channel,
            adapter: GsAdapterInfo::default(),
            enabled_channels: BTreeSet::new(),
            reg_snapshot: GsRegSnapshot::default(),
            posture: None,
        }
    }

    /// The regulatory picture resolved by `prepare_interfaces`. Default
    /// (unknown) until that runs.
    pub fn reg_snapshot(&self) -> &GsRegSnapshot {
        &self.reg_snapshot
    }

    /// The regulatory posture resolved by the last `prepare_interfaces`, `None`
    /// before it first ran.
    pub fn posture(&self) -> Option<&Posture> {
        self.posture.as_ref()
    }

    /// The rendezvous (meeting) channel for this receive plane — the operator's
    /// home, or the optional rendezvous pin. Both rigs derive it identically.
    pub fn rendezvous_channel(&self) -> u8 {
        self.config.rendezvous_channel()
    }

    /// The transmit adapter's interface (the primary one).
    pub fn interface(&self) -> &str {
        &self.interface
    }

    /// The interfaces every receiver listens on, in `wfb_rx` order.
    pub fn rx_ifaces(&self) -> &[String] {
        &self.rx_ifaces
    }

    /// Every WFB interface, transmit adapter first.
    pub fn radio_ifaces(&self) -> &[String] {
        &self.radio_ifaces
    }

    /// The resolved adapter layout, `None` until one was set.
    pub fn plan(&self) -> Option<&AdapterPlan> {
        self.plan.as_ref()
    }

    pub fn channel(&self) -> u8 {
        self.channel
    }

    /// The regulatory-permitted channel set resolved by `prepare_interfaces`.
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

    /// Adopt the adapter layout the run loop resolved. The receive chain, the
    /// bring-up and the sidecar all read it here, rather than a config value
    /// that is empty when no external detector populated it.
    pub fn set_plan(&mut self, plan: &AdapterPlan) {
        self.interface = plan.tx().ifname().to_string();
        self.rx_ifaces = plan.rx_ifaces();
        self.radio_ifaces = plan.radio_ifaces();
        self.plan = Some(plan.clone());
    }

    /// The fleet this receive plane belongs to. Every `link_id` the chain is
    /// keyed with is derived from it.
    pub fn fleet_id(&self) -> u16 {
        self.config.fleet_id
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
        // `prepare_interfaces` so the sweep skips channels this domain forbids
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
            self.config.rendezvous_channel(), // immutable home
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
        // Until prepare_interfaces runs, the permitted set is empty (the acquirer
        // reads empty as "do not restrict").
        let m = WfbRxManager::new(WfbConfig::default());
        assert!(m.enabled_channels().is_empty());
    }

    /// The receiver tunes the channel the regulatory gate verified: a rendezvous
    /// pin wins over the home channel, exactly as on the drone, so both rigs
    /// meet where the gate looked.
    #[test]
    fn the_receiver_tunes_the_rendezvous_channel() {
        let pinned = WfbConfig {
            channel: 149,
            rendezvous_channel: Some(153),
            ..WfbConfig::default()
        };
        assert_eq!(WfbRxManager::new(pinned).channel(), 153);
        let unpinned = WfbConfig {
            channel: 157,
            ..WfbConfig::default()
        };
        assert_eq!(WfbRxManager::new(unpinned).channel(), 157);
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
