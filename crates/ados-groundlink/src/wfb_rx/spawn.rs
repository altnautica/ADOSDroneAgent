//! Receive-chain process spawns: the primary chain (the anchor slot's three
//! receivers plus the fleet's two ground transmitters) and the per-slot
//! receivers. Every receiver listens on all receiving adapters; the two
//! transmitters run on the transmit adapter.

use std::time::Duration;

use ados_proc::{GroupChild, Spec, StderrMode, StdoutMode};
use tokio::process::ChildStdout;

use super::args::{
    aux_rx_port, control_rx_port, data_rx_args, gs_aux_rx_args, gs_aux_tx_args, gs_rx_control_args,
    gs_tx_control_args, rx_key_path, video_rx_port,
};
use super::WfbRxManager;

/// How long a ground transmitter gets after `SIGTERM` before `SIGKILL`.
const TX_STOP_GRACE: Duration = Duration::from_secs(2);

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
    /// The two ground transmitters, started or not as the box's uplink says.
    pub tx: GroundTx,
}

/// The fleet's two ground transmitters: `tx_control` (`wfb_tx -p 1`, HopAck
/// and presence) and `aux_tx` (`wfb_tx -p 3`, the whole ground→drone uplink).
/// A ground off the default fleet runs a third, `provisional_aux_tx`: the aux
/// uplink keyed to the default fleet's ground `link_id`, the only uplink a
/// drone it paired but has not yet addressed can hear (see
/// [`ados_radio::config::provisional_fleet`]).
///
/// Stopped and started on their own, without touching a receiver, so the box
/// can stand by while a phone holds the uplink. Built with its arguments
/// resolved, so a start after a stop respawns exactly the same processes.
pub struct GroundTx {
    control_args: Vec<String>,
    aux_args: Vec<String>,
    /// The provisional aux transmitter's arguments, `None` on the default
    /// fleet.
    provisional_aux_args: Option<Vec<String>>,
    /// The transmitters while running; dropping them killpg's each.
    running: Option<Running>,
}

/// The running ground transmitters.
struct Running {
    control: GroupChild,
    aux: GroupChild,
    provisional_aux: Option<GroupChild>,
}

/// Each running transmitter's per-second `PKT` stats stream.
pub struct TxStdout {
    pub control: Option<ChildStdout>,
    pub aux: Option<ChildStdout>,
}

impl GroundTx {
    pub fn new(
        control_args: Vec<String>,
        aux_args: Vec<String>,
        provisional_aux_args: Option<Vec<String>>,
    ) -> Self {
        Self {
            control_args,
            aux_args,
            provisional_aux_args,
            running: None,
        }
    }

    pub fn is_running(&self) -> bool {
        self.running.is_some()
    }

    /// Spawn the transmitters (a no-op returning no streams while they run).
    ///
    /// stdout PIPED, not discarded: the per-second `PKT` stats line is the only
    /// per-transmitter proof that bytes offered to this `wfb_tx` actually
    /// reached the radio. See [`ados_radio::tx_liveness`]. The caller MUST
    /// drain both streams — an unread 64 KiB pipe blocks the transmitter in
    /// `fprintf(stdout)`.
    ///
    /// The provisional aux transmitter's stdout is discarded: it carries only
    /// the address deliveries to a drone not yet on this fleet, so its silence
    /// says nothing about the link this box serves and must not end the
    /// generation the way a stalled fleet transmitter does.
    pub fn start(&mut self) -> std::io::Result<TxStdout> {
        if self.running.is_some() {
            return Ok(TxStdout {
                control: None,
                aux: None,
            });
        }
        let mut control = spawn_wfb(
            "wfb_tx",
            &self.control_args,
            StdoutMode::Piped,
            Some("/run/ados/wfb-gs-tx-control.log"),
        )?;
        let mut aux = spawn_wfb(
            "wfb_tx",
            &self.aux_args,
            StdoutMode::Piped,
            Some("/run/ados/wfb-gs-aux-tx.log"),
        )?;
        let provisional_aux = match &self.provisional_aux_args {
            Some(args) => Some(spawn_wfb(
                "wfb_tx",
                args,
                StdoutMode::Null,
                Some("/run/ados/wfb-gs-provisional-aux-tx.log"),
            )?),
            None => None,
        };
        let streams = TxStdout {
            control: control.take_stdout(),
            aux: aux.take_stdout(),
        };
        self.running = Some(Running {
            control,
            aux,
            provisional_aux,
        });
        Ok(streams)
    }

