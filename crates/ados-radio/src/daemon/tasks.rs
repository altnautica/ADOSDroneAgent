//! The per-bring-up worker tasks of a running radio group, held together so
//! one call aborts them all on respawn or shutdown.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use ados_protocol::shutdown::Shutdown;
use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::aux_cmd::{self, AuxCmdState};
use crate::bitrate::BitrateController;
use crate::cmdsock::{self, CmdState};
use crate::process::TxPlaneCounters;
use crate::watchdog::{
    aux_liveness_watchdog, control_plane_watchdog, tx_health_watchdog, video_recvq_watchdog,
    CounterHandle, WatchdogFired,
};

use super::bringup::GroupCtx;
use super::heartbeat::spawn_heartbeat;
use super::hop_supervisor::{
    emit_presence_beacons, proof_only_listener, run_hop_supervisor, write_hop_key_unavailable,
};
use super::service::Service;

/// The 13 worker handles of one radio group, plus the per-bring-up cancel and
/// the data-plane stats handle a re-armed TX watchdog needs. The run loop
/// `&mut`-selects the respawn triggers; [`GroupTasks::abort_all`] is the one
/// teardown list for respawn and shutdown alike.
pub(super) struct GroupTasks {
    pub(super) cancel_bridge: JoinHandle<()>,
    pub(super) heartbeat: JoinHandle<()>,
    pub(super) watchdog1: JoinHandle<WatchdogFired>,
    pub(super) watchdog2: JoinHandle<WatchdogFired>,
    pub(super) aux_watchdog: JoinHandle<()>,
    pub(super) control_watchdog: JoinHandle<WatchdogFired>,
    pub(super) group_exit: JoinHandle<()>,
    pub(super) bitrate_ctrl: JoinHandle<()>,
    pub(super) cmd_server: JoinHandle<()>,
    pub(super) radio_cmd_server: JoinHandle<()>,
    pub(super) aux_cmd_server: JoinHandle<()>,
    pub(super) hop: JoinHandle<()>,
    pub(super) beacon: JoinHandle<()>,
    cancel: Shutdown,
    data_stats: Arc<TxPlaneCounters>,
}

/// Spawn the data-plane TX health watchdog (also re-armed after an in-place
/// PHY recovery).
fn spawn_tx_watchdog(
    cancel: &Shutdown,
    iface: &str,
    counters: &CounterHandle,
    data_stats: &Arc<TxPlaneCounters>,
) -> JoinHandle<WatchdogFired> {
    let tx_cancel = cancel.clone();
    let tx_iface = iface.to_string();
    let tx_counters = counters.clone();
    let tx_stats = data_stats.clone();
    tokio::spawn(
        async move { tx_health_watchdog(&tx_iface, tx_stats, tx_counters, tx_cancel).await },
    )
}

