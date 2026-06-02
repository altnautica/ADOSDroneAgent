//! `ados-video` entry point.
//!
//! Resolves the agent video config, gates on the profile + mode (a
//! ground-station node or a `video.mode: disabled` rig exits cleanly with no
//! pipeline), wires the shutdown + camera-hotplug signals, cold-starts the
//! pipeline, opts into cloud push when a relay URL is configured, and hands the
//! orchestrator its run loop.

use std::path::Path;

use ados_video::config::{AgentVideoConfig, CameraConfig};
use ados_video::orchestrator::VideoOrchestrator;
use ados_video::shutdown::Shutdown;

/// Canonical agent config file.
const CONFIG_YAML: &str = "/etc/ados/config.yaml";
/// Directory the mediamtx config is written into.
const CONFIG_DIR: &str = "/etc/ados";

fn init_tracing() {
    use ados_protocol::logd::layer::LogdLayer;
    use tracing_subscriber::prelude::*;
    use tracing_subscriber::EnvFilter;

    let filter = std::env::var("RUST_LOG").unwrap_or_else(|_| "info".to_string());

    // The logd layer ships records to the logging daemon's ingest socket
    // alongside the primary sink; it is best-effort and never blocks the service.
    #[cfg(target_os = "linux")]
    {
        if let Ok(journald) = tracing_journald::layer() {
            let _ = tracing_subscriber::registry()
                .with(EnvFilter::new(&filter))
                .with(journald)
                .with(LogdLayer::new("ados-video"))
                .try_init();
            return;
        }
    }

    let _ = tracing_subscriber::registry()
        .with(EnvFilter::new(&filter))
        .with(tracing_subscriber::fmt::layer())
        .with(LogdLayer::new("ados-video"))
        .try_init();
}

#[tokio::main]
async fn main() {
    init_tracing();

    let config = AgentVideoConfig::load_from(Path::new(CONFIG_YAML));
    let camera_cfg = CameraConfig::load_from(Path::new(CONFIG_YAML));
    tracing::info!(
        mode = %config.mode,
        profile = ?config.profile,
        cloud = config.cloud_enabled(),
        gst_air = config.use_gst_air_pipeline,
        sei_latency = config.wfb.sei_latency,
        "ados-video resolved config"
    );

    // GATE A: a ground-station node never runs the air-side pipeline.
    if config.is_ground_station() {
        tracing::info!("ados-video idle: ground-station profile has no air-side video pipeline");
        return;
    }
    // GATE B: an explicitly-disabled video mode runs no pipeline.
    if config.is_disabled() {
        tracing::info!("ados-video idle: video.mode is disabled");
        return;
    }

    let cloud_enabled = config.cloud_enabled();
    let mut orch = VideoOrchestrator::new(config, camera_cfg, Path::new(CONFIG_DIR));
    let camera_plugged = orch.camera_plugged_handle();

    // Shutdown + signal wiring. SIGTERM / SIGINT trigger the shutdown handle;
    // SIGUSR1 (a fresh /dev/video* node, per the udev rule) wakes the
    // no-primary-camera backoff sleep.
    let shutdown = Shutdown::new();
    #[cfg(target_os = "linux")]
    spawn_signal_handlers(shutdown.clone(), camera_plugged.clone());
    #[cfg(not(target_os = "linux"))]
    {
        // On the dev host only Ctrl-C drives shutdown; SIGUSR1 has no role.
        let _ = &camera_plugged;
        spawn_ctrl_c_handler(shutdown.clone());
    }

    // Cold start. A failed start lands the orchestrator in Error; the run
    // loop's retry-from-error path then takes over (USB hotplug, late mediamtx,
    // etc.), so a cold-start failure is not fatal to the service.
    let started = orch.start_stream().await;
    if started && cloud_enabled {
        orch.start_cloud_push().await;
    }

    orch.run(shutdown).await;
}

#[cfg(target_os = "linux")]
fn spawn_signal_handlers(shutdown: Shutdown, camera_plugged: std::sync::Arc<tokio::sync::Notify>) {
    use tokio::signal::unix::{signal, SignalKind};

    tokio::spawn(async move {
        let mut sigterm = match signal(SignalKind::terminate()) {
            Ok(s) => s,
            Err(e) => {
                tracing::error!(error = %e, "failed to install SIGTERM handler");
                return;
            }
        };
        let mut sigint = match signal(SignalKind::interrupt()) {
            Ok(s) => s,
            Err(e) => {
                tracing::error!(error = %e, "failed to install SIGINT handler");
                return;
            }
        };
        let mut sigusr1 = match signal(SignalKind::user_defined1()) {
            Ok(s) => s,
            Err(e) => {
                tracing::error!(error = %e, "failed to install SIGUSR1 handler");
                return;
            }
        };
        loop {
            tokio::select! {
                _ = sigterm.recv() => {
                    tracing::info!("received SIGTERM");
                    shutdown.trigger();
                    break;
                }
                _ = sigint.recv() => {
                    tracing::info!("received SIGINT");
                    shutdown.trigger();
                    break;
                }
                _ = sigusr1.recv() => {
                    tracing::debug!("received SIGUSR1: camera hotplug");
                    camera_plugged.notify_one();
                }
            }
        }
    });
}

#[cfg(not(target_os = "linux"))]
fn spawn_ctrl_c_handler(shutdown: Shutdown) {
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            tracing::info!("received Ctrl-C");
            shutdown.trigger();
        }
    });
}