    /// Stop the transmitters and return only after every one exited: `SIGTERM`
    /// each group, wait up to 2 s, then `SIGKILL`. Receivers are untouched.
    pub async fn stop(&mut self) {
        if let Some(Running {
            mut control,
            mut aux,
            mut provisional_aux,
        }) = self.running.take()
        {
            tokio::join!(
                control.terminate(TX_STOP_GRACE),
                aux.terminate(TX_STOP_GRACE),
                async {
                    if let Some(p) = provisional_aux.as_mut() {
                        p.terminate(TX_STOP_GRACE).await;
                    }
                }
            );
        }
    }
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

/// The receivers a ground off the default fleet runs for a drone it paired but
/// has not yet addressed (see [`WfbRxManager::spawn_provisional_receivers`]).
/// Held for the generation; dropping them killpg's both.
pub struct ProvisionalReceivers {
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
    /// Spawn the receive chain anchored on `primary_slot`, and resolve the two
    /// single ground transmitters without starting them: the generation starts
    /// them unless the box stands by for a phone.
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
        let tx_iface = self.interface.as_str();

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
        // Aux UPLINK (`aux_tx`): the counterpart to the receiver above, and
        // the half that made the lane one-directional before it existed: the
        // drone has always listened on radio_id 3. `tx_control` carries HopAck
        // and the presence beacon. Both are idle-cheap, so whenever this box
        // holds the uplink both run. Off the default fleet, the provisional
        // aux uplink runs beside them (idle until a delivery is offered).
        let provisional_aux = super::provisional_links(fleet).map(|links| {
            gs_aux_tx_args(
                tx_iface,
                &rx_key,
                self.config.mcs_index,
                links.ground,
                ados_radio::config::PROVISIONAL_UPLINK_PORT,
            )
        });
        let tx = GroundTx::new(
            gs_tx_control_args(tx_iface, &rx_key, self.config.mcs_index, ground),
            gs_aux_tx_args(
                tx_iface,
                &rx_key,
                self.config.mcs_index,
                ground,
                self.config.aux_tx_port,
            ),
            provisional_aux,
        );
        Ok(PrimaryChain {
            slot: primary_slot,
            video,
            aux,
            control,
            tx,
        })
    }

    /// The standby presence watcher's receiver: `wfb_rx -p 1` on the box's
    /// own ground `link_id`, decoding whichever ground holds the uplink onto
    /// [`crate::uplink::watcher::watch_port`]. Run only while the box's own
    /// transmitters are stopped.
    pub fn spawn_uplink_watcher(&self) -> std::io::Result<GroupChild> {
        let ground =
            ados_radio::config::link_id(self.config.fleet_id, ados_radio::config::SLOT_GROUND);
        spawn_wfb(
            "wfb_rx",
            &gs_rx_control_args(
                &self.rx_ifaces,
                &rx_key_path(),
                crate::uplink::watcher::watch_port(),
                ground,
            ),
            StdoutMode::Null,
            Some("/run/ados/wfb-gs-uplink-watch.log"),
        )
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

    /// Spawn the receivers for a drone this ground paired but has not yet
    /// addressed, when this ground is off the default fleet: aux (p2) and
    /// control (p1) keyed to `link_id(DEFAULT_FLEET_ID, FIRST_DRONE_SLOT)`.
    /// `None` on the default fleet, whose own first-slot receivers hear it.
    ///
    /// They decode onto [`FIRST_DRONE_SLOT`]'s ports, which this ground always
    /// binds (see [`super::fleet_slots`]): the presence listener records the
    /// drone's beacon from there (enrolment reads it) and the aux consumer
    /// hands the drone's relay answers to the proxy that asked. Both lanes
    /// already serve more than one drone, since every frame on them names its
    /// sender. No video receiver: nothing watches an unaddressed drone's video.
    ///
    /// [`FIRST_DRONE_SLOT`]: super::FIRST_DRONE_SLOT
    pub async fn spawn_provisional_receivers(
        &self,
    ) -> std::io::Result<Option<ProvisionalReceivers>> {
        let Some(links) = super::provisional_links(self.config.fleet_id) else {
            return Ok(None);
        };
        let rx = &self.rx_ifaces;
        let rx_key = rx_key_path();
        let slot = super::FIRST_DRONE_SLOT;
        let aux = spawn_wfb(
            "wfb_rx",
            &gs_aux_rx_args(rx, &rx_key, aux_rx_port(slot), links.drone),
            StdoutMode::Null,
            Some("/run/ados/wfb-gs-provisional-aux-rx.log"),
        )?;
        let control = spawn_wfb(
            "wfb_rx",
            &gs_rx_control_args(rx, &rx_key, control_rx_port(slot), links.drone),
            StdoutMode::Null,
            Some("/run/ados/wfb-gs-provisional-rx-control.log"),
        )?;
        Ok(Some(ProvisionalReceivers { aux, control }))
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
