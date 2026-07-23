//! `ados-crsf` binary — the CRSF / ExpressLRS RC control lane service.
//!
//! Opens the pinned USB-serial RC transmitter module at 420 kbaud, transmits
//! the packed RC channels frame at the configured cadence, decodes returned
//! telemetry, serves the command socket, and writes the lane state sidecar
//! every second. Idles harmlessly (with an honest `disabled` sidecar) when
//! the lane is not opted in or this node's profile does not run it, and shuts
//! down cleanly on SIGTERM/SIGINT.
//!
//! ## Port ownership per `radio.crsf.mode`
//!
//! The module's port has exactly one owner, decided by the configured mode:
//!
//! - **`crsf_rc`** — THIS service owns the pinned device (CRSF framing at
//!   420 kbaud); the MAVLink router excludes the pin from FC candidacy.
//! - **`mavlink`** — the module runs its native MAVLink mode: a MAVLink byte
//!   carrier (the module firmware owns the CRSF air protocol internally). The
//!   MAVLink router ingests the carrier as its FC source — the pinned device
//!   at the fixed MAVLink-mode baud, or the WiFi backpack's UDP listen — so
//!   this service NEVER opens the port. It stands by at state `ready` with
//!   `mode: "mavlink"` reported: alive, honest about why no RC is transmitted,
//!   and reclaiming the port on the next reload that flips the mode back. The
//!   router reads telemetry up but keeps the host->FC command-down direction
//!   gated closed by default (`radio.crsf.mavlink_command_enabled`), so the
//!   source is telemetry-only until that marker is set. The command socket is
//!   not served (channel injection has no lane to land on and reads `503` at
//!   the control plane).
//! - **`airport`** — a generic serial data pipe with no ADOS owner yet; the
//!   lane reads `disabled` with the mode reported.
//!
//! SIGHUP is the in-process config reload (the unit's `ExecReload`): the
//! current bring-up tears down cleanly, `/etc/ados/config.yaml` is re-read,
//! and every gate (opt-in, profile, mode, device pin) re-runs — so enabling
//! or re-pinning the lane through the config surface applies without
//! dropping the unit.

use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::{watch, Mutex, Notify};

use ados_crsf::cmdsock::{self, CmdState};
use ados_crsf::config::{profile_is_drone, CrsfLaneConfig, LaneMode};
use ados_crsf::link::{derive_state, LaneState, LinkInputs};
use ados_crsf::sidecar::{build_stats_value, write_stats_sidecar, StatsInputs};
use ados_crsf::sources::{ChannelSourceMode, SourceMerge};
use ados_crsf::transport::{open_serial, run_rx, run_tx, TelemetryState, WireCounters, CRSF_BAUD};

const CONFIG_YAML: &str = "/etc/ados/config.yaml";
const PROFILE_CONF: &str = "/etc/ados/profile.conf";
/// Sidecar heartbeat cadence while the lane runs.
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(1);
/// Backoff between bring-up attempts (no device, open failure, port death).
const RETRY_INTERVAL: Duration = Duration::from_secs(5);
/// Sidecar refresh cadence while the lane idles in a terminal state
/// (disabled / wrong profile / non-RC mode). The body does not change; the
/// rewrite keeps the file's mtime fresh so a staleness-gated reader can tell
/// a live idle lane from a dead service's orphaned file. Half the tightest
/// consumer window (the heartbeat's 10 s gate), so an idle lane never flaps
/// in and out of the heartbeat at the staleness boundary.
const IDLE_REFRESH_INTERVAL: Duration = Duration::from_secs(5);