impl GroupTasks {
    /// Run watchdogs + hop supervisor concurrently for the group `g`.
    pub(super) async fn spawn(
        svc: &Service<'_>,
        g: &GroupCtx,
        shutdown: &watch::Receiver<bool>,
    ) -> Self {
        // Per-bring-up cancel for the worker tasks. Each worker is also aborted
        // explicitly on respawn/shutdown, so this latching `Shutdown` is the graceful
        // wake; a small bridge task fires it once the latched shutdown watch flips
        // so a worker's own `cancel.wait()` arm wins promptly. The bridge is
        // aborted alongside the workers, never outliving the bring-up.
        let cancel = Shutdown::new();
        let cancel_bridge = {
            let task_cancel = cancel.clone();
            let mut bridge_shutdown = shutdown.clone();
            tokio::spawn(async move {
                let _ = bridge_shutdown.wait_for(|s| *s).await;
                task_cancel.trigger();
            })
        };

        // The data plane's own stats totals (process-lifetime for this group, so
        // a retune respawn keeps feeding the same handle): the heartbeat's
        // transmit liveness and both data-plane watchdogs read them.
        let data_stats = g.proc.lock().await.data_stats();
        let heartbeat = spawn_heartbeat(svc, g, cancel.clone(), &data_stats);

        let watchdog1 = spawn_tx_watchdog(&cancel, &g.iface, &g.counters, &data_stats);

        let recvq_cancel = cancel.clone();
        let recvq_counters = g.counters.clone();
        let recvq_stats = data_stats.clone();
        let watchdog2 = tokio::spawn(async move {
            video_recvq_watchdog(recvq_stats, recvq_counters, recvq_cancel).await
        });

        // Auxiliary application-stream liveness watchdog. Idles while the aux pair
        // is closed (the safe-by-default boot state) and, when a plugin has opened
        // the stream, applies the same delta-counter contract the data plane uses
        // — a flat ingress counter on a live aux transmitter is a silent stall.
        // It owns its OWN recovery (restart only the aux pair in place), so it
        // never trips the whole-group respawn select; it ends only when cancelled,
        // and is aborted alongside the other siblings on respawn/shutdown.
        let aux_wd_cancel = cancel.clone();
        let aux_wd_proc = g.proc.clone();
        // Not `&mut`-selected in the run loop (it owns its own recovery and ends
        // only on cancel), so it is only ever aborted, never polled by the select.
        let aux_watchdog =
            tokio::spawn(async move { aux_liveness_watchdog(aux_wd_proc, aux_wd_cancel).await });

        // Control-plane delta-counter watchdog. The tx-control transmitter and the
        // rx-control receiver carry HopAnnounce/beacons, HopAck and the only link
        // measurement; each one's own stats counter must advance inside a 30 s
        // window, and a live process with a flat counter trips a group respawn.
        let ctl_cancel = cancel.clone();
        let ctl_proc = g.proc.clone();
        let control_watchdog =
            tokio::spawn(async move { control_plane_watchdog(ctl_proc, ctl_cancel).await });

        // Group exit watch. The counter watchdogs only fire after a 30 s flat
        // window, so a plane that crashes on its own (segfault, OOM kill, a
        // driver-rejected arg on respawn) would otherwise leave the link dead for
        // up to 30 s. This arm polls every plane's liveness on a 1 s cadence and
        // completes the moment any has exited, tripping an immediate respawn of
        // the whole radio group via the run-loop select. A brief lock per poll
        // (non-blocking `try_wait` reaps) does not contend with the FEC/MCS
        // setters in practice.
        let exit_cancel = cancel.clone();
        let exit_proc = g.proc.clone();
        let group_exit = tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(1));
            tick.tick().await; // consume the immediate first tick
            loop {
                tokio::select! {
                    _ = tick.tick() => {
                        if let Some(plane) = exit_proc.lock().await.exited_plane() {
                            tracing::warn!(plane, "wfb_plane_exited_respawning");
                            return;
                        }
                    }
                    _ = exit_cancel.wait() => return,
                }
            }
        });

        // Adaptive bitrate / FEC / MCS controller. Off by default (it only
        // refreshes the snapshot when disabled); when enabled it retunes the data
        // plane's FEC and modulation on the running transmitter through the
        // wfb_tx management socket, and publishes the rung's bitrate to the video
        // encoder as a ceiling. It never ends on its own, so it is not part of
        // the respawn-trigger select arm — it's aborted alongside the other
        // siblings on respawn/shutdown.
        let bc_cancel = cancel.clone();
        let bc_proc = g.proc.clone();
        let bc_snapshot = svc.bitrate_snapshot.clone();
        let bc_enabled = svc.adaptive_enabled.clone();
        // The ladder's top-rung cap and the rung wfb_tx was actually spawned on,
        // so the ladder starts believing reality rather than its own default.
        let bc_mcs_cap = svc.cfg.adaptive_mcs_max;
        let bc_mcs_start = g.spawn_cfg.mcs_index;
        // The watchdog counters carry the transmit-queue congestion flag, which
        // drives the bitrate ladder whenever the receiving peer's link report
        // is missing or stale.
        let bc_counters = g.counters.clone();
        let bitrate_ctrl = tokio::spawn(async move {
            BitrateController::new(bc_enabled, bc_mcs_cap, bc_mcs_start)
                .run(bc_proc, bc_snapshot, bc_counters, bc_cancel)
                .await;
        });

        // Operator command socket: serves the live FEC/MCS/TX-power/tier knobs to
        // the REST layer when the native radio is the running transmit plane.
        // Holds the SAME process handle + adaptive flag the controller uses, so a
        // knob change reaches the live radio. Spawned per bring-up alongside the
        // sibling tasks (it is aborted + re-served with the new process handle on
        // every respawn, the same lifecycle as the heartbeat + watchdogs).
        let cmd_state = CmdState {
            proc: g.proc.clone(),
            adaptive_enabled: svc.adaptive_enabled.clone(),
            tx_power: svc.tx_power.clone(),
            unrestricted: svc.unrestricted,
        };
        let cmd_cancel = cancel.clone();
        let cmd_sock_path = crate::paths::run_path("wfb-cmd.sock");
        let cmd_server = tokio::spawn(async move {
            tokio::select! {
                r = cmdsock::serve(cmd_state, Path::new(&cmd_sock_path)) => {
                    if let Err(e) = r {
                        tracing::warn!(error = %e, "wfb_command_socket_serve_ended");
                    }
                }
                _ = cmd_cancel.wait() => {}
            }
        });

        // Radio command socket: an operator (the REST layer) can trigger a
        // coordinated channel hop on demand. Validated requests flow over this
        // bounded mpsc to the hop supervisor's select loop, which runs the
        // paired/peer/bind checks and drives the EXISTING announce path so the
        // GS follows. Spawned per bring-up alongside the sibling tasks (aborted +
        // re-served on every respawn). The receiver is handed to the hop
        // supervisor below; when auto-hop is fully disabled (the else branch runs
        // proof_only_listener) no receiver drains it, so the socket reports
        // `unavailable` for hop requests rather than hanging.
        let (manual_hop_tx, manual_hop_rx) =
            tokio::sync::mpsc::channel::<crate::radio_cmd::ManualHopRequest>(8);
        let radio_cmd_state = crate::radio_cmd::CmdState {
            hop_tx: manual_hop_tx,
            operating_channel: svc.operating_channel.clone(),
        };
        let radio_cmd_cancel = cancel.clone();
        let radio_cmd_sock_path = crate::paths::run_path("radio-cmd.sock");
        let radio_cmd_server = tokio::spawn(async move {
            tokio::select! {
                r = crate::radio_cmd::serve(
                    radio_cmd_state,
                    Path::new(&radio_cmd_sock_path),
                ) => {
                    if let Err(e) = r {
                        tracing::warn!(error = %e, "radio_command_socket_serve_ended");
                    }
                }
                _ = radio_cmd_cancel.wait() => {}
            }
        });

        // Auxiliary-stream command socket: a plugin (via the plugin host) opens or
        // closes the additive aux transmit/receive pair through this socket. Holds
        // the SAME process handle the watchdogs + operator socket use, and the boot
        // config (the source of the effective aux ports/FEC/MCS an open applies).
        // SAFE-BY-DEFAULT: nothing starts here at bring-up — the pair exists only
        // between an explicit open and its close. Spawned + aborted per bring-up
        // like the sibling sockets.
        // The fan-out sender and its receive loop are process-lifetime state
        // hoisted above the respawn loop; this bring-up's command socket just
        // takes a clone, so a subscriber attached after a respawn is fed by the
        // same still-running receive half.
        let aux_cmd_state = AuxCmdState::radio(
            g.proc.clone(),
            Arc::new(svc.cfg.clone()),
            svc.aux_app_tx.clone(),
        );
        let aux_cmd_cancel = cancel.clone();
        let aux_cmd_sock_path = crate::paths::run_path("radio-aux.sock");
        let aux_cmd_server = tokio::spawn(async move {
            tokio::select! {
                r = aux_cmd::serve(aux_cmd_state, Path::new(&aux_cmd_sock_path)) => {
                    if let Err(e) = r {
                        tracing::warn!(error = %e, "aux_command_socket_serve_ended");
                    }
                }
                _ = aux_cmd_cancel.wait() => {}
            }
        });

        let hop_cancel = cancel.clone();
        let hop_iface = g.iface.clone();
        let hop_proc = g.proc.clone();
        let hop_cfg = svc.cfg.clone();
        let hop_key = g.pair_key;
        let presence_cancel = cancel.clone();
        let device_id = ados_protocol::identity::device_id(None);

        // Presence beacon emitter (10s interval). It advertises the LIVE channel
        // (read from the interface each tick), not the configured value, so a GS
        // that hears the beacon jumps to where the drone ACTUALLY is. A beacon
        // that lies about its channel is the loop that hides a fallback-frequency
        // landing; feeding the live value turns it into a true rendezvous pointer.
        let beacon_cancel = presence_cancel.clone();
        let beacon_key = hop_key;
        let beacon_iface = g.iface.clone();
        let beacon_fallback = g.rendezvous_ch;
        let beacon_device = device_id.clone();
        let beacon = tokio::spawn(async move {
            match beacon_key {
                Some(key) => {
                    emit_presence_beacons(
                        &beacon_device,
                        &beacon_iface,
                        beacon_fallback,
                        &key,
                        beacon_cancel,
                    )
                    .await
                }
                // No usable key: a beacon nobody can verify is worse than none.
                None => beacon_cancel.wait().await,
            }
        });

        // Hop supervisor (enabled only when configured). When hop is disabled the
        // else branch still runs an always-on control-plane proof listener so the
        // drone's received-side proof (`channel_locked` / `rf_unverified`) works
        // regardless of hop config — both paths feed the same shared `RxProof`.
        let hop_enabled = hop_cfg.auto_hop_enabled;
        let hop_link = g.link.clone();
        let hop_enabled_channels = g.enabled_channels.clone();
        let hop_restart = svc.restart_count.clone();
        let hop_proof = g.rx_proof.clone();
        let hop_proof_reference = g.proof_reference;
        let hop_operating = svc.operating_channel.clone();
        let hop_shared_state = svc.hop_state.clone();
        let hop = tokio::spawn(async move {
            let Some(hop_key) = hop_key else {
                // The key file exists but cannot be read. Refuse every hop
                // (manual requests read as unavailable once the receiver drops)
                // and say why on the hop sidecar, rather than hopping under a
                // key anyone can compute.
                drop(manual_hop_rx);
                write_hop_key_unavailable(&hop_cfg);
                hop_cancel.wait().await;
                return;
            };
            if hop_enabled {
                run_hop_supervisor(
                    &hop_iface,
                    &hop_cfg,
                    hop_proc,
                    &hop_key,
                    &device_id,
                    hop_link,
                    hop_enabled_channels,
                    hop_restart,
                    hop_proof,
                    hop_proof_reference,
                    hop_operating,
                    hop_shared_state,
                    manual_hop_rx,
                    hop_cancel,
                )
                .await;
            } else {
                // Auto-hop fully disabled: drain (and reject) the manual-hop
                // channel so the command socket's `try_send` reports `unavailable`
                // promptly instead of filling the buffer. Dropping the receiver
                // here closes the channel, which the socket reads as unavailable.
                drop(manual_hop_rx);
                proof_only_listener(
                    &hop_key,
                    &device_id,
                    hop_proof,
                    hop_proof_reference,
                    hop_cancel,
                )
                .await;
            }
        });

        GroupTasks {
            cancel_bridge,
            heartbeat,
            watchdog1,
            watchdog2,
            aux_watchdog,
            control_watchdog,
            group_exit,
            bitrate_ctrl,
            cmd_server,
            radio_cmd_server,
            aux_cmd_server,
            hop,
            beacon,
            cancel,
            data_stats,
        }
    }

    /// Re-arm the TX health watchdog after an in-place PHY recovery.
    pub(super) fn rearm_tx_watchdog(&mut self, g: &GroupCtx) {
        self.watchdog1 = spawn_tx_watchdog(&self.cancel, &g.iface, &g.counters, &self.data_stats);
    }

    /// Abort every worker so none outlives its radio group.
    pub(super) fn abort_all(&self) {
        self.cancel_bridge.abort();
        self.heartbeat.abort();
        self.watchdog1.abort();
        self.watchdog2.abort();
        self.aux_watchdog.abort();
        self.control_watchdog.abort();
        self.group_exit.abort();
        self.bitrate_ctrl.abort();
        self.cmd_server.abort();
        self.radio_cmd_server.abort();
        self.aux_cmd_server.abort();
        self.hop.abort();
        self.beacon.abort();
    }
}
