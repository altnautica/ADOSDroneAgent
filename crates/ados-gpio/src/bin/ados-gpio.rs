//! `ados-gpio` daemon.
//!
//! The agent's GPIO-output service: it owns the host's output lines and drives a
//! status buzzer or LED on request. It serves a command socket
//! (`/run/ados/gpio-cmd.sock`) that accepts three ops — set a line high/low, play
//! a bounded beep envelope on a line, and report the current line states — and
//! mirrors the driven state to the `/run/ados/gpio-output.json` sidecar.
//!
//! Safe-by-default: the service drives no line until it receives an explicit
//! command, every beep is bounded so a line is never held high indefinitely, and
//! on shutdown it drives every owned line low. On a host with no GPIO chip the
//! line writes simply fail and are reported back as an error; the daemon stays up
//! so a later command (or a hot-plugged controller) is served.
//!
//! Wire protocol (mirrors the radio command socket framing): one newline-
//! terminated JSON request, one newline-terminated JSON response per connection,
//! then the server closes.
//!
//! ```text
//! {"op":"set","chip":0,"pin":17,"level":"high"}
//!     -> {"ok":true,"chip":0,"pin":17,"level":"high"}
//!     -> {"ok":false,"error":"E_DRIVE_FAILED: ..."}
//! {"op":"beep","pin":18,"on_ms":120,"off_ms":80,"cycles":3,"freq_hz":2700}
//!     -> {"ok":true,"phases":6}        (the bounded schedule was accepted and is
//!        playing; the reply returns before the beep finishes)
//! {"op":"status"}
//!     -> {"ok":true,"lines":[{"chip":0,"pin":17,"level":"high"}]}
//! ```

use std::path::Path;
use std::sync::Arc;

use anyhow::Result;
use serde_json::{json, Value};
use tokio::signal::unix::{signal, SignalKind};
#[cfg(target_os = "linux")]
use tokio::sync::Mutex;

use ados_gpio::sidecar::{GpioOutputState, GPIO_OUTPUT_PATH};
use ados_gpio::{beep_schedule, parse_command, Command, GPIO_CMD_SOCK};
use ados_protocol::ipc::{bind_command_socket, serve_rpc};

/// Cap on a single request line so a malformed client can't grow the buffer.
const MAX_REQUEST_BYTES: usize = 64 * 1024;

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
                .with(LogdLayer::new("ados-gpio"))
                .try_init();
            return;
        }
    }

    let _ = tracing_subscriber::registry()
        .with(EnvFilter::new(&filter))
        .with(tracing_subscriber::fmt::layer())
        .with(LogdLayer::new("ados-gpio"))
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

/// The shared driver + the sidecar path. On Linux the driver owns the real GPIO
/// lines; off Linux there is no driver, so the command path reports a clean
/// `not available` error (the daemon still builds and runs on a dev host).
#[derive(Clone)]
struct State {
    #[cfg(target_os = "linux")]
    output: Arc<Mutex<ados_gpio::GpioOutput>>,
    sidecar_path: Arc<std::path::PathBuf>,
}

#[tokio::main]
async fn main() -> Result<()> {
    init_logging();
    tracing::info!("ados-gpio starting");

    let state = State {
        #[cfg(target_os = "linux")]
        output: Arc::new(Mutex::new(ados_gpio::GpioOutput::new())),
        sidecar_path: Arc::new(std::path::PathBuf::from(GPIO_OUTPUT_PATH)),
    };

    // Publish the safe-by-default initial state (nothing driven) so a reader sees
    // the service is up with no lines energized.
    persist_state(&state).await;

    // Serve the command socket in its own task so the signal loop owns shutdown.
    let sock_state = state.clone();
    let server = tokio::spawn(async move {
        if let Err(e) = serve(sock_state, Path::new(GPIO_CMD_SOCK)).await {
            tracing::warn!(error = %e, "gpio command socket exited");
        }
    });

    sd_ready();

    let mut sigterm = signal(SignalKind::terminate())?;
    let mut sigint = signal(SignalKind::interrupt())?;
    tokio::select! {
        _ = sigterm.recv() => tracing::info!("received SIGTERM"),
        _ = sigint.recv() => tracing::info!("received SIGINT"),
    }

    server.abort();
    // Drive every owned line low so a buzzer/LED is never left energized.
    #[cfg(target_os = "linux")]
    {
        state.output.lock().await.all_low();
    }
    persist_state(&state).await;
    let _ = std::fs::remove_file(GPIO_CMD_SOCK);
    tracing::info!("ados-gpio stopped");
    Ok(())
}

