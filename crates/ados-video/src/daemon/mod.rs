//! `ados-video` daemon entry.
//!
//! Resolves the agent video config, gates on the profile + mode (a
//! ground-station node or a `video.mode: disabled` rig exits cleanly with no
//! pipeline), wires the shutdown + camera-hotplug signals, cold-starts the
//! pipeline, opts into cloud push when a relay URL is configured, and hands the
//! orchestrator its run loop.

mod signals;

use std::path::Path;
use std::process::ExitCode;

use ados_protocol::shutdown::Shutdown;

use crate::config::{AgentVideoConfig, CameraConfig};
use crate::orchestrator::VideoOrchestrator;

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

/// Run the `ados-video` daemon to completion.
pub fn run() -> ExitCode {
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("ados-video: failed to build tokio runtime: {e}");
            return ExitCode::FAILURE;
        }
    };
    runtime.block_on(run_async());
    ExitCode::SUCCESS
}

async fn run_async() {
    init_tracing();

    let config = AgentVideoConfig::load_from(Path::new(CONFIG_YAML));
    let camera_cfg = CameraConfig::load_from(Path::new(CONFIG_YAML));
    tracing::info!(
        mode = %config.mode,
        profile = ?config.profile,
        cloud = config.cloud_enabled(),
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
    // Probe the encoder environment on the blocking pool before constructing
    // the orchestrator: the probe shells `gst-inspect-1.0` up to three times
    // and is slow on a cold SBC page cache, so it must not run on a reactor
    // worker.
    let env = crate::encoder::EncoderEnv::detect_async().await;
    let mut orch = VideoOrchestrator::new(config, camera_cfg, Path::new(CONFIG_DIR), env);
    let camera_plugged = orch.camera_plugged_handle();

    // Shutdown + signal wiring. SIGTERM / SIGINT trigger the shutdown handle;
    // SIGUSR1 (a fresh /dev/video* node, per the udev rule) retries an
    // error-state pipeline at once instead of on the next tick.
    let shutdown = Shutdown::new();
    #[cfg(target_os = "linux")]
    signals::spawn_signal_handlers(shutdown.clone(), camera_plugged.clone());
    #[cfg(not(target_os = "linux"))]
    {
        // On the dev host only Ctrl-C drives shutdown; SIGUSR1 has no role.
        let _ = &camera_plugged;
        signals::spawn_ctrl_c_handler(shutdown.clone());
    }

    // The encoder command socket: the ONE cross-process entry point through
    // which the operator's hero choice (ados-control) and the adaptive-bitrate
    // ladder (ados-radio) retarget this encoder. Served before the cold start so
    // a promotion issued during startup is queued rather than refused.
    {
        let control = orch.encoder_control();
        let shutdown = shutdown.clone();
        tokio::spawn(async move {
            crate::profile::serve(
                control,
                Path::new(crate::profile::VIDEO_ENCODER_SOCK),
                shutdown,
            )
            .await;
        });
    }

    // Hero lease expiry: an abandoned lease reverts the encoder to its base
    // profile with no further request.
    tokio::spawn(crate::profile::lease::run_lease_expiry(
        orch.encoder_control(),
        shutdown.clone(),
    ));

    // Cold start. A failed start lands the orchestrator in Error; the run
    // loop's retry-from-error path then takes over (USB hotplug, late mediamtx,
    // etc.), so a cold-start failure is not fatal to the service.
    let started = orch.start_stream().await;
    if started && cloud_enabled {
        orch.start_cloud_push().await;
    }

    orch.run(shutdown).await;
}
