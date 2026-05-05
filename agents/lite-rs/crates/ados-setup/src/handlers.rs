//! Axum handlers for the universal setup REST surface.
//!
//! Every handler returns response shapes byte-for-byte compatible with
//! the Python reference at `src/ados/api/routes/setup.py`. A conformance
//! conformance test suite replays Python responses against this implementation
//! to keep the two halves in sync.

use std::sync::Arc;

use axum::{
    extract::{Path as AxumPath, State, WebSocketUpgrade},
    http::StatusCode,
    response::{IntoResponse, Json, Response},
};
use serde_json::{json, Value};

use crate::cloud::apply_cloud_choice;
use crate::cloudflare::{install_cloudflare_token, redact_log_line, verify_tunnel_async};
use crate::hardware::run_hardware_check;
use crate::models::{
    CloudChoiceRequest, CloudflareTokenRequest, ProfileChoiceRequest, SetupActionResult,
    REQUIRED_STEP_IDS, VALID_STEP_IDS,
};
use crate::profile::apply_profile;
use crate::router::SetupState;

// ---------------------------------------------------------------------------
// Status
// ---------------------------------------------------------------------------

pub async fn get_status(State(state): State<Arc<SetupState>>) -> Json<Value> {
    Json(state.snapshot_status().await)
}

// ---------------------------------------------------------------------------
// Profile
// ---------------------------------------------------------------------------

pub async fn post_profile(
    State(state): State<Arc<SetupState>>,
    Json(req): Json<ProfileChoiceRequest>,
) -> Response {
    match apply_profile(&state.agent_yaml, &req.profile, req.ground_role.as_deref()) {
        Ok(()) => action_ok("profile saved", state.snapshot_status().await),
        Err(e) => action_err(&format!("invalid profile request: {e}")),
    }
}

// ---------------------------------------------------------------------------
// Hardware check
// ---------------------------------------------------------------------------

pub async fn get_hardware_check(State(state): State<Arc<SetupState>>) -> Json<Value> {
    let (profile, ground_role) = read_profile_from_agent_yaml(&state.agent_yaml);
    // run_hardware_check shells out to lsusb + reads /proc + /sys
    // synchronously. On the Luckfox single-core A7 this can block ~200 ms;
    // hand it off to the blocking pool so the axum handler thread (which
    // also serves the WS log stream + every other route) doesn't stall.
    let status = tokio::task::spawn_blocking(move || run_hardware_check(&profile, &ground_role))
        .await
        .ok();
    let status = status.unwrap_or_else(|| run_hardware_check("drone", ""));
    Json(serde_json::to_value(status).unwrap_or_else(|_| json!({})))
}

pub async fn post_hardware_check_refresh(State(state): State<Arc<SetupState>>) -> Json<Value> {
    let (profile, ground_role) = read_profile_from_agent_yaml(&state.agent_yaml);
    let status = tokio::task::spawn_blocking(move || run_hardware_check(&profile, &ground_role))
        .await
        .ok();
    let status = status.unwrap_or_else(|| run_hardware_check("drone", ""));
    Json(serde_json::to_value(status).unwrap_or_else(|_| json!({})))
}