#[tokio::main]
async fn main() {
    {
        use ados_protocol::logd::layer::LogdLayer;
        use tracing_subscriber::prelude::*;

        // fmt as the primary sink plus the logd layer that ships records to
        // the logging daemon's ingest socket; the logd layer is best-effort
        // and never blocks the service.
        let filter =
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into());
        let _ = tracing_subscriber::registry()
            .with(filter)
            .with(tracing_subscriber::fmt::layer())
            .with(LogdLayer::new("ados-crsf"))
            .try_init();
    }
    tracing::info!("crsf_service_starting");

    // Shutdown is a latching watch flag, not a one-shot notify: once SIGTERM
    // flips it to true the value stays set, so a select arm that loses a race
    // on the first signal still sees the shutdown on the next loop iteration.
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    tokio::spawn(async move {
        wait_for_shutdown().await;
        let _ = shutdown_tx.send(true);
    });

    // SIGHUP = in-process config reload. `notify_one` stores a permit when no
    // arm is waiting, so a reload landing mid-teardown is not lost; a burst of
    // reloads coalesces into one re-read.
    let reload = Arc::new(Notify::new());
    #[cfg(unix)]
    {
        let reload = reload.clone();
        tokio::spawn(async move {
            use tokio::signal::unix::{signal, SignalKind};
            let Ok(mut hup) = signal(SignalKind::hangup()) else {
                return;
            };
            while hup.recv().await.is_some() {
                tracing::info!("crsf_config_reload_signal");
                reload.notify_one();
            }
        });
    }

    run_until_shutdown(
        Path::new(CONFIG_YAML),
        Path::new(PROFILE_CONF),
        shutdown_rx,
        reload,
    )
    .await;
    tracing::info!("crsf_service_stopped");
}

/// Why one service pass returned: the process is stopping, or a config
/// reload asked for a fresh pass over a re-read config.
#[derive(Debug, PartialEq, Eq)]
enum RunExit {
    Shutdown,
    Reload,
}

/// The reload loop: (re)read the config and run the service until shutdown.
/// A `Reload` exit tears the pass down cleanly and re-runs every gate against
/// the fresh config; `Shutdown` ends the process.
async fn run_until_shutdown(
    config_path: &Path,
    profile_conf: &Path,
    shutdown: watch::Receiver<bool>,
    reload: Arc<Notify>,
) {
    loop {
        if *shutdown.borrow() {
            return;
        }
        let cfg = CrsfLaneConfig::load_from(config_path);
        match run_service(
            &cfg,
            config_path,
            profile_conf,
            shutdown.clone(),
            reload.clone(),
        )
        .await
        {
            RunExit::Shutdown => return,
            RunExit::Reload => {
                tracing::info!("crsf_config_reloaded");
            }
        }
    }
}

/// One pass of the service under one loaded config. Owns the per-pass worker
/// set: the HID source (when the channel-source mode wants it) is scoped to
/// this pass via a latching stop flag, so a reload never leaks a second
/// reader onto the same gamepad.
async fn run_service(
    cfg: &CrsfLaneConfig,
    config_path: &Path,
    profile_conf: &Path,
    shutdown: watch::Receiver<bool>,
    reload: Arc<Notify>,
) -> RunExit {
    let (pass_over_tx, pass_over_rx) = watch::channel(false);
    let exit = run_service_pass(
        cfg,
        config_path,
        profile_conf,
        shutdown,
        reload,
        pass_over_rx,
    )
    .await;
    // Latch the pass-scope flag so per-pass workers (the HID source) stop.
    let _ = pass_over_tx.send(true);
    exit
}

