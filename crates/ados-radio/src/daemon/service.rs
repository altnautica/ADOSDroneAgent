//! The radio service's respawn loop: process-lifetime state, one radio-group
//! bring-up per pass, the group's tasks, the respawn-trigger select and the
//! teardown.

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use ados_protocol::logd::emitter::{EventEmitter, IngestEmitter};
use tokio::sync::{broadcast, watch};

use crate::bitrate::{new_enabled, new_snapshot, EnabledHandle, SnapshotHandle};
use crate::cmdsock::TxPowerState;
use crate::config::WfbConfig;
use crate::hop::HopState;
use crate::link_quality::LinkStats;
use crate::reg_gate::Posture;
use crate::watchdog::WatchdogFired;

use super::bringup::phy::ensure_radiating;
use super::bringup::{bring_up_group, GroupCtx, Park};
use super::sidecar::{write_stats_sidecar, ChannelTruth, RegPosture, RegSnapshot};
use super::signals::wait_for_shutdown_flag;
use super::tasks::GroupTasks;
use super::txrate::TxRates;
use super::CONFIG_YAML;

/// Process-lifetime state shared by every radio-group bring-up.
pub(super) struct Service<'a> {
    pub(super) cfg: &'a WfbConfig,
    pub(super) posture: Posture,
    pub(super) unrestricted: bool,
    pub(super) region_domain: String,
    pub(super) reg_posture: RegPosture,
    pub(super) events: EventEmitter,
    pub(super) metrics: IngestEmitter,
    pub(super) restart_count: Arc<AtomicU64>,
    pub(super) bitrate_snapshot: SnapshotHandle,
    pub(super) adaptive_enabled: EnabledHandle,
    pub(super) tx_power: Arc<TxPowerState>,
    pub(super) aux_app_tx: broadcast::Sender<(u8, Vec<u8>)>,
    pub(super) operating_channel: Arc<AtomicU64>,
    pub(super) hop_state: Arc<tokio::sync::Mutex<HopState>>,
}

impl<'a> Service<'a> {
    fn new(cfg: &'a WfbConfig) -> Self {
        // Operating-region posture, read once at service start through the same
        // resolver the ground station uses, so both ends set the same domain and
        // apply the same gate. The default is unrestricted: the radio brings up +
        // TX-enables on the home channel without a verified operating region, and
        // the operator is responsible for local RF compliance. Pinning a region
        // re-enables the strict regulatory gate for that jurisdiction. It never
        // lifts the power-budget / brownout clamp (tx_power_max_dbm / the
        // muted-readback rf_unverified detector stay armed in both postures).
        let posture = Posture::load(Path::new(CONFIG_YAML));
        let unrestricted = posture.is_unrestricted();
        // The wanted domain the bring-up + global reconciler target. Region → the
        // pinned region code; unrestricted → the configured `reg_domain` fallback so
        // the global cfg80211 reconciler still keeps a sane domain (never `00`).
        let region_domain = posture.wanted_domain.clone();
        // The posture surfaced on every sidecar (even unpaired / no-adapter), so the
        // GCS / OLED / webapp always show the honest operating-region state.
        let reg_posture = RegPosture::new(unrestricted, &region_domain);
        if unrestricted {
            tracing::info!(
                note = "operating region not pinned; radiating at hardware-bounded power; operator responsible for local RF compliance",
                "wfb_unrestricted_posture"
            );
        } else {
            tracing::info!(region = %region_domain, "wfb_region_pinned");
        }

        // Application datagram fan-out, shared by every aux `subscribe` connection.
        // Created ONCE, outside the respawn loop, like `bitrate_snapshot` /
        // `adaptive_enabled`: a fresh channel per bring-up left generation
        // N-1's publisher feeding a channel with no subscribers while generation N's
        // subscribers attached to a channel nothing fed, so every inbound auxiliary
        // application datagram (config-over-radio replies, payload command lanes)
        // was lost for the rest of the process lifetime after the first respawn —
        // and respawns are routine (a wfb_tx stall kill, an FEC/MCS retune crash, a
        // hop end).
        //
        // This service does NOT bind the aux-RX loopback port. It used to, and so
        // does `ados-mavlink-router`'s aux-uplink consumer — a different binary and
        // unit on the same drone profile, resolving the same port through
        // `ados_protocol::aux_ports`, neither with SO_REUSEADDR. Whichever lost
        // startup took down a whole lane silently while the link still read healthy:
        // this service winning killed the entire ground→drone uplink (command
        // injection, relay-proxy RPC, config-over-radio, link feedback); the router
        // winning killed every plugin application lane. The consumer is the single
        // owner now, and it hands the two application channels back here over
        // `radio-aux.sock`'s `publish` op (see [`crate::aux_cmd`]), which is
        // what feeds this channel.
        let (aux_app_tx, _) = broadcast::channel::<(u8, Vec<u8>)>(256);
        Service {
            cfg,
            posture,
            unrestricted,
            region_domain,
            reg_posture,
            // Event emitter for discrete, queryable verdicts shipped to the logging
            // daemon (the regulatory-gate decision, the received-side rf-unverified
            // state change). Best-effort and non-blocking: an absent daemon socket is
            // dropped quietly, never stalling the radio. The log lines + sidecar stay.
            events: EventEmitter::new("ados-radio"),
            // Telemetry emitter for the periodic link-quality samples shipped to the
            // logging daemon (RSSI / SNR / uncorrected-FEC, one set per heartbeat).
            // Same non-blocking, best-effort transport as the event emitter; a saturated
            // channel or an absent daemon drops the low-severity sample and counts it.
            metrics: IngestEmitter::new("ados-radio"),
            // Count of radio-group respawns since service start, shared with the
            // heartbeat that surfaces it in the sidecar.
            restart_count: Arc::new(AtomicU64::new(0)),
            // Adaptive bitrate / FEC controller snapshot, shared across respawns so the
            // heartbeat always surfaces the controller's intent (link preset, enable
            // flag, recommended bitrate) even before / between radio bring-ups.
            bitrate_snapshot: new_snapshot(cfg),
            // Runtime-flippable adaptive-controller enable flag, shared across respawns
            // and between the bitrate controller and the operator command socket so the
            // auto/manual link-tier toggle survives a watchdog kill or a channel hop.
            adaptive_enabled: new_enabled(cfg),
            // The operator's TX power request and the power the driver last accepted,
            // shared with the command socket. Process-lifetime like the adaptive flag, so
            // a live change survives a respawn and the heartbeat reports what is applied.
            tx_power: Arc::new(TxPowerState::new(cfg.tx_power_dbm, cfg.tx_power_max_dbm)),
            aux_app_tx,
            // The live operating channel and the hop state are process-lifetime too,
            // like `retained_trio` and `tx_power`: a whole-group respawn after a
            // committed hop must come back up on the hopped channel while the peer is
            // there, not re-tune the rendezvous home and split the pair. The hop
            // supervisor updates both on a committed move.
            operating_channel: Arc::new(AtomicU64::new(cfg.rendezvous_channel() as u64)),
            hop_state: Arc::new(tokio::sync::Mutex::new(HopState::new(
                cfg.rendezvous_channel(),
            ))),
        }
    }
}

