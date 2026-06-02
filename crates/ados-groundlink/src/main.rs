//! Entry point for the ground-station data-plane service.
//!
//! Dispatches on the mesh role: `direct` runs the standalone WFB receive
//! manager (this file's `receive_loop`); `relay` forwards drone fragments to a
//! receiver over batman-adv; `receiver` aggregates the local NIC + remote relay
//! forwards and republishes the combined FEC stream. The role comes from the
//! `--role` argument when present, else the `/etc/ados/mesh/role` sentinel
//! (`role_manager` owns that file). The relay/receiver roles run as their own
//! systemd units (`ados-wfb-relay` / `ados-wfb-receiver`), each invoking this
//! binary with the matching `--role`.
//!
//! Direct-role detail: per generation it spawns the data RX + both control
//! planes, starts the video fan-out and the presence emit/listen loops as
//! sub-services, and runs the stats reader, the valid-packet watchdog, and the
//! stdout-silence zombie watchdog concurrently. When the data RX exits (or a
//! watchdog terminates it), the generation ends and the loop respawns with a
//! bounded backoff.
//!
//! Adapter detection for the direct receive plane takes the already-prepared
//! interface from config; the relay/receiver roles run the shared radio
//! selector themselves (adapter detect + monitor mode) before spawning their
//! forwarder/aggregator. The rx-key pairing gate and regulatory-domain/tx-power
//! application stay where they were.

use std::sync::atomic::AtomicU32;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::{Mutex, Notify};

use ados_radio::config::WfbConfig;
use ados_radio::link_quality::LinkStats;

use ados_groundlink::wfb_rx::{
    self, DataRxHandle, IwChannelSetter, SharedValidCounter, SystemClock, WfbRxManager,
};
use ados_groundlink::{fanout, mesh, presence, receiver, relay, GsPresenceCache};

const CONFIG_YAML: &str = "/etc/ados/config.yaml";
const RX_KEY: &str = ados_radio::paths::WFB_RX_KEY;

/// Resolve the run role: an explicit `--role <value>` argument wins, else the
/// on-disk sentinel. Unknown values fall back to `direct`.
fn resolve_role() -> String {
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--role" {
            if let Some(v) = args.next() {
                if matches!(v.as_str(), "direct" | "relay" | "receiver") {
                    return v;
                }
                tracing::warn!(value = %v, "unknown_role_arg_falling_back");
            }
        } else if let Some(v) = arg.strip_prefix("--role=") {
            if matches!(v, "direct" | "relay" | "receiver") {
                return v.to_string();
            }
            tracing::warn!(value = %v, "unknown_role_arg_falling_back");
        }
    }
    mesh::get_current_role()
}

fn init_logging() {
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
                .with(LogdLayer::new("ados-groundlink"))
                .try_init();
            return;
        }
    }

    let _ = tracing_subscriber::registry()
        .with(EnvFilter::new(&filter))
        .with(tracing_subscriber::fmt::layer())
        .with(LogdLayer::new("ados-groundlink"))
        .try_init();
}

#[tokio::main]
async fn main() -> Result<()> {
    init_logging();

    // Tell systemd we are up (reuses the orchestrator's notify shim).
    ados_supervisor::sdnotify::ready();

    let mut sigterm = signal(SignalKind::terminate())?;
    let mut sigint = signal(SignalKind::interrupt())?;

    let role = resolve_role();
    match role.as_str() {
        "relay" => {
            tracing::info!("ground-station relay role starting");
            run_relay_or_receiver(true, &mut sigterm, &mut sigint).await;
        }
        "receiver" => {
            tracing::info!("ground-station receiver role starting");
            run_relay_or_receiver(false, &mut sigterm, &mut sigint).await;
        }
        _ => {
            run_direct(&mut sigterm, &mut sigint).await?;
        }
    }

    tracing::info!("ground-station data-plane stopping");
    Ok(())
}