#[allow(unused_variables)] // `pass_scope` feeds the Linux-only HID source.
async fn run_service_pass(
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

    // Lane state shared across serial respawns: the channel-source merge the
    // TX task reads and the latest sidecar body the status verb serves.
    let merge = Arc::new(Mutex::new(SourceMerge::new(cfg.channel_source)));
    let latest_status = Arc::new(Mutex::new(serde_json::Value::Null));

    // The HID/PIC source: stick + switch intent from the primary gamepad,
    // fed into the merge for the whole service lifetime (the gamepad is
    // independent of the serial module's respawn cycle). Device reads are
    // Linux-only; elsewhere the hid slot stays empty, which reads as the
    // safe neutral under HID authority.
    #[cfg(target_os = "linux")]
    if cfg.channel_source != ChannelSourceMode::Inject {
        tokio::spawn(ados_crsf::hid::run_hid_source(
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
        if cfg.device.is_empty() {
            tracing::info!("crsf_no_device_pinned");
            let body = build_stats_value(LaneState::Unconfigured, &StatsInputs::default());
            *latest_status.lock().await = body;
            write_stats_sidecar(
                LaneState::Unconfigured,
                &StatsInputs::default(),
                Some(&metrics),
            );
            tokio::select! {
                biased;
                _ = wait_for_shutdown_flag(&mut shutdown) => return RunExit::Shutdown,
                _ = reload.notified() => return RunExit::Reload,
                _ = tokio::time::sleep(RETRY_INTERVAL) => continue,
            }
        }
        let Some(stream) = open_serial(&cfg.device, CRSF_BAUD) else {
            tracing::warn!(device = %cfg.device, "crsf_serial_open_failed");
            let body = build_stats_value(LaneState::Unconfigured, &StatsInputs::default());
            *latest_status.lock().await = body;
            write_stats_sidecar(
                LaneState::Unconfigured,
                &StatsInputs::default(),
                Some(&metrics),
            );
            tokio::select! {
                biased;
                _ = wait_for_shutdown_flag(&mut shutdown) => return RunExit::Shutdown,
                _ = reload.notified() => return RunExit::Reload,
                _ = tokio::time::sleep(RETRY_INTERVAL) => continue,
            }
        };
        tracing::info!(device = %cfg.device, baud = CRSF_BAUD, rate_hz = cfg.packet_rate_hz, "crsf_serial_open");

        // ── Regulatory posture at link bring-up ──────────────────────────
        // One durable record per link session: the operator posture
        // (unrestricted/region + the acknowledgement audit), the host-applied
        // frame cadence, and the module-side knob targets whose mechanism is
        // the CRSF parameter carrier — never the system radio stack. This is
        // the record behind the operator-responsibility badge, so it is
        // emitted before the first frame is transmitted.
        let reg_policy = ados_crsf::reg::RegPolicy::load_from(config_path);
        ados_crsf::reg::emit_reg_posture(&metrics, cfg, &reg_policy);

        // ── Bring-up: spawn the sibling tasks ────────────────────────────
        let (read_half, write_half) = tokio::io::split(stream);
        let counters = Arc::new(WireCounters::default());
        let telemetry = Arc::new(Mutex::new(TelemetryState::default()));
        // Per-bring-up out-of-band lane: parameter frames queued for a port
        // that dies die with it (a stale write must not fire on a fresh port).
        let oob = Arc::new(ados_crsf::transport::OobQueue::default());
        let task_cancel = Arc::new(Notify::new());

        // Bridge the latched shutdown onto the per-bring-up cancel notify so
        // worker tasks stop on SIGTERM even while mid-await.
        let mut bridge_shutdown = shutdown.clone();
        let bridge_cancel = task_cancel.clone();
        let cancel_bridge = tokio::spawn(async move {
            let _ = bridge_shutdown.wait_for(|s| *s).await;
            bridge_cancel.notify_waiters();
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
        // The flat-TX liveness watchdog: frames must keep being accepted by
        // the module at cadence. A fire breaks to the respawn loop, which
        // reinitialises the transport and re-verifies from a fresh window.
        let mut watchdog_task = tokio::spawn(ados_crsf::watchdog::tx_liveness_watchdog(
            counters.clone(),
            task_cancel.clone(),
        ));
        let cmd_state = CmdState {
            merge: merge.clone(),
            latest_status: latest_status.clone(),
            oob: oob.clone(),
        };
        let cmd_cancel = task_cancel.clone();
        let cmd_task = tokio::spawn(async move {
            let sock = ados_crsf::paths::run_path("crsf-cmd.sock");
            tokio::select! {
                r = cmdsock::serve(cmd_state, Path::new(&sock)) => {
                    if let Err(e) = r {
                        tracing::warn!(error = %e, "crsf command socket failed");
                    }
                }
                _ = cmd_cancel.notified() => {}
            }
        });

        // ── Heartbeat loop ───────────────────────────────────────────────
        let started = Instant::now();
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
                let pic_path = ados_crsf::paths::run_path("pic-state.json");
                let view = ados_crsf::sources::read_pic_view(
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
                tx_running_for: Some(now.duration_since(started)),
                stats_age,
                uplink_lq: link_copy.map(|l| l.uplink_lq),
            });
            // Only a FRESH link-statistics frame is reported: a stale reading
            // re-surfacing as current would be a lying surface.
            let fresh_link = match stats_age {
                Some(age) if age <= ados_crsf::link::STATS_FRESH_WINDOW => link_copy,
                _ => None,
            };
            let source = merge.lock().await.current(now).1.map(|s| s.as_str());
            let inputs = StatsInputs {
                link: fresh_link.as_ref(),
                // The operating band is a measurement the lane does not have
                // (link statistics do not carry it); the configured band is a
                // target, reported by the regulatory posture surface — the
                // sidecar never echoes a target as a reading.
                band: None,
                packet_rate_hz: Some(cfg.packet_rate_hz),
                tx_frames_per_s: Some(tx_fps),
                rx_frames_per_s: Some(rx_fps),
                mode: Some(cfg.mode.as_str()),
                channel_source: source,
                pic: pic_report,
                relay_role: cfg.relay_role.sidecar_str(),
                // None in this branch: the RC channel lane has no
                // MAVLink-over-ELRS command source to gate (config-derived, so
                // it stays honest if the predicate ever changes).
                fc_command_down_gated: cfg.fc_command_down_gated(),
            };
            *latest_status.lock().await = build_stats_value(state, &inputs);
            write_stats_sidecar(state, &inputs, Some(&metrics));
        };

        // ── Teardown ─────────────────────────────────────────────────────
        task_cancel.notify_waiters();
        tx_task.abort();
        rx_task.abort();
        watchdog_task.abort();
        cmd_task.abort();
        cancel_bridge.abort();

        if *shutdown.borrow() {
            return RunExit::Shutdown;
        }
        if exit_reason == "reload" {
            return RunExit::Reload;
        }
        tracing::info!(reason = exit_reason, "crsf_lane_respawning");
        let body = build_stats_value(LaneState::Unconfigured, &StatsInputs::default());
        *latest_status.lock().await = body;
        write_stats_sidecar(
            LaneState::Unconfigured,
            &StatsInputs::default(),
            Some(&metrics),
        );
        tokio::select! {
            biased;
            _ = wait_for_shutdown_flag(&mut shutdown) => return RunExit::Shutdown,
            _ = reload.notified() => return RunExit::Reload,
            _ = tokio::time::sleep(RETRY_INTERVAL) => {}
        }
    }
}

/// Wait until the latched shutdown watch flips to `true`. Returns immediately
/// if it is already set (the latch never loses an edge) and on a closed
/// channel (the sender dropped — treat as shutdown).
async fn wait_for_shutdown_flag(shutdown: &mut watch::Receiver<bool>) {
    let _ = shutdown.wait_for(|s| *s).await;
}

/// Idle in a terminal gate state until shutdown or a config reload, rewriting
/// the honest sidecar on a slow cadence so its mtime keeps proving the
/// service is alive (a staleness-gated reader must be able to tell a live
/// idle lane from a dead one). `state` is the gate's verdict — `disabled`
/// when the lane does not run on this node, `ready` when it stands by with
/// the port handed to the MAVLink router in `mavlink` mode — and `inputs`
/// carries the gate's config-level facts (the lane mode, a relay role).
async fn idle_with_sidecar(
    state: LaneState,
    metrics: &ados_protocol::logd::emitter::IngestEmitter,
    shutdown: &mut watch::Receiver<bool>,
    reload: &Notify,
    inputs: &StatsInputs<'_>,
) -> RunExit {
    write_stats_sidecar(state, inputs, Some(metrics));
    loop {
        tokio::select! {
            biased;
            _ = wait_for_shutdown_flag(shutdown) => return RunExit::Shutdown,
            _ = reload.notified() => return RunExit::Reload,
            _ = tokio::time::sleep(IDLE_REFRESH_INTERVAL) => {
                write_stats_sidecar(state, inputs, Some(metrics));
            }
        }
    }
}

/// Resolve when SIGTERM or SIGINT is received.
async fn wait_for_shutdown() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut term = signal(SignalKind::terminate()).expect("SIGTERM handler");
        let mut int = signal(SignalKind::interrupt()).expect("SIGINT handler");
        tokio::select! {
            _ = term.recv() => {}
            _ = int.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

#[cfg(test)]
mod shutdown_tests {
    use super::*;

    /// A latched shutdown watch that is already `true` resolves
    /// `wait_for_shutdown_flag` immediately — the latch never loses the edge.
    #[tokio::test]
    async fn shutdown_flag_already_set_resolves_immediately() {
        let (tx, rx) = watch::channel(false);
        tx.send(true).unwrap();
        let mut rx = rx;
        wait_for_shutdown_flag(&mut rx).await;
    }

    /// With `biased;` and the shutdown arm first, a latched shutdown wins the
    /// poll even when a competing arm is also ready in the same instant.
    #[tokio::test]
    async fn shutdown_arm_wins_over_a_ready_competitor() {
        let (tx, rx) = watch::channel(false);
        tx.send(true).unwrap();
        let mut rx = rx;
        let competitor = std::future::ready(());

        #[derive(Debug, PartialEq)]
        enum Won {
            Shutdown,
            Competitor,
        }
        let won = tokio::select! {
            biased;
            _ = wait_for_shutdown_flag(&mut rx) => Won::Shutdown,
            _ = competitor => Won::Competitor,
        };
        assert_eq!(won, Won::Shutdown);
    }

    /// A dropped sender (no more shutdown signal possible) also resolves the
    /// wait, so a vanished signaller never strands the run loop forever.
    #[tokio::test]
    async fn shutdown_flag_resolves_on_sender_drop() {
        let (tx, rx) = watch::channel(false);
        let mut rx = rx;
        drop(tx);
        wait_for_shutdown_flag(&mut rx).await;
    }

    /// Serialize the tests in this binary target that mutate the
    /// process-global `ADOS_RUN_DIR` (env vars are per-process, tests run on
    /// parallel threads). An async-aware mutex because the guard spans awaits.
    async fn env_guard() -> tokio::sync::MutexGuard<'static, ()> {
        static GUARD: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
        GUARD.lock().await
    }

    /// Poll the sidecar until its `state` reads `want` (bounded), returning
    /// the last body seen for the caller's follow-up assertions.
    async fn wait_for_state(path: &std::path::Path, want: &str) -> serde_json::Value {
        for _ in 0..400 {
            if let Ok(text) = std::fs::read_to_string(path) {
                if let Ok(body) = serde_json::from_str::<serde_json::Value>(&text) {
                    if body["state"] == want {
                        return body;
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("sidecar at {path:?} never reached state {want:?}");
    }

    /// The disabled gate writes the honest sidecar and idles until shutdown —
    /// the whole-service acid test for "absent/false enabled".
    #[tokio::test]
    async fn disabled_lane_writes_the_sidecar_and_exits_on_shutdown() {
        let _g = env_guard().await;
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("ADOS_RUN_DIR", dir.path());
        let cfg = CrsfLaneConfig {
            enabled: false,
            ..Default::default()
        };
        let (tx, rx) = watch::channel(false);
        let reload = Arc::new(Notify::new());
        let missing = dir.path().join("missing.yaml");
        let missing_conf = dir.path().join("missing.conf");
        let service =
            tokio::spawn(
                async move { run_service(&cfg, &missing, &missing_conf, rx, reload).await },
            );
        let path = dir.path().join("crsf-stats.json");
        let body = wait_for_state(&path, "disabled").await;
        assert!(body["rf_unverified"].is_null());
        // The service idles until the shutdown latch flips, then exits.
        tx.send(true).unwrap();
        let exit = tokio::time::timeout(Duration::from_secs(5), service)
            .await
            .expect("clean exit on shutdown")
            .unwrap();
        assert_eq!(exit, RunExit::Shutdown);
        std::env::remove_var("ADOS_RUN_DIR");
    }

    /// A config reload re-runs the gates in place: a lane enabled through the
    /// config surface goes from `disabled` to `unconfigured` (enabled, no
    /// device pinned) on one reload notify, with no unit restart.
    #[tokio::test]
    async fn config_reload_reruns_the_gates_without_a_restart() {
        let _g = env_guard().await;
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("ADOS_RUN_DIR", dir.path());
        let config_path = dir.path().join("config.yaml");
        let profile_conf = dir.path().join("profile.conf");
        std::fs::write(&config_path, "radio:\n  crsf:\n    enabled: false\n").unwrap();

        let (tx, rx) = watch::channel(false);
        let reload = Arc::new(Notify::new());
        let loop_reload = reload.clone();
        let loop_config = config_path.clone();
        let loop_profile = profile_conf.clone();
        let service = tokio::spawn(async move {
            run_until_shutdown(&loop_config, &loop_profile, rx, loop_reload).await
        });

        let sidecar = dir.path().join("crsf-stats.json");
        wait_for_state(&sidecar, "disabled").await;

        // Enable the lane (no device pin) and signal the reload: the fresh
        // pass passes the opt-in gate and parks at the device guard.
        std::fs::write(&config_path, "radio:\n  crsf:\n    enabled: true\n").unwrap();
        reload.notify_one();
        wait_for_state(&sidecar, "unconfigured").await;

        tx.send(true).unwrap();
        tokio::time::timeout(Duration::from_secs(5), service)
            .await
            .expect("clean exit on shutdown")
            .unwrap();
        std::env::remove_var("ADOS_RUN_DIR");
    }

    /// In `mavlink` mode the lane holds off the serial device — the MAVLink
    /// router owns the carrier — and stands by honestly: state `ready` with
    /// the mode reported, no RC transmitted, nothing flyable. The pinned
    /// device is a plain file: had the pass ever reached the device guard it
    /// would have tried (and failed) to open it and written `unconfigured`,
    /// so a stable `ready` proves the mode gate held the lane off the port.
    #[tokio::test]
    async fn mavlink_mode_holds_off_the_device_and_stands_by_ready() {
        let _g = env_guard().await;
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("ADOS_RUN_DIR", dir.path());
        let device = dir.path().join("ttyFAKE0");
        std::fs::write(&device, b"").unwrap();
        let cfg = CrsfLaneConfig {
            enabled: true,
            device: device.to_string_lossy().into_owned(),
            mode: LaneMode::Mavlink,
            ..Default::default()
        };
        let (tx, rx) = watch::channel(false);
        let reload = Arc::new(Notify::new());
        let missing = dir.path().join("missing.yaml");
        let missing_conf = dir.path().join("missing.conf");
        let service =
            tokio::spawn(
                async move { run_service(&cfg, &missing, &missing_conf, rx, reload).await },
            );
        let path = dir.path().join("crsf-stats.json");
        let body = wait_for_state(&path, "ready").await;
        assert_eq!(body["mode"], "mavlink");
        assert_eq!(body["flyable"], false, "the RC lane transmits nothing");
        // A resolved MAVLink-over-ELRS source with the command marker off (the
        // default): the host->FC command-down direction is gated closed, and the
        // lane reports it so a consumer of only this lane sees the gate.
        assert_eq!(
            body["fc_command_down_gated"], true,
            "telemetry-only mavlink source reports its command gate closed"
        );
        assert!(
            body["rf_unverified"].is_null(),
            "standing by carries no liveness verdict"
        );
        assert!(
            body["tx_frames_per_s"].is_null(),
            "no transmit counter may be fabricated while held off"
        );
        tx.send(true).unwrap();
        let exit = tokio::time::timeout(Duration::from_secs(5), service)
            .await
            .expect("clean exit on shutdown")
            .unwrap();
        assert_eq!(exit, RunExit::Shutdown);
        std::env::remove_var("ADOS_RUN_DIR");
    }

    /// A mode flip is a clean in-place reload, both directions: `crsf_rc`
    /// (parked at the device guard — the pinned node is absent) → `mavlink`
    /// (standing by `ready`, the port released for the router) → back to
    /// `crsf_rc` (the lane reclaims the device guard), with no unit restart.
    #[tokio::test]
    async fn mode_flips_between_rc_and_mavlink_are_clean_reloads() {
        let _g = env_guard().await;
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("ADOS_RUN_DIR", dir.path());
        let config_path = dir.path().join("config.yaml");
        let profile_conf = dir.path().join("profile.conf");
        let device = dir.path().join("absent-tty");
        let rc_yaml = format!(
            "radio:\n  crsf:\n    enabled: true\n    device: {}\n    mode: crsf_rc\n",
            device.display()
        );
        let mavlink_yaml = format!(
            "radio:\n  crsf:\n    enabled: true\n    device: {}\n    mode: mavlink\n",
            device.display()
        );
        std::fs::write(&config_path, &rc_yaml).unwrap();

        let (tx, rx) = watch::channel(false);
        let reload = Arc::new(Notify::new());
        let loop_reload = reload.clone();
        let loop_config = config_path.clone();
        let loop_profile = profile_conf.clone();
        let service = tokio::spawn(async move {
            run_until_shutdown(&loop_config, &loop_profile, rx, loop_reload).await
        });

        let sidecar = dir.path().join("crsf-stats.json");
        // RC mode with an absent device: the lane owns the port and parks at
        // the open guard.
        wait_for_state(&sidecar, "unconfigured").await;

        // Flip to MAVLink mode: the fresh pass hands the port to the router
        // and stands by.
        std::fs::write(&config_path, &mavlink_yaml).unwrap();
        reload.notify_one();
        let body = wait_for_state(&sidecar, "ready").await;
        assert_eq!(body["mode"], "mavlink");

        // Flip back: the lane reclaims the device guard in place.
        std::fs::write(&config_path, &rc_yaml).unwrap();
        reload.notify_one();
        let body = wait_for_state(&sidecar, "unconfigured").await;
        assert_eq!(body["mode"].as_str(), None);

        tx.send(true).unwrap();
        tokio::time::timeout(Duration::from_secs(5), service)
            .await
            .expect("clean exit on shutdown")
            .unwrap();
        std::env::remove_var("ADOS_RUN_DIR");
    }
}
