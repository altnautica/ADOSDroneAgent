//! `ados-net` binary skeleton.
//!
//! The runnable ground-station uplink-router daemon. For now it initializes
//! logging, resolves the priority list, wires the router FSM with stub
//! managers, signals systemd readiness, and runs the health loop until a
//! shutdown signal. The real hardware managers (Wi-Fi client, ethernet,
//! hostapd, modem), the firewall, the USB-gadget surface, and the data-cap
//! tracker are added incrementally on top of this skeleton. Modeled on the
//! `ados-cloud` binary shape: journald logging on Linux with an fmt fallback.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;

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

/// Build the router with inert stub managers for every real-manager slot. A
/// stub's `is_up` is `false`, so until the hardware managers are wired the only
/// uplink that can go viable is `usb0` (detected via its sysfs carrier).
fn stub_managers() -> HashMap<String, Arc<dyn UplinkManager>> {
    let mut m: HashMap<String, Arc<dyn UplinkManager>> = HashMap::new();
    m.insert("eth0".to_string(), Arc::new(StubManager::new("eth0")));
    m.insert(
        "wlan0_client".to_string(),
        Arc::new(StubManager::new("wlan0_client")),
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
        "uplink router starting (skeleton: real managers not yet wired)"
    );

    let router = Arc::new(UplinkRouter::new(stub_managers(), Some(priority), None));

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
