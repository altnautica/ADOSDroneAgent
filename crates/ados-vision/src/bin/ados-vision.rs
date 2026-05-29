//! `ados-vision` entry point.
//!
//! Resolves the `vision:` config, gates on enablement + the agent profile (a
//! ground-station node or a disabled engine exits cleanly with no work), picks
//! an inference backend for the board, then starts:
//!
//! - one capture task per configured (or HAL-discovered) camera, each opening
//!   its frame source and publishing normalized frames into the engine's ring,
//! - the `/run/ados/vision.sock` request/response server the plugin host
//!   connects to.
//!
//! It runs until SIGTERM / SIGINT. The accelerator runtime is never linked
//! here: NPU inference is reached through the Python sidecar, ONNX is an opt-in
//! cargo feature, and the default build runs the mock backend.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use ados_vision::backend::{select_backend, BackendPrefs};
use ados_vision::config::{CameraEntry, VisionConfig};
use ados_vision::engine::VisionEngine;
use ados_vision::ring::now_ms;
use ados_vision::source::{
    discover_cameras_default, AnySource, CaptureSource, FrameSource, TapSource,
};
use ados_vision::visionsock;
use ados_protocol::framebus::FrameFormat;
use tokio::sync::Notify;

/// Canonical agent config file.
const CONFIG_YAML: &str = "/etc/ados/config.yaml";
/// Board override file the HAL writes the resolved SoC into. Best-effort read
/// to pick the backend; absence falls back to the env / a generic SoC.
const BOARD_OVERRIDE: &str = "/etc/ados/board_override";

fn init_tracing() {
    let filter = std::env::var("RUST_LOG").unwrap_or_else(|_| "info".to_string());

    #[cfg(target_os = "linux")]
    {
        use tracing_subscriber::prelude::*;
        use tracing_subscriber::EnvFilter;
        if let Ok(journald) = tracing_journald::layer() {
            let _ = tracing_subscriber::registry()
                .with(EnvFilter::new(&filter))
                .with(journald)
                .try_init();
            return;
        }
    }

    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::new(&filter))
        .try_init();
}

/// systemd readiness ping. No-op off Linux and when not run under a
/// `Type=notify` unit (`NOTIFY_SOCKET` unset).
#[cfg(target_os = "linux")]
fn sd_ready() {
    if let Err(e) = sd_notify::notify(false, &[sd_notify::NotifyState::Ready]) {
        tracing::debug!(error = %e, "sd_notify READY failed");
    }
}

#[cfg(not(target_os = "linux"))]
fn sd_ready() {}

/// Read the resolved SoC for backend selection. Prefers `ADOS_BOARD_SOC`, then
/// the board-override file's first token, else a generic placeholder.
fn board_soc() -> String {
    if let Ok(soc) = std::env::var("ADOS_BOARD_SOC") {
        if !soc.is_empty() {
            return soc;
        }
    }
    if let Ok(text) = std::fs::read_to_string(BOARD_OVERRIDE) {
        if let Some(first) = text.split_whitespace().next() {
            return first.to_string();
        }
    }
    "generic".to_string()
}

#[tokio::main]
async fn main() {
    init_tracing();

    let config = VisionConfig::load_from(Path::new(CONFIG_YAML));
    tracing::info!(
        enabled = config.enabled,
        profile = ?config.profile,
        backend = %config.backend,
        cameras = config.cameras.len(),
        "ados-vision resolved config"
    );

    // GATE A: a ground-station node has no air-side cameras.
    if config.is_ground_station() {
        tracing::info!("ados-vision idle: ground-station profile has no vision engine");
        return;
    }
    // GATE B: the engine is opt-in.
    if !config.enabled {
        tracing::info!("ados-vision idle: vision is not enabled");
        return;
    }

    let soc = board_soc();
    let backend = select_backend(
        &soc,
        &BackendPrefs {
            preference: &config.backend,
            rknn_socket_path: config.rknn_socket_path(),
        },
    );
    tracing::info!(soc = %soc, backend = backend.name(), "vision backend selected");

    let slot_count = config.slot_count;
    let engine = VisionEngine::new(backend, slot_count);
    let cancel = Arc::new(Notify::new());

    // Resolve the camera set: an explicit config list wins; otherwise HAL
    // discovery enumerates the engine cameras (each tapped by default).
    let cameras = resolve_cameras(&config).await;
    tracing::info!(count = cameras.len(), "vision cameras resolved");

    let mut tasks = Vec::new();

    // One capture task per camera.
    for cam in cameras {
        let engine = engine.clone();
        let cancel = cancel.clone();
        let downscale = (config.downscale_width, config.downscale_height);
        tasks.push(tokio::spawn(async move {
            run_camera(engine, cam, downscale, cancel).await;
        }));
    }

    // The vision.sock server.
    {
        let engine = engine.clone();
        let cancel = cancel.clone();
        let sock = config.vision_socket_path();
        tasks.push(tokio::spawn(async move {
            if let Err(e) = visionsock::serve(engine, &sock, cancel).await {
                tracing::error!(error = %e, "vision_sock_serve_failed");
            }
        }));
    }

    sd_ready();
    tracing::info!("ados-vision ready");
    wait_for_shutdown().await;
    tracing::info!("ados-vision stopping");
    cancel.notify_waiters();
    for t in tasks {
        let _ = t.await;
    }
    tracing::info!("ados-vision stopped");
}

