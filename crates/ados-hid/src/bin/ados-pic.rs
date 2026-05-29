//! `ados-pic` daemon.
//!
//! The sole owner of the PIC arbiter state. Serves the arbiter over the
//! Unix-domain control socket (the IPC seam other processes reach it through),
//! runs the session watchdog (auto-release after the heartbeat timeout), and
//! reads the front-panel GPIO buttons. Modelled on `ados-supervisor`'s main.
//!
//! On a board with no `/dev/gpiochip*` the button reader is skipped cleanly —
//! the daemon stays up for the PIC seam alone, which every ground station needs
//! regardless of whether it has physical buttons.

use std::path::Path;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use anyhow::Result;
use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::Mutex;

use ados_hid::buttons::{self, default_button_mapping};
use ados_hid::pic::{PicArbiter, WATCHDOG_INTERVAL_SECONDS};
use ados_hid::pic_ipc::{self, PIC_SOCK};

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

/// systemd readiness. No-op off Linux / outside a Type=notify unit.
#[cfg(target_os = "linux")]
fn sd_ready() {
    if let Err(e) = sd_notify::notify(false, &[sd_notify::NotifyState::Ready]) {
        tracing::debug!(error = %e, "sd_notify READY failed");
    }
}

#[cfg(not(target_os = "linux"))]
fn sd_ready() {}

#[cfg(target_os = "linux")]
fn sd_watchdog() {
    let _ = sd_notify::notify(false, &[sd_notify::NotifyState::Watchdog]);
}

#[cfg(not(target_os = "linux"))]
fn sd_watchdog() {}

#[tokio::main]
async fn main() -> Result<()> {
    init_logging();
    tracing::info!("ados-pic starting");

    let arbiter: pic_ipc::SharedArbiter = Arc::new(Mutex::new(PicArbiter::new()));

    // The IPC seam: the FastAPI /pic/* routes + the kiosk reach the arbiter
    // here. Always served — every ground station needs it.
    {
        let arbiter = arbiter.clone();
        tokio::spawn(async move {
            let path = Path::new(PIC_SOCK);
            if let Err(e) = pic_ipc::serve(arbiter, path).await {
                tracing::error!(error = %e, "pic control socket exited");
            }
        });
    }

    // Front-panel buttons: skip-clean when the board has no GPIO chip. The live
    // mapping handle is shared with the SIGHUP task so a remap is seen by the
    // next release.
    let mapping = Arc::new(RwLock::new(load_button_mapping()));
    spawn_button_reader(mapping.clone());

    sd_ready();
    tracing::info!(sock = PIC_SOCK, "ados-pic ready");

    // Session watchdog: auto-release a stale PIC after the heartbeat timeout.
    let watchdog_interval = Duration::from_secs_f64(WATCHDOG_INTERVAL_SECONDS);
    let mut tick = tokio::time::interval(watchdog_interval);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    let mut sigterm = signal(SignalKind::terminate())?;
    let mut sigint = signal(SignalKind::interrupt())?;
    let mut sighup = signal(SignalKind::hangup())?;

    loop {
        tokio::select! {
            _ = tick.tick() => {
                let released = {
                    let mut arb = arbiter.lock().await;
                    arb.watchdog_tick()
                };
                if let Some(client) = released {
                    tracing::info!(client_id = %client, "pic auto-released on heartbeat timeout");
                }
                sd_watchdog();
            }
            _ = sighup.recv() => {
                tracing::info!("SIGHUP: reloading button mapping");
                let merged = load_button_mapping();
                if let Ok(mut m) = mapping.write() {
                    *m = merged;
                }
            }
            _ = sigterm.recv() => {
                tracing::info!("received SIGTERM");
                break;
            }
            _ = sigint.recv() => {
                tracing::info!("received SIGINT");
                break;
            }
        }
    }

    tracing::info!("ados-pic stopped");
    Ok(())
}

/// Load + merge `ground_station.ui.buttons.mapping` over the defaults. A missing
/// config or block falls back to the defaults verbatim.
fn load_button_mapping() -> std::collections::HashMap<String, String> {
    match read_buttons_block() {
        Some(block) => buttons::merge_mapping(&block),
        None => default_button_mapping(),
    }
}

/// Read the `ground_station.ui.buttons.mapping` node from the agent config, if
/// present. Best-effort: any read/parse error yields `None` and the caller uses
/// the defaults.
fn read_buttons_block() -> Option<serde_norway::Value> {
    let path = std::env::var("ADOS_CONFIG").unwrap_or_else(|_| "/etc/ados/config.yaml".to_string());
    let text = std::fs::read_to_string(path).ok()?;
    let root: serde_norway::Value = serde_norway::from_str(&text).ok()?;
    root.get("ground_station")?
        .get("ui")?
        .get("buttons")?
        .get("mapping")
        .cloned()
}

/// Spawn the button reader on a blocking task when a GPIO chip exists; otherwise
/// log the skip and return. Only the chip-open is hardware-coupled; the
/// classification runs through the shared [`PressClassifier`].
#[cfg(target_os = "linux")]
fn spawn_button_reader(mapping: Arc<RwLock<std::collections::HashMap<String, String>>>) {
    use ados_hid::buttons::{gpio_subsystem_present, ButtonEvent, PressClassifier, BUTTON_PINS};

    if !gpio_subsystem_present() {
        tracing::info!("button reader skipped: no /dev/gpiochip*");
        return;
    }
    tokio::task::spawn_blocking(move || {
        let mut classifier = PressClassifier::with_mapping(mapping);
        let on_event = |ev: ButtonEvent| {
            tracing::info!(
                pin = ev.pin,
                kind = ev.kind.as_str(),
                action = ?ev.action,
                "button event"
            );
        };
        // gpiochip0 is the conventional primary chip on the supported boards.
        if let Err(e) =
            buttons::run_event_loop("/dev/gpiochip0", &BUTTON_PINS, &mut classifier, on_event)
        {
            // A read error here is not fatal to the PIC seam; log and let the
            // daemon stay up serving the socket.
            tracing::warn!(error = %e, "button reader exited");
        }
    });
}

/// Off Linux there is no character-device GPIO; the reader is a no-op so the
/// daemon still builds and serves the PIC seam on a dev host.
#[cfg(not(target_os = "linux"))]
fn spawn_button_reader(_mapping: Arc<RwLock<std::collections::HashMap<String, String>>>) {
    tracing::debug!("button reader unavailable off Linux");
}
