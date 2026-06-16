//! `ados-input` daemon.
//!
//! Owns the input-device lifecycle for the ground-station profile: a 1 Hz
//! hotplug poll of the attached gamepads, primary-device persistence, and the
//! auto-claim feed into the PIC arbiter. On a gamepad connect it forwards a
//! `gamepad_connected` op to the `ados-pic` daemon over the PIC control socket
//! (the IPC seam) so the arbiter both auto-claims and records the device as the
//! PIC-bound primary, rather than holding its own arbiter, keeping a single
//! owner of PIC state.
//!
//! On a gamepad DISCONNECT it asks the arbiter which device is the PIC-bound
//! primary (`get_state` -> `primary_gamepad_id`) and, when the removed device is
//! that primary, forwards a `disconnect` op so the arbiter drops PIC. Pulling
//! the primary stick must release control; the arbiter is the single source of
//! truth for which gamepad is bound, so the daemon reads it rather than tracking
//! a separate copy.
//!
//! On a host with no `/dev/input` gamepads the poll simply reports an empty set;
//! the daemon stays up so a later hotplug is caught. Modelled on the supervisor
//! main loop.

use std::path::Path;
use std::time::Duration;

use std::sync::Arc;

use anyhow::Result;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::Mutex;

use ados_hid::hid_cmd::{self, CmdState};
use ados_hid::input::{HotplugKind, HotplugTracker, PollOutcome, Snapshot};
use ados_hid::paths::hid_cmd_sock;
use ados_hid::pic_ipc::PIC_SOCK;
use ados_hid::sidecar::GS_INPUT_JSON;

/// Client id the auto-claim runs under, matching the kiosk hint the arbiter's
/// hotplug integration uses.
const CLIENT_HINT: &str = "hdmi-kiosk";

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
                .with(LogdLayer::new("ados-input"))
                .try_init();
            return;
        }
    }

    let _ = tracing_subscriber::registry()
        .with(EnvFilter::new(&filter))
        .with(tracing_subscriber::fmt::layer())
        .with(LogdLayer::new("ados-input"))
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
    // The running hotplug tracker, shared behind a mutex with the operator command
    // socket: the poll loop locks it to diff the device set, and the command socket
    // locks it to apply a `set_primary` selection on the running state so a later
    // poll does not re-promote a different device. Single owner, two callers.
    let tracker = Arc::new(Mutex::new(HotplugTracker::from_sidecar(state_path)));
    tracing::info!(primary = ?tracker.lock().await.primary(), "input primary loaded");

    // The operator command socket (`/run/ados/hid-cmd.sock`): the native front
    // forwards the primary-gamepad selection here so it lands on the running
    // tracker (the single owner of the live primary) and persists the sidecar in
    // lockstep, rather than touching only the on-disk record. Always served — the
    // primary selection is a ground-station write the GCS Hardware tab drives.
    {
        let cmd_state = CmdState {
            tracker: tracker.clone(),
            sidecar_path: state_path.to_path_buf(),
        };
        tokio::spawn(async move {
            let sock = hid_cmd_sock();
            if let Err(e) = hid_cmd::serve(cmd_state, &sock).await {
                tracing::error!(error = %e, "input command socket exited");
            }
        });
    }

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
                let outcome = {
                    let mut t = tracker.lock().await;
                    t.poll(snapshot)
                };
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
                // Auto-claim PIC for the kiosk and bind this gamepad as the
                // PIC-bound primary. The arbiter is a no-op when PIC is held.
                if let Err(e) = forward_gamepad_connected(&ev.device_id).await {
                    tracing::debug!(error = %e, "pic auto-claim forward failed");
                }
            }
            HotplugKind::Disconnected => {
                tracing::info!(device_id = %ev.device_id, "gamepad disconnected");
                // Drop PIC when the removed device is the arbiter's PIC-bound
                // primary. Pulling the primary stick must release control.
                match forward_primary_disconnect(&ev.device_id).await {
                    Ok(true) => tracing::warn!(
                        device_id = %ev.device_id,
                        "primary gamepad removed; dropped PIC"
                    ),
                    Ok(false) => {}
                    Err(e) => tracing::debug!(error = %e, "pic disconnect forward failed"),
                }
            }
        }
    }
}

/// One newline-JSON request -> the one-line reply, parsed as JSON. Used for the
/// request/response ops on the PIC control socket.
async fn pic_request(req: &serde_json::Value) -> std::io::Result<serde_json::Value> {
    let mut stream = UnixStream::connect(PIC_SOCK).await?;
    let mut body = serde_json::to_vec(req)?;
    body.push(b'\n');
    stream.write_all(&body).await?;
    stream.flush().await?;
    let mut buf = Vec::new();
    let mut chunk = [0u8; 512];
    loop {
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.contains(&b'\n') {
            break;
        }
    }
    let line = match buf.iter().position(|&b| b == b'\n') {
        Some(i) => &buf[..i],
        None => &buf[..],
    };
    serde_json::from_slice(line)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

/// Forward a gamepad connect to the `ados-pic` daemon over the control socket.
/// The arbiter records the device as the PIC-bound primary and auto-claims for
/// the kiosk hint when nobody holds PIC; it is a no-op when PIC is already held.
async fn forward_gamepad_connected(device_id: &str) -> std::io::Result<()> {
    let req = serde_json::json!({
        "op": "gamepad_connected",
        "device_id": device_id,
        "client_id_hint": CLIENT_HINT,
    });
    let _ = pic_request(&req).await?;
    Ok(())
}

/// When `device_id` is the arbiter's PIC-bound primary, forward a `disconnect`
/// op so PIC is dropped, and return `true`. Returns `false` when the removed
/// device is not the primary (no PIC change). The arbiter owns
/// `primary_gamepad_id`, so this reads it from `get_state` rather than keeping a
/// local copy that could drift across the process boundary.
async fn forward_primary_disconnect(device_id: &str) -> std::io::Result<bool> {
    let state = pic_request(&serde_json::json!({"op": "get_state"})).await?;
    let primary = state.get("primary_gamepad_id").and_then(|v| v.as_str());
    if primary != Some(device_id) {
        return Ok(false);
    }
    let _ = pic_request(&serde_json::json!({"op": "disconnect"})).await?;
    Ok(true)
}