/// Read the active profile + ground_role from agent.yaml. Defaults to
/// "drone" / "" so the hardware-check still runs sensibly on a fresh
/// install before the operator has confirmed a profile.
fn read_profile_from_agent_yaml(path: &std::path::Path) -> (String, String) {
    let raw = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(_) => return ("drone".into(), String::new()),
    };
    let doc: serde_yaml::Value = match serde_yaml::from_str(&raw) {
        Ok(v) => v,
        Err(_) => return ("drone".into(), String::new()),
    };
    let profile = doc
        .get("agent")
        .and_then(|a| a.get("profile"))
        .and_then(|v| v.as_str())
        .unwrap_or("drone")
        .to_string();
    let role = doc
        .get("ground_station")
        .and_then(|g| g.get("role"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    (profile, role)
}

// ---------------------------------------------------------------------------
// Cloud choice
// ---------------------------------------------------------------------------

pub async fn post_cloud_choice(
    State(state): State<Arc<SetupState>>,
    Json(req): Json<CloudChoiceRequest>,
) -> Response {
    match apply_cloud_choice(&state.agent_yaml, &req.mode, req.self_hosted.as_ref()) {
        Ok(()) => action_ok("cloud choice saved", state.snapshot_status().await),
        Err(e) => action_err(&format!("invalid cloud choice: {e}")),
    }
}

// ---------------------------------------------------------------------------
// Cloudflare Tunnel
// ---------------------------------------------------------------------------

pub async fn post_cloudflare_install(
    State(state): State<Arc<SetupState>>,
    Json(req): Json<CloudflareTokenRequest>,
) -> Response {
    match install_cloudflare_token(&req.token_or_script) {
        Ok(()) => action_ok(
            "cloudflared token persisted; tunnel service starts via the orchestration module",
            state.snapshot_status().await,
        ),
        Err(e) => action_err(&format!("could not install token: {e}")),
    }
}

pub async fn get_cloudflare_verify(State(state): State<Arc<SetupState>>) -> Json<Value> {
    let target = read_cloudflare_setup_url(&state.agent_yaml);
    let resp = verify_tunnel_async(target.as_deref()).await;
    Json(serde_json::to_value(&resp).unwrap_or_else(|_| json!({})))
}

/// Read the operator's Cloudflare Tunnel public setup URL from
/// agent.yaml. Mirrors the Python reference's
/// `app.config.remote_access.cloudflare.setup_url` lookup.
fn read_cloudflare_setup_url(path: &std::path::Path) -> Option<String> {
    let raw = std::fs::read_to_string(path).ok()?;
    let doc: serde_yaml::Value = serde_yaml::from_str(&raw).ok()?;
    doc.get("remote_access")
        .and_then(|r| r.get("cloudflare"))
        .and_then(|c| c.get("setup_url"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

pub async fn ws_cloudflare_logs(ws: WebSocketUpgrade) -> Response {
    // Stream cloudflared service logs over WebSocket. journalctl is the
    // canonical source on systemd; on busybox we tail /var/log if a
    // cloudflared.log exists. Either way we redact JWT-shaped substrings
    // before emitting to the wizard so a future cloudflared regression
    // that logs a bearer doesn't leak it through the WS.
    ws.on_upgrade(|socket| async move {
        if let Err(e) = stream_cloudflared_logs(socket).await {
            tracing::warn!(error = %e, "cloudflared log WS exited with error");
        }
    })
}

/// Absolute paths we are willing to spawn `journalctl` from. We refuse to
/// fall back to PATH lookup so a subverted `$PATH` (operator prepended
/// `/tmp/bin:$PATH`, attacker dropped a malicious `journalctl` there)
/// cannot redirect the subprocess.
const JOURNALCTL_CANDIDATES: &[&str] =
    &["/usr/bin/journalctl", "/bin/journalctl", "/sbin/journalctl"];

/// Absolute paths we are willing to spawn `tail` from. Same rationale as
/// `JOURNALCTL_CANDIDATES`.
const TAIL_CANDIDATES: &[&str] = &["/usr/bin/tail", "/bin/tail"];

/// Return the first candidate that exists on disk, or `None` if no
/// trusted absolute path is available.
fn find_absolute<'a>(candidates: &'a [&'a str]) -> Option<&'a str> {
    candidates
        .iter()
        .copied()
        .find(|p| std::path::Path::new(p).exists())
}

async fn stream_cloudflared_logs(
    mut socket: axum::extract::ws::WebSocket,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    use axum::extract::ws::Message;
    use tokio::io::{AsyncBufReadExt, BufReader};
    use tokio::process::Command;

    let mut child = if std::path::Path::new("/run/systemd/system").is_dir() {
        // Resolve journalctl from a fixed allowlist of absolute paths;
        // refuse to inherit `$PATH`. Also null-redirect stderr so the
        // child never blocks on a full kernel pipe buffer (~64 KiB) when
        // journalctl writes warnings.
        let journalctl_path = match find_absolute(JOURNALCTL_CANDIDATES) {
            Some(p) => p,
            None => {
                let _ = socket
                    .send(Message::Text(
                        "(journalctl not found at /usr/bin, /bin, or /sbin — cannot stream cloudflared logs)".into(),
                    ))
                    .await;
                let _ = socket.close().await;
                return Ok(());
            }
        };
        match Command::new(journalctl_path)
            .args([
                "-u",
                "cloudflared",
                "-f",
                "-n",
                "120",
                "--no-pager",
                "-o",
                "short",
            ])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
        {
            Ok(c) => c,
            Err(e) => {
                let _ = socket
                    .send(Message::Text(format!(
                        "(journalctl failed to start: {e})"
                    )))
                    .await;
                let _ = socket.close().await;
                return Ok(());
            }
        }
    } else {
        // Best-effort fallback: tail /var/log/cloudflared.log if it
        // exists. busybox doesn't always ship `tail -f`; coreutils users
        // get the log live, others see a single snapshot.
        let log_path = "/var/log/cloudflared.log";
        if !std::path::Path::new(log_path).exists() {
            let _ = socket
                .send(Message::Text(
                    "(cloudflared logs not available on this init system — install systemd or pipe logs to /var/log/cloudflared.log)".into(),
                ))
                .await;
            let _ = socket.close().await;
            return Ok(());
        }
        // Resolve tail from a fixed allowlist of absolute paths; refuse
        // to inherit `$PATH` for the same reason as journalctl above.
        let tail_path = match find_absolute(TAIL_CANDIDATES) {
            Some(p) => p,
            None => {
                let _ = socket
                    .send(Message::Text(
                        "(tail not found at /usr/bin or /bin — cannot stream cloudflared logs)".into(),
                    ))
                    .await;
                let _ = socket.close().await;
                return Ok(());
            }
        };
        match Command::new(tail_path)
            .args(["-n", "120", "-f", log_path])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
        {
            Ok(c) => c,
            Err(e) => {
                let _ = socket
                    .send(Message::Text(format!("(tail failed: {e})")))
                    .await;
                let _ = socket.close().await;
                return Ok(());
            }
        }
    };
    let stdout = child.stdout.take().ok_or("no stdout")?;
    // Cap line length at 8 KiB. cloudflared's normal log lines are well
    // under 1 KiB; an unbounded reader would be a memory-amplification
    // gadget when the upstream emits a hostile MESSAGE= field. We truncate
    // after-the-fact below rather than at the reader because next_line()
    // bounds against a sane growth ceiling already (Lines holds one
    // partial line in the BufReader's internal buffer, default 8 KiB).
    const MAX_LINE_BYTES: usize = 8 * 1024;
    let mut reader = BufReader::new(stdout).lines();
    // Hard cap on the WS connection. Operators that walk away with the
    // setup wizard tab open should not keep a journalctl subprocess
    // running forever; reconnect for a fresh 15-minute window.
    const MAX_SESSION: std::time::Duration = std::time::Duration::from_secs(15 * 60);
    let session_deadline = tokio::time::sleep(MAX_SESSION);
    tokio::pin!(session_deadline);

    loop {
        tokio::select! {
            line = reader.next_line() => {
                match line {
                    Ok(Some(mut text)) => {
                        // Truncate over-long lines after-the-fact rather
                        // than dropping them entirely so the operator
                        // still sees something useful in the wizard.
                        if text.len() > MAX_LINE_BYTES {
                            text.truncate(MAX_LINE_BYTES);
                            text.push_str(" ...(truncated)");
                        }
                        let redacted = redact_log_line(&text);
                        if socket.send(Message::Text(redacted)).await.is_err() {
                            break; // peer disconnected
                        }
                    }
                    Ok(None) => break,
                    Err(_) => break,
                }
            }
            _ = socket.recv() => {
                // Anything inbound from the peer closes the stream.
                break;
            }
            _ = &mut session_deadline => {
                // 15 minutes elapsed; close politely so a forgotten tab
                // does not hold a subprocess forever. The wizard can
                // reconnect to resume tailing.
                let _ = socket
                    .send(Message::Text(
                        "(session timeout — refresh the wizard to resume log streaming)".into(),
                    ))
                    .await;
                break;
            }
        }
    }
    // Kill + reap with a 2s timeout — a stuck child must not hold the
    // WebSocket forever.
    let _ = child.kill().await;
    let _ = tokio::time::timeout(std::time::Duration::from_secs(2), child.wait()).await;
    let _ = socket.close().await;
    Ok(())
}

// ---------------------------------------------------------------------------
// Wizard navigation
// ---------------------------------------------------------------------------

pub async fn post_finish(State(state): State<Arc<SetupState>>) -> Response {
    if let Err(e) = state.store.mark_finalized() {
        return action_err(&format!("could not persist finalized state: {e}"));
    }
    Json(state.snapshot_status().await).into_response()
}

pub async fn post_skip(
    State(state): State<Arc<SetupState>>,
    AxumPath(step_id): AxumPath<String>,
) -> Response {
    if !VALID_STEP_IDS.contains(&step_id.as_str()) {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "detail": format!("Unknown step id: {step_id}") })),
        )
            .into_response();
    }
    if REQUIRED_STEP_IDS.contains(&step_id.as_str()) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "detail": format!("Step '{step_id}' cannot be skipped") })),
        )
            .into_response();
    }
    if let Err(e) = state.store.mark_skipped(&step_id) {
        return action_err(&format!("could not persist skip: {e}"));
    }
    Json(state.snapshot_status().await).into_response()
}

