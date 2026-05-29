//! Entry point for the ground-station data-plane service.
//!
//! Initializes logging (journald on Linux, fmt fallback elsewhere), signals
//! readiness to systemd, then runs the WFB receive manager: per generation it
//! spawns the data RX + both control planes, starts the video fan-out and the
//! presence emit/listen loops as sub-services, and runs the stats reader, the
//! valid-packet watchdog, and the stdout-silence zombie watchdog concurrently.
//! When the data RX exits (or a watchdog terminates it), the generation ends and
//! the loop respawns with a bounded backoff.
//!
//! Adapter detection, the rx-key pairing gate, monitor-mode setup, and the
//! regulatory-domain/tx-power application stay in Python (the HAL and pairing
//! flow own those); this binary takes the already-prepared interface from
//! config and drives the live receive plane.

use std::sync::atomic::AtomicU32;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::Mutex;

use ados_radio::config::WfbConfig;
use ados_radio::link_quality::LinkStats;

use ados_groundlink::wfb_rx::{
    self, DataRxHandle, IwChannelSetter, SharedValidCounter, SystemClock, WfbRxManager,
};
use ados_groundlink::{fanout, presence, GsPresenceCache};

const CONFIG_YAML: &str = "/etc/ados/config.yaml";
const RX_KEY: &str = ados_radio::paths::WFB_RX_KEY;

fn init_logging() {
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

#[tokio::main]
async fn main() -> Result<()> {
    init_logging();

    let config = WfbConfig::load_from(std::path::Path::new(CONFIG_YAML));
    tracing::info!(
        channel = config.channel,
        band = %config.band,
        interface = %config.interface,
        "ground-station data-plane starting"
    );

    // Tell systemd we are up (reuses the orchestrator's notify shim).
    ados_supervisor::sdnotify::ready();

    let mut sigterm = signal(SignalKind::terminate())?;
    let mut sigint = signal(SignalKind::interrupt())?;

    // The presence listen loop + cache run for the whole service lifetime (the
    // listener feeds the per-generation watchdog its peer-presence signal). The
    // emit loop runs service-wide too; both survive receive-plane restarts.
    let presence_cache = GsPresenceCache::new();
    tokio::spawn(presence::listen_loop(presence_cache.clone()));
    {
        // The beacon's channel is a hint; the configured channel is a safe
        // service-wide source (the live channel the watchdog locks is surfaced
        // on the sidecar, not the beacon).
        let beacon_channel = config.channel;
        tokio::spawn(presence::emit_loop(move || beacon_channel));
    }

    // Run the receive loop until a shutdown signal arrives.
    tokio::select! {
        _ = receive_loop(&config, presence_cache) => {}
        _ = sigterm.recv() => {
            tracing::info!("received SIGTERM");
        }
        _ = sigint.recv() => {
            tracing::info!("received SIGINT");
        }
    }

    tracing::info!("ground-station data-plane stopping");
    Ok(())
}

/// The receive manager's main loop: spawn a generation, run it to completion,
/// restart with bounded backoff. Mirrors the Python `WfbRxManager.run` structure
/// (sans the Python-owned adapter-detect/pairing gate).
async fn receive_loop(config: &WfbConfig, presence_cache: GsPresenceCache) {
    let manager = WfbRxManager::new(config.clone());
    let interface = manager.interface().to_string();
    let clock: Arc<dyn ados_groundlink::watchdog::Clock> = Arc::new(SystemClock::default());
    let setter: Arc<dyn ados_groundlink::acquire::ChannelSetter> = Arc::new(IwChannelSetter);
    let hint = wfb_rx::default_hint();

    let mut backoff = 1.0_f64;
    loop {
        // Pairing gate: without the rx key on disk there is nothing to receive.
        // (The Python side blocks here too; the pairing flow lands the key.)
        if !std::path::Path::new(RX_KEY).exists() {
            tracing::info!(expected = RX_KEY, "ground_wfb_blocked_unpaired");
            tokio::time::sleep(Duration::from_secs(5)).await;
            continue;
        }
        if interface.is_empty() {
            // No interface resolved (adapter-detect stays in Python). Idle and
            // wait for config to carry one rather than spin.
            tracing::warn!("ground_no_wfb_adapter_found");
            tokio::time::sleep(Duration::from_secs(backoff as u64)).await;
            backoff = (backoff * 2.0).min(30.0);
            continue;
        }

        // Spawn the receive chain for this generation.
        let (mut data_rx, _rx_control, _tx_control) =
            match manager.spawn_receive_chain(&interface).await {
                Ok(chain) => chain,
                Err(e) => {
                    tracing::error!(error = %e, "ground_wfb_rx_failed_to_start");
                    tokio::time::sleep(Duration::from_secs(backoff as u64)).await;
                    backoff = (backoff * 2.0).min(5.0);
                    continue;
                }
            };
        backoff = 1.0;

        let stdout = data_rx.take_stdout();
        let rx_handle = DataRxHandle::new(data_rx);

        // Shared liveness state for this generation.
        let counter = SharedValidCounter::new();
        let link = Arc::new(Mutex::new(LinkStats::default()));
        let last_stdout_at = Arc::new(Mutex::new(clock.monotonic()));
        let zombie_kills = Arc::new(AtomicU32::new(0));

        // Fan-out as a sub-service (5599 → 5600 mediamtx + 5605 LCD), aborted
        // with the generation.
        let fanout_task = tokio::spawn(fanout::run_default_fanout());

        // Stats reader: feeds the counter + LinkStats + the sidecar.
        let stats_task = stdout.map(|out| {
            tokio::spawn(wfb_rx::stats_reader_loop(
                out,
                counter.clone(),
                link.clone(),
                last_stdout_at.clone(),
                clock.clone(),
                interface.clone(),
                manager.channel(),
                config.clone(),
                None,
                true,
            ))
        });

        // Zombie watchdog (stdout-silence).
        let zombie_task = tokio::spawn(wfb_rx::zombie_watchdog(
            rx_handle.clone(),
            last_stdout_at.clone(),
            clock.clone(),
            zombie_kills.clone(),
        ));

        // Valid-packet watchdog: owns a fresh acquirer, reads the shared counter
        // + presence cache, terminates the data RX on a genuine loss.
        let mut watchdog = manager.build_watchdog(
            counter.clone(),
            presence_cache.clone(),
            rx_handle.clone(),
            clock.clone(),
            setter.clone(),
            hint.clone(),
        );
        let watchdog_task = tokio::spawn(async move {
            watchdog.run().await;
        });

        // The generation ends when any of: the data RX exits, the zombie
        // watchdog kills it, or the valid-packet watchdog terminates it.
        tokio::select! {
            _ = wait_for_exit(rx_handle.clone()) => {
                tracing::warn!("ground_wfb_rx_exited");
            }
            _ = zombie_task => {}
            _ = watchdog_task => {}
        }

        // Tear down the generation's sub-tasks before respawning.
        fanout_task.abort();
        if let Some(t) = stats_task {
            t.abort();
        }

        tokio::time::sleep(Duration::from_secs(backoff as u64)).await;
        backoff = (backoff * 2.0).min(5.0);
    }
}

/// Poll the data-RX handle until it reports not-running. One arm of the
/// generation's completion select.
async fn wait_for_exit(rx: Arc<DataRxHandle>) {
    use ados_groundlink::watchdog::RxProcess;
    loop {
        if !rx.is_running() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}
