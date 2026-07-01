//! `ados-atlas` entry point: the on-drone world-model capture service.
//!
//! Resolves the `atlas:` config, gates on enablement + the agent profile (a
//! ground-station node or a disabled service exits cleanly with no work),
//! resolves the pose-source tier, then runs one capture loop: it subscribes to
//! the vision engine's frame-descriptor broadcast, tags each frame with the
//! flight controller's pose (or an offloaded SLAM pose for an NPU-less board),
//! selects keyframes, and publishes the keyframe + pose + capture-state streams
//! on the atlas bus. It runs until SIGTERM / SIGINT.

use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;

use ados_atlas::publish::AtlasPublisher;
use ados_atlas::runtime::{select_pose_tier, AtlasRuntimeConfig, CONFIG_YAML};
use ados_atlas::{
    build_pose_provider, new_session_id, run_capture_loop, serve_control, AtlasControlCmd,
    AtlasFrameSource, CaptureSession, VisionFrameSource,
};
use tokio::sync::{mpsc, Notify};

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
                .with(LogdLayer::new("ados-atlas"))
                .try_init();
            return;
        }
    }

    let _ = tracing_subscriber::registry()
        .with(EnvFilter::new(&filter))
        .with(tracing_subscriber::fmt::layer())
        .with(LogdLayer::new("ados-atlas"))
        .try_init();
}

#[tokio::main]
async fn main() {
    init_tracing();

    let config = AtlasRuntimeConfig::load_from(Path::new(CONFIG_YAML));
    tracing::info!(
        enabled = config.enabled,
        profile = ?config.profile,
        cameras = config.capture.cameras.len(),
        "ados-atlas resolved config"
    );

    // GATE A: a ground-station node has no air-side cameras.
    if config.is_ground_station() {
        tracing::info!("ados-atlas idle: ground-station profile has no capture service");
        return;
    }
    // GATE B: the service is opt-in.
    if !config.enabled {
        tracing::info!("ados-atlas idle: atlas is not enabled");
        return;
    }
    // GATE C: a capture config with no enabled camera can never produce a
    // keyframe — exit cleanly rather than spin a loop that selects nothing.
    if let Err(e) = config.capture.validate() {
        tracing::warn!(error = %e, "ados-atlas idle: invalid capture config");
        return;
    }

    // Resolve the pose-source tier. A compute node is not paired in this build
    // (that wiring lands with the offload/cluster work, which will also read the
    // real HAL accelerator capability), so `Auto` resolves to the
    // always-available local flight-controller pose unless the operator pins one.
    let tier = select_pose_tier(config.pose_tier, false, false);
    tracing::info!(?tier, "atlas pose tier resolved");

    let enabled_cams: HashSet<String> = config
        .capture
        .enabled_cameras()
        .map(|c| c.id.clone())
        .collect();

    let pose = build_pose_provider(tier, &config);
    let frames = AtlasFrameSource::Vision(VisionFrameSource::new(
        config.frames_socket_path(),
        enabled_cams,
    ));
    let publisher = match AtlasPublisher::bind(&config.atlas_socket_path()).await {
        Ok(p) => p,
        Err(e) => {
            tracing::error!(error = %e, "ados-atlas failed to bind the atlas bus");
            return;
        }
    };
    let session = CaptureSession::new(config.capture.clone());
    let cancel = Arc::new(Notify::new());

    // The inbound control socket the GCS drives the session through (start / stop
    // / pause / resume / status). A bind failure is non-fatal: the daemon still
    // auto-captures, it just cannot be driven at runtime, so it is logged and the
    // loop runs on.
    let control_socket = config.control_socket_path();
    let (control_tx, control_rx) = mpsc::channel::<AtlasControlCmd>(32);
    if let Err(e) = serve_control(&control_socket, control_tx).await {
        tracing::warn!(
            error = %e,
            path = %control_socket,
            "ados-atlas control socket bind failed; runtime capture control disabled"
        );
    }

    tracing::info!("ados-atlas ready");
    let loop_cancel = cancel.clone();
    let mut handle = tokio::spawn(async move {
        // Mint the initial session id from the device id BEFORE `config` moves into
        // the loop (argument evaluation would otherwise borrow an already-moved
        // value), so this run's keyframes carry a globally-unique, device-scoped id.
        let session_id = new_session_id(&config.device_id);
        run_capture_loop(
            frames,
            pose,
            publisher,
            session,
            config,
            session_id,
            control_rx,
            loop_cancel,
        )
        .await;
    });

    // Exit on a signal OR if the capture loop ends unexpectedly. The loop only
    // returns on `cancel`, so an early finish (a panic the runtime caught in a
    // debug build, or a future bug) means the service is alive-but-dead; surface
    // it with a non-zero exit so systemd's Restart=on-failure recovers it instead
    // of leaving a running unit that captures nothing.
    tokio::select! {
        _ = wait_for_shutdown() => {
            tracing::info!("ados-atlas stopping");
            cancel.notify_waiters();
            let _ = handle.await;
            tracing::info!("ados-atlas stopped");
        }
        res = &mut handle => {
            tracing::error!(result = ?res, "ados-atlas capture loop exited unexpectedly");
            std::process::exit(1);
        }
    }
}

/// Resolve when the service receives SIGTERM or SIGINT.
async fn wait_for_shutdown() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut term = signal(SignalKind::terminate()).expect("install SIGTERM handler");
        let mut int = signal(SignalKind::interrupt()).expect("install SIGINT handler");
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
