//! Axum handlers for the universal setup REST surface.
//!
//! Every handler returns response shapes byte-for-byte compatible with
//! the Python reference at `src/ados/api/routes/setup.py`. A conformance
//! conformance test suite replays Python responses against this implementation
//! to keep the two halves in sync.

use std::sync::Arc;

use axum::{
    extract::{Extension, Path as AxumPath, State, WebSocketUpgrade},
    http::StatusCode,
    response::{IntoResponse, Json, Response},
};
use serde_json::{json, Value};

use crate::cloud::apply_cloud_choice;
use crate::cloudflare::{install_cloudflare_token, redact_log_line, verify_tunnel_async};
use crate::diag::{now_unix_seconds, read_rss_mb, DiagState};
use crate::hardware::run_hardware_check;
use crate::models::{
    CloudChoiceRequest, CloudflareTokenRequest, ProfileChoiceRequest, SetupActionResult,
    REQUIRED_STEP_IDS, VALID_STEP_IDS,
};
use crate::profile::apply_profile;
use crate::router::SetupState;
use crate::wfb_driver::check_wfb_driver;

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
    Json(run_hardware_check_blocking(profile, ground_role).await)
}

pub async fn post_hardware_check_refresh(State(state): State<Arc<SetupState>>) -> Json<Value> {
    let (profile, ground_role) = read_profile_from_agent_yaml(&state.agent_yaml);
    Json(run_hardware_check_blocking(profile, ground_role).await)
}

/// Run the hardware-check sweep + WFB driver pre-flight on the blocking
/// pool and return the merged JSON body. The hardware-check shells out
/// to lsusb + reads /proc + /sys synchronously; the WFB driver probe
/// reads /proc/modules and /etc/udev/rules.d. On the Luckfox single-core
/// A7 the combined latency lands at ~250 ms, well clear of the axum
/// handler thread which also serves the WS log stream and every other
/// route. The merged body adds a `wfb_driver` field at the top level
/// alongside the canonical HardwareCheckStatus shape; older clients
/// that ignore unknown fields stay wire-compatible.
async fn run_hardware_check_blocking(profile: String, ground_role: String) -> Value {
    let merged = tokio::task::spawn_blocking(move || {
        let status = run_hardware_check(&profile, &ground_role);
        let driver = check_wfb_driver();
        merge_hardware_check_payload(&status, &driver)
    })
    .await;
    match merged {
        Ok(v) => v,
        Err(_) => {
            // Fall back to a synchronous run on the handler thread when
            // the blocking pool spawn fails (effectively impossible on
            // a healthy tokio runtime, but handle it without panicking).
            let status = run_hardware_check("drone", "");
            let driver = check_wfb_driver();
            merge_hardware_check_payload(&status, &driver)
        }
    }
}

