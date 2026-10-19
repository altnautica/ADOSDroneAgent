//! One service pass of the `ados-crsf` daemon under one loaded config.

use super::modules::{
    autonomy_lanes, module_statuses, module_targets, publish_unconfigured, run_autonomy_module,
    serve_command_socket,
};
use super::*;

pub(super) async fn run_service_pass(
    cfg: &CrsfLaneConfig,
    config_path: &Path,
    profile_conf: &Path,
    mut shutdown: watch::Receiver<bool>,
    reload: Arc<Notify>,
    pass_scope: watch::Receiver<bool>,
) -> RunExit {
    // Telemetry emitter for the periodic lane-status events shipped to the
    // logging daemon. Best-effort and non-blocking: an absent daemon socket
    // drops the sample, never stalling the lane.
    let metrics = ados_protocol::logd::emitter::IngestEmitter::new("ados-crsf");

    // ── Opt-in gate ──────────────────────────────────────────────────────
    // Absent/false `radio.crsf.enabled` ⇒ idle harmlessly with an honest
    // `disabled` sidecar until asked to stop. Not an error: the lane is
    // simply not in use on this node.
    if !cfg.enabled {
        tracing::info!("crsf_lane_disabled");
        return idle_with_sidecar(
            LaneState::Disabled,
            &metrics,
            &mut shutdown,
            &reload,
            &StatsInputs::default(),
        )
        .await;
    }

    // ── Profile gate ─────────────────────────────────────────────────────
    // The RC transmitter lane is ground-side; on a drone this binary idles
    // (defensive — the unit is already profile-gated at install time). The
    // sidecar reads `disabled`: the lane does not run on this node.
    if profile_is_drone(config_path, profile_conf) {
        tracing::warn!("crsf_idle_on_drone_profile");
        return idle_with_sidecar(
            LaneState::Disabled,
            &metrics,
            &mut shutdown,
            &reload,
            &StatsInputs::default(),
        )
        .await;
    }

    // ── Mode gate ────────────────────────────────────────────────────────
    // The RC transmitter runs only in `crsf_rc`; the other modes hand the
    // module to a different owner, so this lane must not touch the port —
    // driving RC frames onto it would corrupt the owner's traffic.
    //
    //   - `mavlink`: the module runs its native MAVLink mode (a MAVLink byte
    //     carrier; the module firmware owns the CRSF air protocol internally).
    //     The MAVLink router ingests the carrier as its FC source — the pinned
    //     device at the fixed MAVLink-mode baud, or the WiFi-backpack UDP
    //     listen — so this service holds off the device entirely (one owner
    //     per port) and STANDS BY at `ready` with the mode reported: alive,
    //     transmitting no RC, and reclaiming the port on the next reload that
    //     flips the mode back. The router keeps that source telemetry-only
    //     (host->FC command-down gated by default) until
    //     `radio.crsf.mavlink_command_enabled` is set.
    //   - `airport`: a generic serial data pipe with no ADOS owner yet; the
    //     lane reads `disabled` with the mode reported — it does not run.
    match cfg.mode {
        LaneMode::CrsfRc => {}
        LaneMode::Mavlink => {
            tracing::info!(
                transport = cfg.mavlink_transport.as_str(),
                "crsf_rc_lane_standby_mavlink_mode"
            );
            let idle_inputs = StatsInputs {
                mode: Some(cfg.mode.as_str()),
                relay_role: cfg.relay_role.sidecar_str(),
                // The router owns the carrier in this mode and keeps the
                // host->FC command-down direction gated closed by default;
                // report that gate on the lane's own status so a consumer of
                // only this lane sees whether the ELRS command path is open.
                fc_command_down_gated: cfg.fc_command_down_gated(),
                ..StatsInputs::default()
            };
            return idle_with_sidecar(
                LaneState::Ready,
                &metrics,
                &mut shutdown,
                &reload,
                &idle_inputs,
            )
            .await;
        }
        LaneMode::Airport => {
            tracing::info!(mode = cfg.mode.as_str(), "crsf_rc_lane_idle_for_mode");
            let idle_inputs = StatsInputs {
                mode: Some(cfg.mode.as_str()),
                relay_role: cfg.relay_role.sidecar_str(),
                ..StatsInputs::default()
            };
            return idle_with_sidecar(
                LaneState::Disabled,
                &metrics,
                &mut shutdown,
                &reload,
                &idle_inputs,
            )
            .await;
        }
    }

    // ── Module set ───────────────────────────────────────────────────────
    // The pilot module is the RC lane this pass drives below; each autonomy
    // module runs its own transport for the life of the pass (stopped by the
    // pass-scope latch), injected through the command socket by module id.
    let modules = cfg.resolved_modules();
    let pilot = modules
        .iter()
        .find(|m| m.role == crate::modules::ModuleRole::Pilot)
        .cloned();
    let pilot_port = pilot.as_ref().map(|m| m.port.as_str()).unwrap_or("");
    let autonomy = autonomy_lanes(&modules);
    for lane in &autonomy {
        tokio::spawn(run_autonomy_module(
            lane.clone(),
            cfg.packet_rate_hz,
            pass_scope.clone(),
        ));
    }

    // Lane state shared across serial respawns: the channel-source merge the
    // TX task reads and the latest sidecar body the status verb serves.
    let merge = Arc::new(Mutex::new(SourceMerge::new(cfg.channel_source)));
    let latest_status = Arc::new(Mutex::new(serde_json::Value::Null));
    // Reads the node pairing key to turn a claimed injector name into an
    // attested one. Built once and shared: the read behind it is cached, so the
    // command path does not touch the filesystem per request.
    let injector_auth = Arc::new(crate::injector::InjectorAuth::from_env());
    // The pilot module's out-of-band lane and transport flag, shared with the
    // command socket, which is served for the whole pass so an autonomy module
    // stays addressable while the pilot port is down.
    let oob = Arc::new(crate::transport::OobQueue::default());
    let pilot_connected = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let _cmd_socket = serve_command_socket(CmdState {
        merge: merge.clone(),
        latest_status: latest_status.clone(),
        oob: oob.clone(),
        injector_auth,
        modules: Arc::new(module_targets(
            pilot.as_ref(),
            pilot_connected.clone(),
            &autonomy,
        )),
    });

    // The HID/PIC source: stick + switch intent from the primary gamepad,
    // fed into the merge for the whole service lifetime (the gamepad is
    // independent of the serial module's respawn cycle). Device reads are
    // Linux-only; elsewhere the hid slot stays empty, so under HID authority
    // the lane transmits nothing.
    #[cfg(target_os = "linux")]
    if cfg.channel_source != ChannelSourceMode::Inject {
        tokio::spawn(crate::hid::run_hid_source(
            merge.clone(),
            pass_scope.clone(),
        ));
    }

    loop {
        // Latched shutdown gate at the top of the respawn loop.
        if *shutdown.borrow() {
            return RunExit::Shutdown;
        }

        // ── Device guard ─────────────────────────────────────────────────
        if pilot_port.is_empty() {
            tracing::info!("crsf_no_device_pinned");
            publish_unconfigured(&latest_status, &metrics, pilot.as_ref(), &autonomy).await;
            tokio::select! {
                biased;
                _ = wait_for_shutdown_flag(&mut shutdown) => return RunExit::Shutdown,
                _ = reload.notified() => return RunExit::Reload,
                _ = tokio::time::sleep(RETRY_INTERVAL) => continue,
            }
        }
        let Some(stream) = open_serial(pilot_port, CRSF_BAUD) else {
            tracing::warn!(device = %pilot_port, "crsf_serial_open_failed");
            publish_unconfigured(&latest_status, &metrics, pilot.as_ref(), &autonomy).await;
            tokio::select! {
                biased;
                _ = wait_for_shutdown_flag(&mut shutdown) => return RunExit::Shutdown,
                _ = reload.notified() => return RunExit::Reload,
                _ = tokio::time::sleep(RETRY_INTERVAL) => continue,
            }
        };
        tracing::info!(device = %pilot_port, baud = CRSF_BAUD, rate_hz = cfg.packet_rate_hz, "crsf_serial_open");

        // ── Regulatory posture at link bring-up ──────────────────────────
        // One durable record per link session: the operator posture
        // (unrestricted/region + the acknowledgement audit), the host-applied
        // frame cadence, and the module-side knob targets whose mechanism is
        // the CRSF parameter carrier — never the system radio stack. This is
        // the record behind the operator-responsibility badge, so it is
        // emitted before the first frame is transmitted.
        let reg_policy = crate::reg::RegPolicy::load_from(config_path);
        crate::reg::emit_reg_posture(&metrics, cfg, &reg_policy);

        // ── Bring-up: spawn the sibling tasks ────────────────────────────
        let (read_half, write_half) = tokio::io::split(stream);
        let counters = Arc::new(WireCounters::default());
        let telemetry = Arc::new(Mutex::new(TelemetryState::default()));
        // Parameter frames queued for a port that died die with it: a stale
        // write must not fire on a fresh port.
        drop(oob.drain());
        let task_cancel = Shutdown::new();

        // Bridge the latched shutdown onto the per-bring-up cancel notify so
        // worker tasks stop on SIGTERM even while mid-await.
        let mut bridge_shutdown = shutdown.clone();
        let bridge_cancel = task_cancel.clone();
        let cancel_bridge = tokio::spawn(async move {
            let _ = bridge_shutdown.wait_for(|s| *s).await;
            bridge_cancel.trigger();
        });

        let mut tx_task = tokio::spawn(run_tx(
            write_half,
            merge.clone(),
            cfg.packet_rate_hz,
            counters.clone(),
            oob.clone(),
            task_cancel.clone(),
        ));
        let mut rx_task = tokio::spawn(run_rx(
            read_half,
            telemetry.clone(),
            counters.clone(),
            task_cancel.clone(),
        ));
        // The flat-TX liveness watchdog: while a source is live, frames must
        // keep being accepted by the module at cadence. A fire breaks to the
        // respawn loop, which reinitialises the transport and re-verifies from
        // a fresh window.
        let mut watchdog_task = tokio::spawn(crate::watchdog::tx_liveness_watchdog(
            counters.clone(),
            merge.clone(),
            task_cancel.clone(),
        ));
        pilot_connected.store(true, Ordering::Relaxed);

        // ── Heartbeat loop ───────────────────────────────────────────────
        // When RC frames started flowing continuously. `None` while the lane
        // transmits nothing (no live source), so the link ladder never reads
        // an idle lane as a transmitter waiting for received proof.
        let mut tx_since: Option<Instant> = None;
        let mut prev_tx: u64 = 0;
        let mut prev_rx: u64 = 0;
        let mut prev_at = Instant::now();
        let exit_reason: &str = loop {
            tokio::select! {
                biased;
                _ = wait_for_shutdown_flag(&mut shutdown) => break "shutdown",
                _ = reload.notified() => break "reload",
                r = &mut tx_task => {
                    tracing::warn!(exit = ?r, "crsf tx task exited");
                    break "tx_exit";
                }
                r = &mut rx_task => {
                    tracing::warn!(exit = ?r, "crsf rx task exited");
                    break "rx_exit";
                }
                r = &mut watchdog_task => {
                    tracing::warn!(fired = ?r, "crsf tx liveness watchdog fired");
                    break "tx_stalled";
                }
                _ = tokio::time::sleep(HEARTBEAT_INTERVAL) => {}
            }

            let now = Instant::now();
            let elapsed = now.duration_since(prev_at).as_secs_f64().max(1e-6);
            let tx_total = counters.tx_frames.load(Ordering::Relaxed);
            let rx_total = counters.rx_frames.load(Ordering::Relaxed);
            let tx_fps = ((tx_total - prev_tx) as f64 / elapsed * 10.0).round() / 10.0;
            let rx_fps = ((rx_total - prev_rx) as f64 / elapsed * 10.0).round() / 10.0;
            if tx_total > prev_tx {
                tx_since.get_or_insert(now);
            } else {
                tx_since = None;
            }
            prev_tx = tx_total;
            prev_rx = rx_total;
            prev_at = now;

            // Refresh the PIC arbiter view for the hybrid authority decision (a
            // staleness-gated sidecar read). A read that returns `None` — the
            // arbiter's sidecar absent, unreadable, malformed, or stale — is
            // passed through as-is: hybrid then holds SAFE (the injector never
            // wins on a missing verdict), NOT collapsed to a fabricated
            // unclaimed view.
            let mut pic_report: Option<&str> = None;
            if cfg.channel_source != ChannelSourceMode::Inject {
                let pic_path = crate::paths::run_path("pic-state.json");
                let view = crate::sources::read_pic_view(
                    Path::new(&pic_path),
                    std::time::SystemTime::now(),
                );
                // Surface the arbiter's availability honestly for hybrid — the
                // only mode where its report gates authority. `unavailable`
                // (arbiter not reporting, fail-safe hold) is never conflated
                // with a fresh `unclaimed`.
                if cfg.channel_source == ChannelSourceMode::Hybrid {
                    pic_report = Some(match &view {
                        None => "unavailable",
                        Some(v) if v.claimed => "claimed",
                        Some(_) => "unclaimed",
                    });
                }
                merge.lock().await.set_pic(view);
            }

            let (stats_age, link_copy) = {
                let t = telemetry.lock().await;
                (
                    t.stats_age(now),
                    t.last_link_stats.as_ref().map(|(s, _)| *s),
                )
            };
            let state = derive_state(&LinkInputs {
                enabled: true,
                device_open: true,
                tx_running_for: tx_since.map(|since| now.duration_since(since)),
                stats_age,
                uplink_lq: link_copy.map(|l| l.uplink_lq),
            });
            // Only a FRESH link-statistics frame is reported: a stale reading
            // re-surfacing as current would be a lying surface.
            let fresh_link = match stats_age {
                Some(age) if age <= crate::link::STATS_FRESH_WINDOW => link_copy,
                _ => None,
            };
            let source = merge.lock().await.current(now).map(|(_, s)| s.as_str());
            let module_list =
                module_statuses(pilot.as_ref(), true, fresh_link.as_ref(), &autonomy, now).await;
            let inputs = StatsInputs {
                link: fresh_link.as_ref(),
                // The operating band is a measurement the lane does not have
                // (link statistics do not carry it); the configured band is a
                // target, reported by the regulatory posture surface — the
                // sidecar never echoes a target as a reading.
                band: None,
                packet_rate_hz: Some(cfg.packet_rate_hz),
                tx_frames_per_s: Some(tx_fps),
                tx_frames_total: Some(tx_total),
                rx_frames_per_s: Some(rx_fps),
                mode: Some(cfg.mode.as_str()),
                channel_source: source,
                pic: pic_report,
                relay_role: cfg.relay_role.sidecar_str(),
                // None in this branch: the RC channel lane has no
                // MAVLink-over-ELRS command source to gate (config-derived, so
                // it stays honest if the predicate ever changes).
                fc_command_down_gated: cfg.fc_command_down_gated(),
                modules: &module_list,
            };
            *latest_status.lock().await = build_stats_value(state, &inputs);
            write_stats_sidecar(state, &inputs, Some(&metrics));
        };

        // ── Teardown ─────────────────────────────────────────────────────
        pilot_connected.store(false, Ordering::Relaxed);
        task_cancel.trigger();
        tx_task.abort();
        rx_task.abort();
        watchdog_task.abort();
        cancel_bridge.abort();

        if *shutdown.borrow() {
            return RunExit::Shutdown;
        }
        if exit_reason == "reload" {
            return RunExit::Reload;
        }
        tracing::info!(reason = exit_reason, "crsf_lane_respawning");
        publish_unconfigured(&latest_status, &metrics, pilot.as_ref(), &autonomy).await;
        tokio::select! {
            biased;
            _ = wait_for_shutdown_flag(&mut shutdown) => return RunExit::Shutdown,
            _ = reload.notified() => return RunExit::Reload,
            _ = tokio::time::sleep(RETRY_INTERVAL) => {}
        }
    }
}