/// Snapshot the driver and write the state sidecar. A write failure is logged and
/// swallowed (the sidecar is observability, never a blocker).
async fn persist_state(state: &State) {
    #[cfg(target_os = "linux")]
    let snapshot = state.output.lock().await.snapshot();
    #[cfg(not(target_os = "linux"))]
    let snapshot: Vec<(u32, u32, ados_gpio::Level)> = Vec::new();

    let blob = GpioOutputState::from_snapshot(&snapshot);
    if let Err(e) = blob.save(&state.sidecar_path) {
        tracing::debug!(error = %e, "gpio-output sidecar write failed");
    }
}

/// Bind the command socket and serve requests until the listener errors. The
/// shared helper removes a stale socket first and chmods it 0660; `set_socket_perms`
/// then group-owns it to `ados` so a non-root operator (the API service) and the
/// plugin host can write it. Each connection is one newline-terminated JSON
/// request → one newline-terminated JSON response, then close. Returns only on a
/// bind error.
async fn serve(state: State, sock_path: &Path) -> std::io::Result<()> {
    let listener = bind_command_socket(sock_path, 0o660)?;
    // bind_command_socket already applied 0o660; set_socket_perms re-applies it
    // (harmless) and additionally group-owns the socket to `ados`, which the
    // shared helper does not do.
    set_socket_perms(sock_path);
    tracing::info!(path = %sock_path.display(), "gpio command socket listening");

    serve_rpc(listener, MAX_REQUEST_BYTES, move |req: Vec<u8>| {
        let state = state.clone();
        async move {
            let resp = dispatch(&req, &state).await;
            serde_json::to_vec(&resp)
                .unwrap_or_else(|_| br#"{"ok":false,"error":"E_ENCODE"}"#.to_vec())
        }
    })
    .await;
    Ok(())
}

/// 0o660 + group-own to `ados` so a non-root operator in that group can reach the
/// trusted local plane. Best-effort; an absent group (a dev host) is a quiet
/// no-op. Linux-only.
#[cfg(target_os = "linux")]
fn set_socket_perms(sock_path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(sock_path, std::fs::Permissions::from_mode(0o660));
    match nix::unistd::Group::from_name("ados") {
        Ok(Some(g)) => {
            if let Err(err) = nix::unistd::chown(sock_path, None, Some(g.gid)) {
                tracing::debug!(error = %err, "chgrp gpio command socket failed");
            }
        }
        Ok(None) => tracing::debug!("ados group not present; leaving socket group as-is"),
        Err(err) => tracing::debug!(error = %err, "resolving ados group failed"),
    }
}

#[cfg(not(target_os = "linux"))]
fn set_socket_perms(_sock_path: &Path) {}

/// Parse + route one request. The parse half is pure (covered by the lib tests);
/// the apply half drives real lines, so it is exercised on-rig.
async fn dispatch(line: &[u8], state: &State) -> Value {
    match parse_command(line) {
        ados_gpio::Parsed::Error(code) => json!({"ok": false, "error": code}),
        ados_gpio::Parsed::Cmd(cmd) => apply(cmd, state).await,
    }
}

/// Apply a validated command to the driver and report the result.
async fn apply(cmd: Command, state: &State) -> Value {
    match cmd {
        Command::Set { chip, pin, level } => set_line(chip, pin, level, state).await,
        Command::Beep { chip, pin, pattern } => {
            let phases = beep_schedule(pattern);
            // Drop the trailing terminal-low (hold 0): the player appends its own
            // final low. Report the count of real phases so the caller sees the
            // bounded schedule was accepted.
            let real = phases.iter().filter(|p| p.hold_ms > 0).count();
            spawn_beep(chip, pin, phases, state.clone());
            json!({"ok": true, "phases": real})
        }
        Command::Status => {
            #[cfg(target_os = "linux")]
            let snapshot = state.output.lock().await.snapshot();
            #[cfg(not(target_os = "linux"))]
            let snapshot: Vec<(u32, u32, ados_gpio::Level)> = Vec::new();
            let lines: Vec<Value> = snapshot
                .iter()
                .map(|(chip, pin, level)| {
                    json!({"chip": chip, "pin": pin, "level": serde_json::to_value(level).unwrap_or(Value::Null)})
                })
                .collect();
            json!({"ok": true, "lines": lines})
        }
    }
}

/// Drive one line and mirror the new state to the sidecar.
#[cfg(target_os = "linux")]
async fn set_line(chip: u32, pin: u32, level: ados_gpio::Level, state: &State) -> Value {
    let result = state.output.lock().await.set(chip, pin, level);
    match result {
        Ok(()) => {
            persist_state(state).await;
            json!({"ok": true, "chip": chip, "pin": pin, "level": serde_json::to_value(level).unwrap_or(Value::Null)})
        }
        Err(e) => json!({"ok": false, "error": format!("E_DRIVE_FAILED: {e}")}),
    }
}

/// Off Linux there is no GPIO subsystem: report the command as unavailable rather
/// than pretending it succeeded.
#[cfg(not(target_os = "linux"))]
async fn set_line(_chip: u32, _pin: u32, _level: ados_gpio::Level, _state: &State) -> Value {
    json!({"ok": false, "error": "E_NO_GPIO"})
}

/// Play a beep schedule on a background task so the request returns immediately.
/// On Linux it drives the real line through each phase; off Linux it is a no-op
/// (there is no line to toggle), matching `set_line`.
#[cfg(target_os = "linux")]
fn spawn_beep(chip: u32, pin: u32, phases: Vec<ados_gpio::BeepPhase>, state: State) {
    tokio::spawn(async move {
        for phase in phases {
            {
                let mut out = state.output.lock().await;
                if let Err(e) = out.set(chip, pin, phase.level) {
                    tracing::debug!(chip, pin, error = %e, "beep phase drive failed");
                    break;
                }
            }
            persist_state(&state).await;
            if phase.hold_ms > 0 {
                tokio::time::sleep(std::time::Duration::from_millis(phase.hold_ms as u64)).await;
            }
        }
        // The schedule already ends low, but assert it once more so an aborted
        // mid-schedule run (or a failed phase) still returns the line low.
        {
            let mut out = state.output.lock().await;
            let _ = out.set(chip, pin, ados_gpio::Level::Low);
        }
        persist_state(&state).await;
    });
}

#[cfg(not(target_os = "linux"))]
fn spawn_beep(_chip: u32, _pin: u32, _phases: Vec<ados_gpio::BeepPhase>, _state: State) {}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> State {
        let dir = std::env::temp_dir().join(format!("ados-gpio-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        State {
            #[cfg(target_os = "linux")]
            output: Arc::new(Mutex::new(ados_gpio::GpioOutput::new())),
            sidecar_path: Arc::new(dir.join("gpio-output.json")),
        }
    }

    #[tokio::test]
    async fn bad_request_replies_with_a_stable_error() {
        let v = dispatch(b"not json", &state()).await;
        assert_eq!(v["ok"], false);
        assert!(v["error"].as_str().unwrap().starts_with("E_BAD_REQUEST"));
    }

    #[tokio::test]
    async fn unknown_op_replies_with_a_stable_error() {
        let v = dispatch(br#"{"op":"frob"}"#, &state()).await;
        assert_eq!(v["ok"], false);
        assert!(v["error"].as_str().unwrap().starts_with("E_UNKNOWN_OP"));
    }

    #[tokio::test]
    async fn status_on_a_fresh_service_reports_no_lines() {
        // Safe-by-default: nothing has been driven, so status is an empty list.
        let v = dispatch(br#"{"op":"status"}"#, &state()).await;
        assert_eq!(v["ok"], true);
        assert!(v["lines"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn beep_returns_the_bounded_phase_count_without_blocking() {
        // A beep is accepted and reports its bounded schedule; the reply returns
        // before the (background) playback finishes. Off Linux there is no line
        // to toggle, but the schedule math + the immediate reply are exercised.
        let v = dispatch(
            br#"{"op":"beep","pin":18,"on_ms":50,"off_ms":50,"cycles":2}"#,
            &state(),
        )
        .await;
        assert_eq!(v["ok"], true);
        // 2 cycles → 4 real phases (High,Low,High,Low); the terminal zero-ms low
        // is excluded from the reported count.
        assert_eq!(v["phases"], 4);
    }
}
