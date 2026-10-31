//! Receive-chain process spawns: the primary chain (the anchor slot's three
//! receivers plus the fleet's two ground transmitters) and the per-slot
//! receivers. Every receiver listens on all receiving adapters; the two
//! transmitters run on the transmit adapter.

use ados_proc::{GroupChild, Spec, StderrMode, StdoutMode};

use super::args::{
    aux_rx_port, control_rx_port, data_rx_args, gs_aux_rx_args, gs_aux_tx_args, gs_rx_control_args,
    gs_tx_control_args, rx_key_path, video_rx_port,
};
use super::WfbRxManager;

/// One generation's primary receive chain: the anchor slot's three receivers
/// plus the fleet's two single ground transmitters.
///
/// `video`'s stdout is piped — it is both the stats stream and the liveness
/// anchor, so the generation ends when it exits. Every other field is held only
/// so its `Drop` (killpg) tears the process group down with the generation.
pub struct PrimaryChain {
    /// The fleet slot this chain is anchored on (the lowest registered slot).
    pub slot: u8,
    pub video: GroupChild,
    pub aux: GroupChild,
    pub control: GroupChild,
    pub tx_control: GroupChild,
    pub aux_tx: GroupChild,
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
    pub video: Option<GroupChild>,
    pub aux: GroupChild,
    pub control: GroupChild,
}

/// Spawn one wfb child as its own process group; `log` receives its stderr
/// (emptied at spawn), otherwise stderr is discarded.
pub(crate) fn spawn_wfb(
    program: &str,
    args: &[String],
    stdout: StdoutMode,
    log: Option<&str>,
) -> std::io::Result<GroupChild> {
    let stderr = match log {
        Some(path) => StderrMode::AppendLog(path.into()),
        None => StderrMode::Null,
    };
    GroupChild::spawn(
        program,
        Spec {
            program,
            args,
            stdout,
            stderr,
        },
    )
}

impl SlotReceivers {
    /// Whether this slot currently decodes video.
    pub fn decoding_video(&self) -> bool {
        self.video.is_some()
    }
}