/// A resolved camera the engine captures: its id, its source kind, and the
/// source-specific endpoint (tap socket path or capture device).
struct ResolvedCamera {
    id: String,
    kind: String,
    tap_socket: String,
    device_path: Option<String>,
}

/// Build the camera list. An explicit `vision.cameras` config list is used as
/// given (tap paths defaulted per id); an empty list triggers HAL discovery,
/// which yields one tap-source camera per discovered device.
async fn resolve_cameras(config: &VisionConfig) -> Vec<ResolvedCamera> {
    if !config.cameras.is_empty() {
        return config
            .cameras
            .iter()
            .map(|c: &CameraEntry| ResolvedCamera {
                id: c.id.clone(),
                kind: c.kind.clone(),
                tap_socket: c
                    .tap_socket
                    .clone()
                    .unwrap_or_else(|| config.tap_socket_for(&c.id)),
                device_path: c.device_path.clone(),
            })
            .collect();
    }
    // No explicit list: discover and default each to a tap source. The video
    // pipeline writes the tap; a camera the pipeline does not own simply never
    // produces frames, which the capture task handles as a quiet retry.
    discover_cameras_default()
        .await
        .into_iter()
        .map(|d| ResolvedCamera {
            id: d.device_path.replace('/', "-").trim_start_matches('-').to_string(),
            kind: "tap".to_string(),
            tap_socket: config.tap_socket_for(&d.device_path.replace('/', "-")),
            device_path: Some(d.device_path),
        })
        .collect()
}

/// Run one camera's capture loop: open its source, pull frames, downscale-stamp
/// is left to the source (Phase 1 publishes the source's native format), and
/// publish each into the engine ring. A source error backs off and re-opens.
async fn run_camera(
    engine: Arc<VisionEngine>,
    cam: ResolvedCamera,
    _downscale: (u32, u32),
    cancel: Arc<Notify>,
) {
    let mut frame_id: u64 = 0;
    loop {
        // Build the source for this attempt.
        let mut source: AnySource = match cam.kind.as_str() {
            "capture" => {
                let device = match &cam.device_path {
                    Some(d) => d.clone(),
                    None => {
                        tracing::warn!(camera = %cam.id, "capture camera has no device_path; idling");
                        if backoff(&cancel).await {
                            return;
                        }
                        continue;
                    }
                };
                // Capture at the downscale target so each frame is a fixed size.
                AnySource::Capture(CaptureSource::new(
                    cam.id.clone(),
                    device,
                    _downscale.0,
                    _downscale.1,
                    FrameFormat::Nv12,
                ))
            }
            // Default and "tap": read the video pipeline tap socket.
            _ => AnySource::Tap(TapSource::new(cam.id.clone(), cam.tap_socket.clone())),
        };

        loop {
            tokio::select! {
                frame = source.next_frame() => {
                    match frame {
                        Ok(raw) => {
                            frame_id = frame_id.wrapping_add(1);
                            if let Err(e) = engine
                                .publish_frame(
                                    &cam.id,
                                    frame_id,
                                    now_ms(),
                                    raw.width,
                                    raw.height,
                                    raw.format,
                                    &raw.data,
                                )
                                .await
                            {
                                tracing::warn!(camera = %cam.id, error = %e, "publish_frame_failed");
                            }
                        }
                        Err(e) => {
                            tracing::debug!(camera = %cam.id, error = %e, "frame source ended; reopening");
                            break;
                        }
                    }
                }
                _ = cancel.notified() => return,
            }
        }
        if backoff(&cancel).await {
            return;
        }
    }
}

/// A short backoff that also wakes on shutdown. Returns `true` when shutdown
/// fired during the wait (the caller should stop), `false` when the full
/// backoff elapsed (the caller should reopen its source).
async fn backoff(cancel: &Arc<Notify>) -> bool {
    tokio::select! {
        _ = tokio::time::sleep(Duration::from_millis(500)) => false,
        _ = cancel.notified() => true,
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