/// Run the relay (`is_relay`) or receiver loop until a shutdown signal. The
/// chosen loop owns its own adapter detect + monitor-mode + mDNS + state file;
/// a SIGTERM/SIGINT fires the shared `Notify` so the loop tears down cleanly.
async fn run_relay_or_receiver(
    is_relay: bool,
    sigterm: &mut tokio::signal::unix::Signal,
    sigint: &mut tokio::signal::unix::Signal,
) {
    let shutdown = Arc::new(Notify::new());

    // Observability: publish the mesh snapshot (neighbors / gateways /
    // selected-gateway) so the REST layer + OLED see the fabric. This is the
    // same poll the direct path skips; the relay/receiver FEC supervision below
    // is independent of it.
    let role_label = if is_relay { "relay" } else { "receiver" };
    let snap = mesh::MeshSnapshot::new(role_label, "bat0", "802.11s");
    tokio::spawn(mesh::run_poll_loop(snap));

    let role_task = {
        let shutdown = shutdown.clone();
        tokio::spawn(async move {
            if is_relay {
                relay::run(shutdown).await;
            } else {
                receiver::run(shutdown).await;
            }
        })
    };
    tokio::select! {
        _ = role_task => {}
        _ = sigterm.recv() => {
            tracing::info!("received SIGTERM");
            shutdown.notify_waiters();
        }
        _ = sigint.recv() => {
            tracing::info!("received SIGINT");
            shutdown.notify_waiters();
        }
    }
    // Give the loop a moment to flush its down-state on signal-triggered exit.
    tokio::time::sleep(Duration::from_millis(200)).await;
}

/// The standalone (`direct`) receive plane.
async fn run_direct(
    sigterm: &mut tokio::signal::unix::Signal,
    sigint: &mut tokio::signal::unix::Signal,
) -> Result<()> {
    let config = WfbConfig::load_from(std::path::Path::new(CONFIG_YAML));
    tracing::info!(
        channel = config.channel,
        band = %config.band,
        interface = %config.interface,
        "ground-station data-plane starting (direct role)"
    );

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
    // Export the GS-side hop-supervisor snapshot (band + hop-follow history) to
    // /run/ados/hop-supervisor.json so the REST layer + the on-box channel-hops
    // page read the same surface the Python listener produced. Service-wide, so
    // the history survives receive-plane restarts.
    {
        let hop_cache = presence_cache.clone();
        let band = config.band.clone();
        tokio::spawn(presence::hop_supervisor_persist_loop(hop_cache, band));
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
    Ok(())
}

/// The receive manager's main loop: spawn a generation, run it to completion,
/// restart with bounded backoff. Mirrors the Python `WfbRxManager.run` structure
/// (sans the Python-owned adapter-detect/pairing gate).
async fn receive_loop(config: &WfbConfig, presence_cache: GsPresenceCache) {
    let mut manager = WfbRxManager::new(config.clone());
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
        // Resolve the receive adapter. Honors an explicit `video.wfb.interface`
        // override; otherwise auto-detects the RTL injection adapter (the
        // management wifi and the operator's control path are excluded and
        // monitor mode is proven) — symmetric with the drone-side selection, so
        // the ground station resolves its own adapter instead of idling until an
        // external detector supplies one.
        let interface = match ados_radio::adapter::select_interface(&config.interface).await {
            Some(sel) if sel.injection_ok => {
                manager.set_adapter(Some(sel.chipset.clone()), true);
                manager.set_interface(sel.ifname.clone());
                sel.ifname
            }
            Some(sel) => {
                manager.set_adapter(Some(sel.chipset.clone()), false);
                tracing::warn!(interface = %sel.ifname, "ground_wfb_adapter_no_injection");
                tokio::time::sleep(Duration::from_secs(backoff as u64)).await;
                backoff = (backoff * 2.0).min(30.0);
                continue;
            }
            None => {
                manager.set_adapter(None, false);
                tracing::warn!("ground_no_wfb_adapter_found");
                tokio::time::sleep(Duration::from_secs(backoff as u64)).await;
                backoff = (backoff * 2.0).min(30.0);
                continue;
            }
        };

        // Bring the interface to receive-ready BEFORE the spawn, in the
        // kernel-required order: regulatory domain (global, before monitor-mode
        // bring-up so the home channel is not capped to the startup domain's
        // limits), then monitor mode, TX power (brownout guard on marginal USB
        // hosts), and the rendezvous-home channel. Also resolves the
        // regulatory-permitted channel set the acquirer intersects its sweep
        // against. Re-applied each generation, matching the Python restart cycle.
        manager.prepare_interface(&interface).await;

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
        // Live receive-health publish seam: the valid-packet watchdog writes its
        // reacquire-kill total + the valid-decode silence here, and the stats
        // reader pulls them onto the sidecar so the GS heartbeat carries the real
        // values instead of hardcoded zeros.
        let rx_health = wfb_rx::SharedRxHealth::new();

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
                Some(rx_health.clone()),
                zombie_kills.clone(),
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
