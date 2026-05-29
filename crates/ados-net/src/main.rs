//! `ados-net` binary skeleton.
//!
//! The runnable ground-station uplink-router daemon. It initializes logging,
//! resolves the priority list, wires the router FSM with the real ethernet and
//! Wi-Fi-client managers (the still-HW-gated cellular slot stays a stub),
//! signals systemd readiness, and runs the health loop until a shutdown signal.
//! The hostapd and modem managers plus the USB-gadget surface are added in
//! later chunks. Modeled on the `ados-cloud` binary shape: journald logging on
//! Linux with an fmt fallback.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;

use ados_net::cmd::TokioCmdRunner;
use ados_net::managers::{EthernetManager, WifiClientManager};
use ados_net::router::failover;
use ados_net::{StubManager, UplinkManager, UplinkRouter};

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

/// Notify systemd of readiness (no-op off Linux / outside a notify unit).
#[cfg(target_os = "linux")]
fn notify_ready() {
    let _ = sd_notify::notify(false, &[sd_notify::NotifyState::Ready]);
}

#[cfg(not(target_os = "linux"))]
fn notify_ready() {}

/// Resolve the physical ethernet iface name. Predictable names vary across
/// BSPs (`eth0`, `end1`, `enp*`, `enx*`), so scan `/sys/class/net` for the
/// first non-virtual wired device that exposes a carrier file; fall back to
/// `eth0` when nothing matches (the manager then reads a missing carrier as
/// "down", which is correct on a board with no NIC).
fn detect_ethernet_iface() -> String {
    let read = match std::fs::read_dir("/sys/class/net") {
        Ok(rd) => rd,
        Err(_) => return "eth0".to_string(),
    };
    let mut candidates: Vec<String> = Vec::new();
    for entry in read.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let wired = name.starts_with("eth") || name.starts_with("en");
        if !wired {
            continue;
        }
        // Skip virtual ifaces (no device symlink under the iface dir).
        let dev_link = entry.path().join("device");
        let carrier = entry.path().join("carrier");
        if dev_link.exists() && carrier.exists() {
            candidates.push(name);
        }
    }
    candidates.sort();
    candidates
        .into_iter()
        .next()
        .unwrap_or_else(|| "eth0".to_string())
}

/// Wire the real ethernet + Wi-Fi-client managers. The cellular `wwan0` slot
/// stays a stub until the modem manager chunk lands; `usb0` has no manager
/// (the FSM checks its sysfs carrier directly).
fn build_managers() -> HashMap<String, Arc<dyn UplinkManager>> {
    let runner = Arc::new(TokioCmdRunner);
    let eth_iface = detect_ethernet_iface();
    let mut m: HashMap<String, Arc<dyn UplinkManager>> = HashMap::new();
    m.insert(
        "eth0".to_string(),
        Arc::new(EthernetManager::new(eth_iface, runner.clone())),
    );
    m.insert(
        "wlan0_client".to_string(),
        Arc::new(WifiClientManager::new(runner.clone())),
    );
    m.insert("wwan0".to_string(), Arc::new(StubManager::new("wwan0")));
    m
}

#[tokio::main]
async fn main() -> Result<()> {
    init_logging();

    let priority = failover::load_priority(ados_net::paths::gs_uplink_json());
    tracing::info!(
        priority = ?priority,
        host = ados_net::health::HEALTH_HOST,
        "uplink router starting (cellular slot stubbed until the modem chunk)"
    );

    let router = Arc::new(UplinkRouter::new(build_managers(), Some(priority), None));

    notify_ready();

    // Health loop: tick now, then every HEALTH_INTERVAL, until SIGTERM/SIGINT.
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut interval = tokio::time::interval(ados_net::health::HEALTH_INTERVAL);
    loop {
        tokio::select! {
            _ = interval.tick() => {
                router.tick().await;
            }
            _ = tokio::signal::ctrl_c() => {
                tracing::info!("uplink router stopping (SIGINT)");
                break;
            }
            _ = sigterm.recv() => {
                tracing::info!("uplink router stopping (SIGTERM)");
                break;
            }
        }
    }

    Ok(())
}