/// Clean shutdown: stop the tasks, the radio group, then restore the adapter
/// to managed mode so it isn't left stuck in monitor.
async fn stop_group(tasks: &GroupTasks, g: &GroupCtx) {
    tasks.abort_all();
    g.proc.lock().await.kill_all().await;
    crate::adapter::set_managed_mode(&g.iface).await;
    tracing::info!("wfb_service_stopping");
}

pub(super) async fn run_service(cfg: &WfbConfig, mut shutdown: watch::Receiver<bool>) {
    let svc = Service::new(cfg);
    // The data-plane trio the previous radio group was running (an operator
    // manual tier or preset, or an adaptive step). Every respawn brings the new
    // group up on it rather than on the boot config; `None` until a group ran.
    let mut retained_trio: Option<(u8, u8, u8)> = None;
    loop {
        // Latched shutdown gate at the top of the respawn loop: if SIGTERM flipped
        // the watch while we were tearing down a radio group below, never start
        // another bring-up — return before re-spawning into a stopping service.
        if *shutdown.borrow() {
            return;
        }
        let g = match bring_up_group(&svc, retained_trio).await {
            Ok(g) => g,
            Err(Park(delay)) => {
                tokio::select! {
                    biased;
                    _ = wait_for_shutdown_flag(&mut shutdown) => return,
                    _ = tokio::time::sleep(delay) => continue,
                }
            }
        };
        let iface = g.iface.as_str();
        let mut tasks = GroupTasks::spawn(&svc, &g, &shutdown).await;

        // Wait for any task to finish (cancel → shutdown; watchdog → respawn).
        // `&mut` the handles so the un-selected ones are NOT dropped-and-detached
        // here — we abort them explicitly below so tasks don't pile up across
        // respawns.
        // Inner select loop: most fires fall through (break) to the full kill +
        // respawn below, but a runtime PHY-mute is recovered IN PLACE (re-cycle
        // monitor + channel + txpower) without killing wfb_tx — respawning the
        // process can never un-mute a driver/PHY-level mute, so the old kill path
        // looped forever. On a successful in-place recovery we re-arm the TX
        // watchdog and `continue` to keep watching the live radio group.
        loop {
            // `biased` + the shutdown arm FIRST: a latched SIGTERM must win the
            // poll even when a watchdog / data-tx-exit task is simultaneously ready,
            // so a real shutdown is never mistaken for a respawn trigger.
            tokio::select! {
                biased;
                _ = wait_for_shutdown_flag(&mut shutdown) => {
                    stop_group(&tasks, &g).await;
                    return;
                }
                result = &mut tasks.watchdog1 => {
                    // A finished watchdog could win this poll in the same instant a
                    // SIGTERM arrives; the latched-watch re-check after the select
                    // (and the top-of-respawn-loop gate) catches that ordering so we
                    // never respawn into a stopping service.
                    match result {
                        Ok(WatchdogFired::PhyMuted) => {
                            // Recover on the channel the pair is running on now,
                            // not the rendezvous home: after a committed hop the
                            // peer is on the hopped channel, and retuning home
                            // here would split the pair while the hop state still
                            // claims the hopped channel.
                            let live_ch = svc.operating_channel.load(Ordering::Relaxed) as u8;
                            tracing::warn!(
                                iface,
                                channel = live_ch,
                                "watchdog_phy_muted: attempting in-place PHY recovery"
                            );
                            if let Some(dbm) = ensure_radiating(
                                iface,
                                live_ch,
                                svc.tx_power.requested(),
                                svc.tx_power.max_dbm(),
                                svc.unrestricted,
                            )
                            .await
                            {
                                svc.tx_power.record_applied(Some(dbm));
                                tracing::info!(iface, "watchdog_phy_recovered_in_place");
                                tasks.rearm_tx_watchdog(&g);
                                continue;
                            }
                            tracing::warn!(iface, "watchdog_phy_recovery_failed: killing wfb_tx");
                        }
                        Ok(WatchdogFired::TxStalled | WatchdogFired::RecvqBacklog) => {
                            tracing::warn!("watchdog_fired_killing_wfb_tx");
                        }
                        _ => {}
                    }
                }
                result = &mut tasks.watchdog2 => {
                    if let Ok(WatchdogFired::RecvqBacklog) = result {
                        tracing::warn!("video_recvq_watchdog_fired");
                    }
                }
                result = &mut tasks.control_watchdog => {
                    if let Ok(WatchdogFired::ControlStalled) = result {
                        tracing::warn!("control_plane_watchdog_fired");
                    }
                }
                _ = &mut tasks.group_exit => {
                    // A plane exited on its own — respawn the whole group
                    // immediately (the falls-through path below handles it).
                }
                _ = &mut tasks.hop => {}
                _ = &mut tasks.beacon => {}
                _ = &mut tasks.heartbeat => {}
            }
            break;
        }

        // Latched shutdown re-check: a watchdog / exit-poll arm can legitimately
        // win the select in the same instant SIGTERM flips the watch. Honor the
        // shutdown here rather than falling into the respawn path below, then run
        // the same clean teardown the shutdown arm runs.
        if *shutdown.borrow() {
            stop_group(&tasks, &g).await;
            return;
        }

        // A task exited (watchdog fired / hop ended / data-tx self-crashed) —
        // abort the siblings so they don't accumulate, kill the whole radio
        // group, and respawn.
        tasks.abort_all();
        {
            // Keep the trio the dying group was running (a pinned manual tier,
            // a preset, or an adaptive step) for the next bring-up.
            let mut p = g.proc.lock().await;
            let (fec_k, fec_n) = p.data_fec();
            retained_trio = Some((fec_k, fec_n, p.data_mcs()));
            p.kill_all().await;
        }
        // The radio group will respawn at the top of the loop — count it.
        svc.restart_count.fetch_add(1, Ordering::Relaxed);
        let wd = *g.counters.lock().await;
        // Re-read the live regulatory status for the respawn-transient sidecar so
        // it carries the truth (domain + verified) rather than a stale snapshot.
        let respawn_reg_status = crate::adapter::read_reg_status(&g.wanted_domain).await;
        write_stats_sidecar(
            "connecting",
            &ChannelTruth::configured(g.rendezvous_ch),
            &RegSnapshot {
                domain: respawn_reg_status.domain,
                verified: respawn_reg_status.verified,
                enabled_channels: g.enabled_channels_vec.clone(),
                posture: svc.reg_posture.clone(),
            },
            svc.tx_power.effective().unwrap_or(g.effective_tx_dbm),
            Some(&g.adapter_info),
            &LinkStats::default(),
            cfg,
            svc.restart_count.load(Ordering::Relaxed),
            &wd,
            &TxRates::default(),
            &svc.bitrate_snapshot.lock().await.clone(),
            Some(&svc.metrics),
        );
        tokio::select! {
            biased;
            _ = wait_for_shutdown_flag(&mut shutdown) => return,
            _ = tokio::time::sleep(Duration::from_secs(1)) => {}
        }
    }
}
