//! `ados-crsf` binary — the CRSF / ExpressLRS RC control lane service.
//!
//! Opens the pinned USB-serial RC transmitter module at 420 kbaud, transmits
//! the packed RC channels frame at the configured cadence, decodes returned
//! telemetry, serves the command socket, and writes the lane state sidecar
//! every second. Idles harmlessly (with an honest `disabled` sidecar) when
//! the lane is not opted in or this node's profile does not run it, and shuts
//! down cleanly on SIGTERM/SIGINT.

use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::{watch, Mutex, Notify};

use ados_crsf::bank::ChannelBank;
use ados_crsf::cmdsock::{self, CmdState};
use ados_crsf::config::{profile_is_drone, CrsfLaneConfig};
use ados_crsf::link::{derive_state, LaneState, LinkInputs};
use ados_crsf::sidecar::{build_stats_value, write_stats_sidecar, StatsInputs};
use ados_crsf::transport::{open_serial, run_rx, run_tx, TelemetryState, WireCounters, CRSF_BAUD};

const CONFIG_YAML: &str = "/etc/ados/config.yaml";
const PROFILE_CONF: &str = "/etc/ados/profile.conf";
/// Sidecar heartbeat cadence while the lane runs.
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(1);
/// Backoff between bring-up attempts (no device, open failure, port death).
const RETRY_INTERVAL: Duration = Duration::from_secs(5);

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

    let cfg = CrsfLaneConfig::load_from(Path::new(CONFIG_YAML));

    // Shutdown is a latching watch flag, not a one-shot notify: once SIGTERM
    // flips it to true the value stays set, so a select arm that loses a race
    // on the first signal still sees the shutdown on the next loop iteration.
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    tokio::spawn(async move {
        wait_for_shutdown().await;
        let _ = shutdown_tx.send(true);
    });

    run_service(&cfg, shutdown_rx).await;
    tracing::info!("crsf_service_stopped");
}

async fn run_service(cfg: &CrsfLaneConfig, mut shutdown: watch::Receiver<bool>) {
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
        write_stats_sidecar(LaneState::Disabled, &StatsInputs::default(), Some(&metrics));
        wait_for_shutdown_flag(&mut shutdown).await;
        return;
    }

    // ── Profile gate ─────────────────────────────────────────────────────
    // The RC transmitter lane is ground-side; on a drone this binary idles
    // (defensive — the unit is already profile-gated at install time). The
    // sidecar reads `disabled`: the lane does not run on this node.
    if profile_is_drone(Path::new(CONFIG_YAML), Path::new(PROFILE_CONF)) {
        tracing::warn!("crsf_idle_on_drone_profile");
        write_stats_sidecar(LaneState::Disabled, &StatsInputs::default(), Some(&metrics));
        wait_for_shutdown_flag(&mut shutdown).await;
        return;
    }

    // Lane state shared across serial respawns: the live channel bank and
    // the latest sidecar body the status verb serves.
    let bank = Arc::new(Mutex::new(ChannelBank::default()));
    let latest_status = Arc::new(Mutex::new(serde_json::Value::Null));

    loop {
        // Latched shutdown gate at the top of the respawn loop.
        if *shutdown.borrow() {
            return;
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
                _ = wait_for_shutdown_flag(&mut shutdown) => return,
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
                _ = wait_for_shutdown_flag(&mut shutdown) => return,
                _ = tokio::time::sleep(RETRY_INTERVAL) => continue,
            }
        };
        tracing::info!(device = %cfg.device, baud = CRSF_BAUD, rate_hz = cfg.packet_rate_hz, "crsf_serial_open");

        // ── Bring-up: spawn the sibling tasks ────────────────────────────
        let (read_half, write_half) = tokio::io::split(stream);
        let counters = Arc::new(WireCounters::default());
        let telemetry = Arc::new(Mutex::new(TelemetryState::default()));
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
            bank.clone(),
            cfg.packet_rate_hz,
            counters.clone(),
            task_cancel.clone(),
        ));
        let mut rx_task = tokio::spawn(run_rx(
            read_half,
            telemetry.clone(),
            counters.clone(),
            task_cancel.clone(),
        ));
        let cmd_state = CmdState {
            bank: bank.clone(),
            latest_status: latest_status.clone(),
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
                r = &mut tx_task => {
                    tracing::warn!(exit = ?r, "crsf tx task exited");
                    break "tx_exit";
                }
                r = &mut rx_task => {
                    tracing::warn!(exit = ?r, "crsf rx task exited");
                    break "rx_exit";
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
            let source = bank.lock().await.source().map(|s| s.as_str());
            let inputs = StatsInputs {
                link: fresh_link.as_ref(),
                band: None,
                packet_rate_hz: Some(cfg.packet_rate_hz),
                tx_frames_per_s: Some(tx_fps),
                rx_frames_per_s: Some(rx_fps),
                mode: Some("rc"),
                channel_source: source,
                relay_role: None,
            };
            *latest_status.lock().await = build_stats_value(state, &inputs);
            write_stats_sidecar(state, &inputs, Some(&metrics));
        };

        // ── Teardown ─────────────────────────────────────────────────────
        task_cancel.notify_waiters();
        tx_task.abort();
        rx_task.abort();
        cmd_task.abort();
        cancel_bridge.abort();

        if *shutdown.borrow() {
            return;
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
            _ = wait_for_shutdown_flag(&mut shutdown) => return,
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

    /// The disabled gate writes the honest sidecar and idles until shutdown —
    /// the whole-service acid test for "absent/false enabled".
    #[tokio::test]
    async fn disabled_lane_writes_the_sidecar_and_exits_on_shutdown() {
        let dir = tempfile::tempdir().unwrap();
        // Not using the shared env guard here: this is the only test in THIS
        // binary target that touches ADOS_RUN_DIR (the lib tests are a
        // separate process).
        std::env::set_var("ADOS_RUN_DIR", dir.path());
        let cfg = CrsfLaneConfig {
            enabled: false,
            ..Default::default()
        };
        let (tx, rx) = watch::channel(false);
        let service = tokio::spawn(async move { run_service(&cfg, rx).await });
        // Wait for the disabled sidecar to land.
        let path = dir.path().join("crsf-stats.json");
        for _ in 0..200 {
            if path.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let body: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(body["state"], "disabled");
        assert!(body["rf_unverified"].is_null());
        // The service idles until the shutdown latch flips, then exits.
        tx.send(true).unwrap();
        tokio::time::timeout(Duration::from_secs(5), service)
            .await
            .expect("clean exit on shutdown")
            .unwrap();
        std::env::remove_var("ADOS_RUN_DIR");
    }
}
