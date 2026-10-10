//! `ados-groundlink --ground-video`: the ground station's media server.
//!
//! Runs mediamtx with the ground profile config (path `main` reads the radio
//! RTP stream natively from UDP 5600), restarts it 3 s after any exit with no
//! attempt cap, and restarts it when its inbound counter stalls while the radio
//! is delivering. Browsers reach it through the agent's `/whep` route
//! (`127.0.0.1:8889/main/whep`).

pub mod config;
pub mod watchdog;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use ados_protocol::shutdown::Shutdown;
use ados_video::mediamtx::{MediamtxManager, MediamtxProfile, MAIN_PATH};

use watchdog::{StallWatchdog, Verdict, POLL_INTERVAL};

/// Delay before mediamtx is started again after it exits or is restarted.
pub const RESTART_DELAY: Duration = Duration::from_secs(3);

/// Run the ground media server until `shutdown` fires. `run_dir` is the agent's
/// runtime directory: the config is written under it and the wfb-stats sidecar
/// is read from it.
pub async fn run(shutdown: Shutdown, run_dir: PathBuf) {
    let config_dir = run_dir.join("mediamtx-gs");
    let stats_path = run_dir.join("wfb-stats.json");
    let mut manager = MediamtxManager::for_profile(MediamtxProfile::Ground, &config_dir);

    while !shutdown.is_fired() {
        match write_config(manager.config_path()).await {
            Ok(()) => match manager.start().await {
                Ok(true) => {
                    tracing::info!("ground_mediamtx_started");
                    supervise(&mut manager, &stats_path, &shutdown).await;
                }
                Ok(false) => tracing::error!("ground_mediamtx_exited_on_start"),
                Err(e) => tracing::error!(error = %e, "ground_mediamtx_spawn_failed"),
            },
            Err(e) => tracing::error!(error = %e, "ground_mediamtx_config_write_failed"),
        }
        manager.stop().await;
        if shutdown.is_fired() {
            break;
        }
        tokio::select! {
            _ = tokio::time::sleep(RESTART_DELAY) => {}
            _ = shutdown.wait() => {}
        }
    }
    manager.stop().await;
}

/// Watch a running mediamtx until it exits, stalls or shutdown fires.
async fn supervise(manager: &mut MediamtxManager, stats_path: &Path, shutdown: &Shutdown) {
    let mut wd = StallWatchdog::new();
    let mut tick = tokio::time::interval(POLL_INTERVAL);
    loop {
        tokio::select! {
            _ = tick.tick() => {}
            _ = shutdown.wait() => return,
        }
        if !manager.is_running() {
            tracing::warn!("ground_mediamtx_exited");
            return;
        }
        let bytes = manager.inbound_bytes(MAIN_PATH).await;
        let stats = stats_path.to_path_buf();
        let rx = tokio::task::spawn_blocking(move || watchdog::rx_active(&stats))
            .await
            .unwrap_or(false);
        if wd.observe(bytes, rx, Instant::now()) == Verdict::Restart {
            tracing::warn!(bytes = ?bytes, "ground_mediamtx_inbound_stalled_with_rx");
            return;
        }
    }
}

/// Render the ground config off the runtime and write it to `path`.
async fn write_config(path: &Path) -> std::io::Result<()> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let yaml = config::render(&ados_video::mediamtx::detect_lan_ips());
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, yaml)
    })
    .await
    .map_err(std::io::Error::other)?
}
