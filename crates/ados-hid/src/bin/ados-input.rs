//! `ados-input` daemon.
//!
//! Owns the input-device lifecycle for the ground-station profile: a 1 Hz
//! hotplug poll of the attached gamepads, primary-device persistence, and the
//! auto-claim feed into the PIC arbiter. On a gamepad connect it forwards
//! `on_gamepad_connected` to the `ados-pic` daemon over the PIC control socket
//! (the IPC seam) rather than holding its own arbiter, keeping a single owner.
//!
//! On a host with no `/dev/input` gamepads the poll simply reports an empty set;
//! the daemon stays up so a later hotplug is caught. Modelled on the supervisor
//! main loop.

use std::path::Path;
use std::time::Duration;

use anyhow::Result;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::signal::unix::{signal, SignalKind};

use ados_hid::input::{HotplugKind, HotplugTracker, PollOutcome, Snapshot};
use ados_hid::pic_ipc::PIC_SOCK;
use ados_hid::sidecar::GS_INPUT_JSON;

/// Client id the auto-claim runs under, matching the kiosk hint the arbiter's
/// hotplug integration uses.
const CLIENT_HINT: &str = "hdmi-kiosk";

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

/// Enumerate the current gamepad set. On Linux this reads evdev; elsewhere it is
/// always empty (the dev host has no joystick subsystem), so the diff engine and
/// the daemon loop still run.
fn enumerate() -> Snapshot {
    #[cfg(target_os = "linux")]
    {
        ados_hid::input::enumerate_gamepads()
    }
    #[cfg(not(target_os = "linux"))]
    {
        Snapshot::new()
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    init_logging();
    tracing::info!("ados-input starting");

    let state_path = Path::new(GS_INPUT_JSON);
    let mut tracker = HotplugTracker::from_sidecar(state_path);
    tracing::info!(primary = ?tracker.primary(), "input primary loaded");

    sd_ready();

    let mut tick = tokio::time::interval(Duration::from_secs_f64(
        ados_hid::input::HOTPLUG_POLL_SECONDS,
    ));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    let mut sigterm = signal(SignalKind::terminate())?;
    let mut sigint = signal(SignalKind::interrupt())?;

    loop {
        tokio::select! {
            _ = tick.tick() => {
                let snapshot = enumerate();
                let outcome = tracker.poll(snapshot);
                handle_outcome(&outcome, state_path).await;
                sd_watchdog();
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

    tracing::info!("ados-input stopped");
    Ok(())
}

/// Act on one poll: persist a newly auto-promoted primary, and forward each
/// connect to the PIC arbiter's auto-claim over the IPC seam.
async fn handle_outcome(outcome: &PollOutcome, state_path: &Path) {
    if let Some(primary) = &outcome.auto_primary {
        tracing::info!(device_id = %primary, "input primary auto-assigned");
        // The tracker already holds the new primary; persist it.
        let blob = ados_hid::sidecar::GroundStationInput {
            primary: Some(primary.clone()),
        };
        if let Err(e) = blob.save(state_path) {
            tracing::error!(error = %e, "failed to persist input primary");
        }
    }
    for ev in &outcome.events {
        match ev.kind {
            HotplugKind::Connected => {
                tracing::info!(device_id = %ev.device_id, name = %ev.name, "gamepad connected");
                // Auto-claim PIC for the kiosk on first gamepad. The arbiter
                // is a no-op when PIC is already held.
                if let Err(e) = forward_gamepad_connected(&ev.device_id).await {
                    tracing::debug!(error = %e, "pic auto-claim forward failed");
                }
            }
            HotplugKind::Disconnected => {
                tracing::info!(device_id = %ev.device_id, "gamepad disconnected");
            }
        }
    }
}

/// Send the auto-claim to the `ados-pic` daemon over the PIC control socket. The
/// arbiter's `on_gamepad_connected` is the no-op-if-claimed path; here it is
/// expressed as a non-forced `claim` under the kiosk client id, so a gamepad
/// connect grants PIC only when nobody holds it.
async fn forward_gamepad_connected(device_id: &str) -> std::io::Result<()> {
    let mut stream = UnixStream::connect(PIC_SOCK).await?;
    let req = serde_json::json!({
        "op": "claim",
        "client_id": CLIENT_HINT,
        "device_id": device_id,
    });
    let mut body = serde_json::to_vec(&req)?;
    body.push(b'\n');
    stream.write_all(&body).await?;
    stream.flush().await?;
    // Drain the one-line reply so the socket closes cleanly.
    let mut buf = [0u8; 512];
    let _ = stream.read(&mut buf).await;
    Ok(())
}