pub async fn post_reset(State(state): State<Arc<SetupState>>) -> Response {
    if let Err(e) = state.store.reset() {
        return action_err(&format!("could not reset state: {e}"));
    }
    Json(state.snapshot_status().await).into_response()
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn action_ok(message: &str, status: Value) -> Response {
    Json(SetupActionResult {
        ok: true,
        message: Some(message.to_string()),
        status,
    })
    .into_response()
}

fn action_err(message: &str) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(SetupActionResult {
            ok: false,
            message: Some(message.to_string()),
            status: Value::Null,
        }),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_absolute_returns_first_existing() {
        // /usr/bin and /bin (or one of them) exists on every Unix
        // host that runs this test; pick whichever is present.
        let candidates: &[&str] = &["/nonexistent/foo/bar", "/usr/bin", "/bin"];
        let found = find_absolute(candidates);
        assert!(found == Some("/usr/bin") || found == Some("/bin"));
    }

    #[test]
    fn find_absolute_returns_none_when_all_missing() {
        let candidates: &[&str] = &[
            "/nonexistent/aaa",
            "/nonexistent/bbb",
            "/nonexistent/ccc",
        ];
        assert_eq!(find_absolute(candidates), None);
    }

    #[test]
    fn journalctl_candidates_are_absolute_paths() {
        // No relative paths or bare names — refusing PATH lookup is the
        // whole point of the allowlist.
        for p in JOURNALCTL_CANDIDATES {
            assert!(p.starts_with('/'), "non-absolute candidate: {p}");
        }
        for p in TAIL_CANDIDATES {
            assert!(p.starts_with('/'), "non-absolute candidate: {p}");
        }
    }
}