impl WfbRxManager {
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
    /// Every receiver listens on all receiving adapters; the two transmitters
    /// run on the transmit adapter. Adapter detection and monitor-mode setup
    /// are assumed already applied to every one of them.
    pub async fn spawn_receive_chain(&self, primary_slot: u8) -> std::io::Result<PrimaryChain> {
        let rx_key = rx_key_path();
        let fleet = self.config.fleet_id;
        let ground = ados_radio::config::link_id(fleet, ados_radio::config::SLOT_GROUND);
        let drone = ados_radio::config::link_id(fleet, primary_slot);
        let rx = &self.rx_ifaces;
        let tx = self.interface.as_str();

        // Data RX: stdout piped for the stats reader, in its own process group.
        let video = spawn_wfb(
            "wfb_rx",
            &data_rx_args(rx, &rx_key, video_rx_port(primary_slot), drone),
            StdoutMode::Piped,
            None,
        )?;
        let control = spawn_wfb(
            "wfb_rx",
            &gs_rx_control_args(rx, &rx_key, control_rx_port(primary_slot), drone),
            StdoutMode::Null,
            Some("/run/ados/wfb-gs-rx-control.log"),
        )?;
        // Aux application lane. The drone half has always been complete
        // (`wfb_tx -p 2` behind the radio-aux socket) but this receiver was
        // written, unit-tested, and then never spawned, so nothing on the
        // ground ever decoded p2 and the lane carried no traffic in either
        // direction. It is spawned unconditionally rather than behind the aux
        // consumer's enable flag: a linked ground station must be able to
        // receive whatever the drone chooses to send, and an idle wfb_rx with
        // no inbound frames costs essentially nothing.
        let aux = spawn_wfb(
            "wfb_rx",
            &gs_aux_rx_args(rx, &rx_key, aux_rx_port(primary_slot), drone),
            StdoutMode::Null,
            Some("/run/ados/wfb-gs-aux-rx.log"),
        )?;
        // stdout PIPED, not discarded: the per-second `PKT` stats line is the
        // only per-transmitter proof that bytes offered to this `wfb_tx`
        // actually reached the radio. Nothing read it before, so a silently
        // dead transmitter took down HopAck / presence (here) or the whole
        // uplink (below) while every surface still read `active`. See
        // [`ados_radio::tx_liveness`]. The caller MUST drain it — an unread 64 KiB
        // pipe blocks the transmitter in `fprintf(stdout)`.
        let tx_control = spawn_wfb(
            "wfb_tx",
            &gs_tx_control_args(tx, &rx_key, self.config.mcs_index, ground),
            StdoutMode::Piped,
            Some("/run/ados/wfb-gs-tx-control.log"),
        )?;
        // Aux UPLINK. The counterpart to the receiver above, and the half that
        // made the lane one-directional: the drone has always listened on
        // radio_id 3, but nothing on the ground ever transmitted there, so a
        // client's request could reach the ground station and go no further.
        // Spawned unconditionally for the same reason as the receiver — a
        // linked ground station must be able to answer the drone it can hear,
        // and an idle wfb_tx with no ingress traffic costs essentially nothing.
        let aux_tx = spawn_wfb(
            "wfb_tx",
            &gs_aux_tx_args(
                tx,
                &rx_key,
                self.config.mcs_index,
                ground,
                self.config.aux_tx_port,
            ),
            StdoutMode::Piped,
            Some("/run/ados/wfb-gs-aux-tx.log"),
        )?;
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
    /// All instances bind the SAME receiving adapters as the primary chain.
    /// That is legal, not a workaround: `wfb_rx` opens a promiscuous,
    /// non-exclusive pcap handle and compiles a per-instance kernel BPF on
    /// `channel_id` (`vendor/wfb-ng/src/rx.cpp:70,84`), so each receiver sees
    /// only its own drone's frames and N drones need N receivers, not N radios.
    ///
    /// stdout is discarded: only the primary slot's video RX drives the stats
    /// reader and the channel acquirer.
    pub async fn spawn_slot_receivers(
        &self,
        slot: u8,
        with_video: bool,
    ) -> std::io::Result<SlotReceivers> {
        let rx = &self.rx_ifaces;
        let rx_key = rx_key_path();
        let drone = ados_radio::config::link_id(self.config.fleet_id, slot);
        let video = if with_video {
            Some(spawn_wfb(
                "wfb_rx",
                &data_rx_args(rx, &rx_key, video_rx_port(slot), drone),
                StdoutMode::Null,
                Some(&format!("/run/ados/wfb-gs-video-rx-{slot}.log")),
            )?)
        } else {
            None
        };
        let aux = spawn_wfb(
            "wfb_rx",
            &gs_aux_rx_args(rx, &rx_key, aux_rx_port(slot), drone),
            StdoutMode::Null,
            Some(&format!("/run/ados/wfb-gs-aux-rx-{slot}.log")),
        )?;
        let control = spawn_wfb(
            "wfb_rx",
            &gs_rx_control_args(rx, &rx_key, control_rx_port(slot), drone),
            StdoutMode::Null,
            Some(&format!("/run/ados/wfb-gs-rx-control-{slot}.log")),
        )?;
        Ok(SlotReceivers {
            slot,
            video,
            aux,
            control,
        })
    }

    /// Bring the video receiver (p0) up for a slot, the bring-back half of the
    /// unwatched-video-slots split. Called when the slot becomes the hero, off the
    /// 1 s hero poll. The process is created anew because `GroupChild` is not
    /// `Clone` (it owns a process group); the previous one, if any, was dropped
    /// (killpg'd) when it was unpromoted.
    pub async fn spawn_slot_video(&self, slot: u8) -> std::io::Result<GroupChild> {
        let rx_key = rx_key_path();
        let drone = ados_radio::config::link_id(self.config.fleet_id, slot);
        spawn_wfb(
            "wfb_rx",
            &data_rx_args(&self.rx_ifaces, &rx_key, video_rx_port(slot), drone),
            StdoutMode::Null,
            Some(&format!("/run/ados/wfb-gs-video-rx-{slot}.log")),
        )
    }
}