/// Combine the canonical `HardwareCheckStatus` body with the WFB
/// driver pre-flight tile. Failure paths collapse to an empty object
/// rather than 500 — the wizard renders an "unknown" tile in that
/// case rather than blocking the operator.
fn merge_hardware_check_payload(
    status: &crate::models::HardwareCheckStatus,
    driver: &crate::wfb_driver::WfbDriverCheck,
) -> Value {
    let mut body = serde_json::to_value(status).unwrap_or_else(|_| json!({}));
    if let Some(map) = body.as_object_mut() {
        let driver_value = serde_json::to_value(driver).unwrap_or_else(|_| json!({}));
        map.insert("wfb_driver".to_string(), driver_value);
    }
    body
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
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
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
    // Cap per-line growth at 8 KiB. cloudflared's normal log lines are
    // well under 1 KiB; an unbounded reader (e.g. AsyncBufReadExt::lines,
    // which calls read_line internally and grows the line buffer until
    // \n is seen) is a memory-amplification gadget when the upstream
    // emits a hostile MESSAGE= field with no newline. We use an explicit
    // read_until against a take(MAX_LINE_BYTES) limiter so a single
    // pathological line can never push the buffer past the ceiling. When
    // the cap is hit without a trailing \n we drain the rest of the
    // over-long line into io::sink() and tag the emitted prefix as
    // truncated so the operator still sees something useful.
    const MAX_LINE_BYTES: usize = 8 * 1024;
    let mut reader = BufReader::new(stdout);
    let mut line_buf: Vec<u8> = Vec::with_capacity(MAX_LINE_BYTES);
    // Hard cap on the WS connection. Operators that walk away with the
    // setup wizard tab open should not keep a journalctl subprocess
    // running forever; reconnect for a fresh 15-minute window.
    const MAX_SESSION: std::time::Duration = std::time::Duration::from_secs(15 * 60);
    let session_deadline = tokio::time::sleep(MAX_SESSION);
    tokio::pin!(session_deadline);

    loop {
        tokio::select! {
            read_result = async {
                let mut limited = (&mut reader).take(MAX_LINE_BYTES as u64);
                limited.read_until(b'\n', &mut line_buf).await
            } => {
                match read_result {
                    Ok(0) => break, // EOF
                    Ok(_) => {
                        // The line either ended with \n (the last byte is
                        // \n) or it was truncated at exactly
                        // MAX_LINE_BYTES because the take() limiter cut
                        // us off before a newline appeared. In the
                        // truncated case we drain the rest of the
                        // over-long line into a fixed scratch buffer so
                        // the next iteration lines up on the next real
                        // logical line boundary, without any unbounded
                        // allocation.
                        let truncated = !line_buf.ends_with(b"\n");
                        if line_buf.ends_with(b"\n") {
                            line_buf.pop();
                            if line_buf.ends_with(b"\r") {
                                line_buf.pop();
                            }
                        }
                        let mut text = String::from_utf8_lossy(&line_buf).into_owned();
                        line_buf.clear();
                        if truncated {
                            text.push_str(" ...(truncated)");
                            // Drain forward to the next \n (or EOF) using
                            // a fixed-size scratch that we clear between
                            // iterations. Memory ceiling = MAX_LINE_BYTES
                            // for this scratch, regardless of how long
                            // the rest of the over-long line runs.
                            let mut scratch: Vec<u8> = Vec::with_capacity(MAX_LINE_BYTES);
                            loop {
                                scratch.clear();
                                let drained = (&mut reader)
                                    .take(MAX_LINE_BYTES as u64)
                                    .read_until(b'\n', &mut scratch)
                                    .await;
                                match drained {
                                    Ok(0) => break, // EOF mid-overline
                                    Ok(_) if scratch.ends_with(b"\n") => break,
                                    Ok(_) => continue, // still no newline; keep draining
                                    Err(_) => break,
                                }
                            }
                        }
                        let redacted = redact_log_line(&text);
                        if socket.send(Message::Text(redacted)).await.is_err() {
                            break; // peer disconnected
                        }
                    }
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
// Health + Diag (operability surface, outside the /api/v1/setup/* gate)
// ---------------------------------------------------------------------------

/// Liveness probe. Returns `200 OK` with `{"status": "ok", "version": ...}`
/// when the HTTP server is responsive. The lite agent at v0.1 has no live
/// FC heartbeat probe, so the body never reports `degraded`; future
/// phases that wire FC connectivity tracking can flip this to a 503 with
/// a `reasons` array.
///
/// Intentionally outside the same-origin gate so a monitoring agent on a
/// neighbouring host can hit the endpoint without forging an `Origin`
/// header. It is also free of any operator-controlled mutation surface.
pub async fn get_health() -> Json<Value> {
    Json(json!({
        "status": "ok",
        "version": env!("CARGO_PKG_VERSION"),
    }))
}

/// Diagnostic dump for deep operator debugging. Wider than `/health`:
/// surfaces uptime, runtime mode, identity, and best-effort live counters
/// for the cloud relay + MAVLink router. Fields the agent does not yet
/// track (`mavlink.frame_rate_recent`) appear as `null` so a future phase
/// can populate them without breaking consumers.
///
/// Never includes secrets — pair codes, API keys, and Cloudflare tokens
/// are deliberately omitted from this surface.
pub async fn get_diag(
    State(state): State<Arc<SetupState>>,
    Extension(diag): Extension<Arc<DiagState>>,
) -> Json<Value> {
    let yaml = read_diag_yaml(&state.agent_yaml);
    let paired = crate::pairing::PairingStore::new(&yaml.pairing_path)
        .load()
        .map(|p| p.is_paired())
        .unwrap_or(false);

    let now = now_unix_seconds();
    let cloud = diag.cloud_snapshot();

    Json(json!({
        "version": env!("CARGO_PKG_VERSION"),
        "uptime_seconds": diag.uptime_seconds(),
        "device_id": yaml.device_id,
        "paired": paired,
        "runtime_mode": "lite",
        "rss_mb": read_rss_mb(None),
        "mqtt": {
            "broker": yaml.mqtt_broker,
            "connected_recently": diag.mqtt_connected_recently(now),
        },
        "cloud_relay": {
            "convex_url": yaml.convex_url,
            "last_heartbeat_at": cloud.last_heartbeat_at,
            "consecutive_failures": cloud.consecutive_failures,
        },
        "mavlink": {
            "port": yaml.mavlink_port,
            "frame_rate_recent": Value::Null,
        },
    }))
}

/// Subset of agent.yaml fields the diag surface needs. Mirrors the
/// `read_yaml_view` helper in the agent binary but lives here so the
/// handler is self-contained and the binary does not have to thread a
/// closure through the SetupState.
struct DiagYaml {
    device_id: String,
    mqtt_broker: String,
    convex_url: String,
    mavlink_port: String,
    pairing_path: std::path::PathBuf,
}

fn read_diag_yaml(agent_yaml: &std::path::Path) -> DiagYaml {
    let pairing_path = agent_yaml
        .parent()
        .unwrap_or_else(|| std::path::Path::new("/etc/ados"))
        .join("pairing.json");
    let raw = match std::fs::read_to_string(agent_yaml) {
        Ok(s) => s,
        Err(_) => {
            return DiagYaml {
                device_id: String::new(),
                mqtt_broker: String::new(),
                convex_url: String::new(),
                mavlink_port: String::new(),
                pairing_path,
            };
        }
    };
    let doc: serde_yaml::Value = match serde_yaml::from_str(&raw) {
        Ok(v) => v,
        Err(_) => {
            return DiagYaml {
                device_id: String::new(),
                mqtt_broker: String::new(),
                convex_url: String::new(),
                mavlink_port: String::new(),
                pairing_path,
            };
        }
    };
    let s = |path: &[&str]| -> String {
        let mut cur = &doc;
        for k in path {
            match cur.get(k) {
                Some(v) => cur = v,
                None => return String::new(),
            }
        }
        cur.as_str().unwrap_or("").to_string()
    };
    DiagYaml {
        device_id: s(&["agent", "device_id"]),
        mqtt_broker: s(&["cloud", "mqtt_broker"]),
        convex_url: s(&["cloud", "convex_url"]),
        mavlink_port: s(&["mavlink", "port"]),
        pairing_path,
    }
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
