//! The direct receive manager's generation loop: resolve the adapters, bring
//! every WFB interface up, spawn the receive chain and its watchdogs, run the
//! generation to completion, tear down and respawn.

use super::generation::{reconcile_fleet, spawn_link_telemetry};
use super::*;

/// The receive manager's main loop: spawn a generation, run it to completion,
/// restart with bounded backoff.
pub(super) async fn receive_loop(
    config: &WfbConfig,
    presence_cache: GsPresenceCache,
    radio_group: wfb_rx::RadioGroup,
    hop_follower: presence::HopFollower,
    aux_counters: crate::AuxCounters,
    progress: ados_supervisor::sdnotify::MonitorProgress,
) {
    let mut manager = WfbRxManager::new(config.clone());
    let clock: Arc<dyn crate::watchdog::Clock> = Arc::new(SystemClock::default());
    // Every retune (acquirer sweep, hop follow) lands on all WFB adapters at
    // once, so the diversity receiver never splits across channels.
    let setter: Arc<dyn crate::acquire::ChannelSetter> = Arc::new(wfb_rx::GroupChannelSetter::new(
        Arc::new(IwChannelSetter),
        radio_group.clone(),
    ));
    let hint = wfb_rx::default_hint();
    // Telemetry emitter for the per-generation receive-link samples shipped to
    // the logging daemon. Constructed once for the service lifetime; each
    // generation spawns a 1 Hz task that clones it. Best-effort and
    // non-blocking, like the drone-side radio emitter.
    let ingest = ados_protocol::logd::emitter::IngestEmitter::new("ados-groundlink");

    // When the unpaired sidecar was last refreshed. The gate polls every 5 s so a
    // key landing is picked up promptly, but the sidecar only needs to stay
    // fresh, not be rewritten twelve times a minute on a flash card.
    let mut unpaired_published: Option<std::time::Instant> = None;
    loop {
        // One stamp per pass, before the pairing gate. Every arm below either
        // completes a pass and comes back here or is itself inside a generation
        // whose 1 s hero tick stamps again, so a healthy service always feeds
        // the watchdog and a loop wedged inside any one stage does not.
        progress.mark();
        // Pairing gate: without the rx key on disk there is nothing to receive.
        // (The Python side blocks here too; the pairing flow lands the key.)
        if !std::path::Path::new(RX_KEY).exists() {
            tracing::info!(expected = RX_KEY, "ground_wfb_blocked_unpaired");
            // Say so on the sidecar too. This arm used to `continue` without
            // writing anything, so a ground station with no key published NOTHING
            // — and an unlinked pair had one half simply missing from every
            // surface but the journal.
            let due = unpaired_published
                .map(|t| t.elapsed() >= UNPAIRED_SIDECAR_REFRESH)
                .unwrap_or(true);
            if due {
                wfb_rx::write_blocked_unpaired_sidecar(
                    &config.interface,
                    config.rendezvous_channel(),
                    config,
                    Some(&ingest),
                );
                unpaired_published = Some(std::time::Instant::now());
            }
            tokio::time::sleep(Duration::from_secs(5)).await;
            continue;
        }
        // Paired again: the next unpaired spell publishes immediately rather than
        // waiting out a refresh interval left over from the last one.
        unpaired_published = None;
        // Resolve the adapters. Honors an explicit `video.wfb.interface` pin
        // (moved to the front when it passes the gate); otherwise every
        // verified RTL injection adapter is used (the management wifi and the
        // operator's control path are excluded and monitor mode is proven),
        // with roles from `radio.adapters`. The direct role runs no mesh
        // carrier, so no adapter is held back for it.
        let plan = match crate::adapter_plan::resolve(&config.interface, false).await {
            crate::adapter_plan::Resolution::Ready(plan) => plan,
            crate::adapter_plan::Resolution::NoInjection(sel) => {
                // Injection setup did not establish. A slow USB link is the usual
                // cause, so keep the USB facts and publish a `no_injection`
                // sidecar carrying them: a stuck receive plane must self-report
                // WHY it is deaf rather than going silent while the run
                // loop retries.
                let adapter = wfb_rx::GsAdapterInfo::from(&sel);
                manager.set_adapter(adapter.clone());
                wfb_rx::write_no_injection_sidecar(
                    &sel.ifname,
                    &adapter,
                    config.rendezvous_channel(),
                    config,
                    Some(&ingest),
                );
                tracing::warn!(
                    interface = %sel.ifname,
                    usb_speed_mbps = ?adapter.usb_speed_mbps,
                    usb_degraded = adapter.usb_degraded,
                    "ground_wfb_adapter_no_injection"
                );
                tokio::time::sleep(RETRY_INTERVAL).await;
                continue;
            }
            crate::adapter_plan::Resolution::NoAdapter => {
                manager.set_adapter(wfb_rx::GsAdapterInfo::default());
                tracing::warn!("ground_no_wfb_adapter_found");
                // Replace whatever the last generation published: an adapter
                // pulled mid-session must not leave `active` on the sidecar.
                wfb_rx::write_no_adapter_sidecar(
                    &config.interface,
                    config.rendezvous_channel(),
                    config,
                    Some(&ingest),
                );
                tokio::time::sleep(RETRY_INTERVAL).await;
                continue;
            }
        };
        // Carry the transmit adapter's record (chipset, injection verdict, USB
        // link health) onto the manager, so every sidecar write reports the
        // adapter actually in use rather than a default.
        let adapter = wfb_rx::GsAdapterInfo::from(&plan.tx().adapter);
        if adapter.usb_degraded {
            tracing::warn!(
                interface = %plan.tx().ifname(),
                usb_speed_mbps = ?adapter.usb_speed_mbps,
                "ground_wfb_adapter_usb_degraded: adapter on a slow USB link (needs 480 Mbps); RF may not be received"
            );
        }
        manager.set_adapter(adapter.clone());
        manager.set_plan(&plan);
        // Record every WFB interface so the group setter retunes all of them
        // and the shutdown path (in `run_direct`) restores each to managed mode.
        *radio_group.lock().await = manager.radio_ifaces().to_vec();
        let interface = manager.interface().to_string();
        // Stage boundary: the adapters are resolved. Stamped here as well as at
        // the top of the pass so the regulatory gate + monitor-mode + txpower
        // bring-up below — the longest legitimate single stage on this path —
        // cannot be mistaken for a wedge.
        progress.mark();

        // Bring the interfaces to receive-ready BEFORE the spawn, in the
        // kernel-required order: the regulatory gate (set + verify the domain,
        // then assert the rendezvous channel is permitted and non-DFS, both
        // before monitor-mode bring-up so the home channel is never capped to the
        // startup domain's limits), then monitor mode, TX power (brownout guard
        // on marginal USB hosts), and the rendezvous-home channel on every WFB
        // adapter. Re-applied each generation. On a strict-gate failure the
        // receive chain is NOT spawned on a capped radio: park in `reg_blocked`,
        // surface it, and retry with bounded backoff.
        if let Err(e) = manager.prepare_interfaces().await {
            // Surface the live domain vs the wanted one (the manager's snapshot
            // may be partial when the gate failed before reading it), so the panel
            // shows the actual regulatory conflict, not a configured-channel lie.
            let posture = manager.posture().cloned();
            let wanted = posture
                .as_ref()
                .map_or(wfb_rx::DEFAULT_REG_DOMAIN, |p| p.wanted_domain.as_str())
                .to_string();
            let status = ados_radio::adapter::read_reg_status(&wanted).await;
            let reg = wfb_rx::GsRegSnapshot {
                domain: status.domain,
                verified: status.verified,
                enabled_channels: manager.enabled_channels().iter().copied().collect(),
                posture,
            };
            wfb_rx::write_reg_blocked_sidecar(
                &interface,
                &adapter,
                config.rendezvous_channel(),
                config,
                &reg,
                e.reason_code(),
                Some(&ingest),
            );
            tokio::time::sleep(RETRY_INTERVAL).await;
            continue;
        }

        // Resolve this generation's fleet slots and spawn the receive chain.
        // The lowest registered slot is the PRIMARY: its video RX carries the
        // stats stream, anchors the generation's liveness, and is the link the
        // channel acquirer sweeps for. Every other registered slot gets its own
        // additive video/aux/control trio on the same receiving adapters.
        let mut slots = wfb_rx::fleet_slots(&FleetRegistry::load(std::path::Path::new(
            FLEET_REGISTRY_PATH,
        )));
        slots.sort_unstable();
        let primary_slot = slots[0];
        let mut chain = match manager.spawn_receive_chain(primary_slot).await {
            Ok(chain) => chain,
            Err(e) => {
                tracing::error!(error = %e, slot = primary_slot, "ground_wfb_rx_failed_to_start");
                tokio::time::sleep(RETRY_INTERVAL).await;
                continue;
            }
        };
        // Secondary slots are best-effort: a failure to spawn one drone's
        // receivers must not take the whole fleet's receive plane down, and the
        // reconcile tick below retries it on the next pass.
        let mut secondaries: std::collections::BTreeMap<u8, SlotReceivers> =
            std::collections::BTreeMap::new();
        for &slot in &slots[1..] {
            match manager.spawn_slot_receivers(slot, false).await {
                Ok(r) => {
                    secondaries.insert(slot, r);
                }
                Err(e) => tracing::error!(error = %e, slot, "ground_slot_rx_failed_to_start"),
            }
        }
        tracing::info!(
            fleet_id = manager.fleet_id(),
            primary_slot,
            slots = ?slots,
            "ground_receive_chain_spawned"
        );

        let stdout = chain.video.take_stdout();
        let mut tx_control = chain.tx_control;
        let mut aux_tx = chain.aux_tx;
        // Each transmitter's per-second `PKT` stats stream, taken before the
        // processes are parked for the generation. These MUST be drained: an
        // unread pipe eventually blocks the transmitter in `fprintf(stdout)`,
        // which would wedge the very lane the watchdog exists to protect.
        let tx_control_stats = tx_control.take_stdout();
        let aux_tx_stats = aux_tx.take_stdout();
        // The rest of the chain (the primary's aux + control receivers and the
        // two ground transmitters) is held for the generation's lifetime; its
        // `Drop` killpg's each process group when this iteration ends.
        let _primary_rest = (chain.aux, chain.control, tx_control, aux_tx);
        let rx_handle = DataRxHandle::new(chain.video);

        // Shared liveness state for this generation.
        let counter = SharedValidCounter::new();
        hop_follower.set_valid_counter(counter.clone());
        let link = Arc::new(Mutex::new(LinkStats::default()));
        let last_stdout_at = Arc::new(Mutex::new(clock.monotonic()));
        let zombie_kills = Arc::new(AtomicU32::new(0));
        // Live receive-health publish seam: the valid-packet watchdog writes its
        // reacquire-kill total + the valid-decode silence here, and the stats
        // reader pulls them onto the sidecar so the GS heartbeat carries the real
        // values instead of hardcoded zeros.
        let rx_health = wfb_rx::SharedRxHealth::new();

        // Fan-out as a sub-service (the HERO slot's video egress → 5600 mediamtx
        // + 5605 LCD), aborted with the generation. It follows the operator's
        // hero selection, published as a sidecar by the hero route, and re-points
        // itself when that changes; the primary slot is what it serves when no
        // live selection exists, which is the boot state and the permanent state
        // of a single-drone fleet. The shared counters are read by the stats
        // reader so the wfb-stats sidecar surfaces the forwarded/drop totals (the
        // fan-out hop, otherwise blind to the cross-process diagnostics).
        let fanout_counters = fanout::FanoutCounters::new();
        let fanout_task = tokio::spawn(fanout::run_default_fanout(
            primary_slot,
            fanout_counters.clone(),
        ));

        // 1 Hz receive-link telemetry for this generation: ship the link's
        // RSSI / SNR / uncorrected-FEC (the uplink command radio, mirroring the
        // drone-side downlink video radio) and a lock/unlock event on a real
        // link-state transition. Aborted with the generation. Best-effort; an
        // absent logging daemon drops the samples without disturbing receive.
        let telemetry_task = spawn_link_telemetry(&ingest, &link);

        // 1 Hz link-quality feedback to the transmitting drone. The drone cannot
        // measure its own downlink, so its adaptive bitrate ladder has no loss
        // sample to step on; this receiver has one and reports it up the aux
        // uplink. Aborted with the generation, exactly like the telemetry task,
        // so a receive chain that is being torn down stops asserting a link
        // quality it is no longer measuring.
        let feedback_task = {
            let link = link.clone();
            let port = config.aux_tx_port;
            tokio::spawn(crate::link_feedback::run(link, port, primary_slot))
        };

        // Stats reader: feeds the counter + LinkStats + the sidecar. Carries the
        // rendezvous home, the regulatory snapshot the gate resolved, and the
        // resolved adapter facts so the sidecar surfaces the truthful channel,
        // reg picture, and adapter/USB health, symmetric with the drone side.
        let stats_task = stdout.map(|out| {
            tokio::spawn(wfb_rx::stats_reader_loop(
                out,
                counter.clone(),
                link.clone(),
                last_stdout_at.clone(),
                clock.clone(),
                interface.clone(),
                manager.channel(),
                manager.rendezvous_channel(),
                manager.reg_snapshot().clone(),
                config.clone(),
                manager.adapter().clone(),
                wfb_rx::AdapterStatsTracker::new(&plan),
                Some(rx_health.clone()),
                zombie_kills.clone(),
                Some(ingest.clone()),
                fanout_counters.clone(),
                aux_counters.clone(),
            ))
        });

        // Zombie watchdog (stdout-silence).
        let mut zombie_task = tokio::spawn(wfb_rx::zombie_watchdog(
            rx_handle.clone(),
            last_stdout_at.clone(),
            clock.clone(),
            zombie_kills.clone(),
        ));

        // Valid-packet watchdog: owns a fresh acquirer, reads the shared counter
        // + presence cache, terminates the data RX on a genuine loss. It also
        // observes live video off the shared counter each poll (so a healthy
        // stream with a dropped peer beacon does not trip the teardown) and
        // mirrors its receive-health counters to the stats reader's sidecar.
        let mut watchdog = manager
            .build_watchdog(
                counter.clone(),
                presence_cache.clone(),
                rx_handle.clone(),
                clock.clone(),
                setter.clone(),
                hint.clone(),
            )
            .with_health(rx_health.clone());
        let mut watchdog_task = tokio::spawn(async move {
            watchdog.run().await;
        });

        // Transmitter liveness. Both GS transmitters were previously held only
        // so their `Drop` would killpg them — nothing observed either one, so a
        // `wfb_tx` that went silently dead without exiting took down HopAck and
        // the presence beacon (`tx_control`) or the ENTIRE ground→drone uplink
        // (`aux_tx`: arm, disarm, mode, mission upload, param writes, relay RPC,
        // link feedback) while video RX kept decoding and every surface still
        // read `active`. These watchers assert the delta counters off each
        // transmitter's own `PKT` stats stream and end the generation on a
        // stall, which respawns the whole chain. `&mut` in the select below for
        // the same reason as the two watchdogs above: the arm that did not win
        // must not be dropped-and-detached into the next generation.
        //
        // A transmitter whose stdout handle could not be taken is watched by
        // nothing rather than reported as stalled — a missing handle is this
        // process's own plumbing fault, and restarting the radio on it would be
        // a fabricated verdict about the transmitter.
        let mut tx_control_watch = tx_control_stats.map(|out| {
            tokio::spawn(ados_radio::tx_liveness::watch_tx_liveness(
                "tx_control",
                out,
                ados_radio::tx_liveness::TX_POLL_INTERVAL,
                ados_radio::tx_liveness::TX_SILENCE_WINDOW,
            ))
        });
        let mut aux_tx_watch = aux_tx_stats.map(|out| {
            tokio::spawn(ados_radio::tx_liveness::watch_tx_liveness(
                "aux_tx",
                out,
                ados_radio::tx_liveness::TX_POLL_INTERVAL,
                ados_radio::tx_liveness::TX_SILENCE_WINDOW,
            ))
        });
        if tx_control_watch.is_none() || aux_tx_watch.is_none() {
            tracing::warn!(
                tx_control_watched = tx_control_watch.is_some(),
                aux_tx_watched = aux_tx_watch.is_some(),
                "ground_tx_liveness_unwatched: a transmitter stats stream was unavailable"
            );
        }

        // Re-read the fleet registry on a slow tick and add/remove the SECONDARY
        // slots' receivers in place. Pairing a 25th drone must not interrupt the
        // other 24, so a registry change reconciles inside the generation rather
        // than restarting the whole chain. Dropping a `SlotReceivers` killpg's
        // its three process groups, which is the despawn.
        //
        // Returning from this arm ENDS the generation, and it only returns when
        // the PRIMARY slot is released: the primary's video RX is the stats
        // stream and the channel acquirer's target, so a new primary has to be
        // chosen by a fresh generation.
        let reconcile = reconcile_fleet(&manager, primary_slot, &mut secondaries, &progress);

        // The generation ends when any of: the data RX exits, the zombie
        // watchdog kills it, the valid-packet watchdog terminates it, either
        // ground transmitter stops producing bytes, or the primary slot leaves
        // the fleet.
        // `&mut` the watchdog handles so the arm that did NOT win is not
        // dropped-and-detached here — a dropped JoinHandle leaves the task
        // running, so the zombie + valid-packet watchdogs would pile up across
        // generations, each holding an acquirer + driving `iw` retunes against
        // the next generation's radio. They are aborted explicitly below.
        tokio::select! {
            _ = wait_for_exit(rx_handle.clone()) => {
                tracing::warn!("ground_wfb_rx_exited");
            }
            _ = &mut zombie_task => {}
            _ = &mut watchdog_task => {}
            _ = wait_tx_stall(&mut tx_control_watch) => {}
            _ = wait_tx_stall(&mut aux_tx_watch) => {}
            _ = reconcile => {}
        }

        // Tear down the generation's sub-tasks before respawning. The two
        // watchdog handles are aborted alongside the fan-out / telemetry / stats
        // tasks (an already-finished task's abort is a no-op), so no generation's
        // watchdog survives into the next, mirroring the air-side abort-siblings
        // discipline.
        zombie_task.abort();
        watchdog_task.abort();
        fanout_task.abort();
        telemetry_task.abort();
        feedback_task.abort();
        if let Some(t) = stats_task {
            t.abort();
        }
        // The transmitter watchers hold the read end of a pipe whose writer is
        // about to be killpg'd with the generation; aborting them here keeps a
        // previous generation's watcher from surviving to judge the next one's
        // radio, exactly as for the two watchdogs above.
        for t in [tx_control_watch, aux_tx_watch].into_iter().flatten() {
            t.abort();
        }

        tokio::time::sleep(RETRY_INTERVAL).await;
    }
}

/// Poll the data-RX handle until it reports not-running. One arm of the
/// generation's completion select.
pub(super) async fn wait_for_exit(rx: Arc<DataRxHandle>) {
    use crate::watchdog::RxProcess;
    loop {
        if !rx.is_running() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// One generation-completion arm per ground transmitter: resolve when its
/// liveness watcher reports a stall.
///
/// A transmitter with no stats handle has no watcher, and this arm then parks
/// forever rather than resolving. That is deliberate: the alternative — treating
/// a missing pipe as a stall — would restart the radio on this process's own
/// plumbing fault and report it as a transmitter failure. The other arms still
/// end the generation, and the missing handle is logged where it happens.
pub(super) async fn wait_tx_stall(
    watch: &mut Option<tokio::task::JoinHandle<ados_radio::tx_liveness::TxVerdict>>,
) {
    match watch {
        Some(handle) => {
            let _ = handle.await;
        }
        None => std::future::pending::<()>().await,
    }
}
