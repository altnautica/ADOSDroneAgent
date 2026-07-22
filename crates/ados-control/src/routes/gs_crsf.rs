//! Ground-station CRSF RC-lane routes.
//!
//! - **`GET /api/v1/ground-station/crsf`** — the RC lane's state sidecar
//!   (`crsf-stats.json`), staleness-gated: the lane daemon rewrites it ~1 Hz
//!   while running and every ~10 s while idling, so a file older than the
//!   window is an orphan of a dead service and reads `404`, never a stale
//!   reading served as current.
//! - **`POST /api/v1/ground-station/crsf/channels`** — programmatic channel
//!   injection, forwarded as a `set_channels` op to the lane daemon's command
//!   socket at `crsf-cmd.sock`. The body carries the 16 channel values plus
//!   an optional time-to-live and client id; the daemon validates the values,
//!   applies its TTL discipline, and replies with the live authority.
//! - **`POST /api/v1/ground-station/crsf/params`** — an RC-module
//!   configuration parameter write (the packet-rate / TX-power / telemetry
//!   surface), forwarded as a `param_write` op; the daemon frames it and
//!   queues it on the transmit lane.
//!
//! ## Why the writes forward to the lane's command socket
//!
//! The running `ados-crsf` daemon owns the live channel merge (authority,
//! TTL expiry, the transmit tick) — a write that only touched a file would
//! never reach the transmitted frames. This is the same command-socket
//! forward the sibling gamepad/network/radio write routes use; the socket
//! and its ops are owned by the lane daemon.
//!
//! ## Error posture
//!
//! Profile gate first: these are ground-station routes, `404
//! E_PROFILE_MISMATCH` elsewhere. An unreachable command socket is a `503`
//! (the lane is not running — it is opt-in and only serves its socket while
//! the serial module is up). A daemon-side rejection (`ok:false`, e.g. an
//! out-of-range channel value) is the caller's error: `400` carrying the
//! daemon's `E_*` code.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::routes::detail;
use crate::state::AppState;

/// The lane daemon's state sidecar filename under the run dir.
const SIDECAR_FILE: &str = "crsf-stats.json";

/// The lane daemon's command socket filename under the run dir.
const CMD_SOCK_FILE: &str = "crsf-cmd.sock";

/// How stale the sidecar may be before the route treats it as absent. The
/// daemon rewrites it ~1 Hz while running and every ~10 s while idling
/// disabled; beyond this window it is no longer reporting.
const STALE_AFTER: Duration = Duration::from_secs(30);

/// The runtime dir (`ADOS_RUN_DIR`, default `/run/ados`), the same override
/// the sibling sockets + sidecars resolve under.
fn run_dir() -> PathBuf {
    PathBuf::from(std::env::var("ADOS_RUN_DIR").unwrap_or_else(|_| "/run/ados".to_string()))
}

fn sidecar_path() -> PathBuf {
    run_dir().join(SIDECAR_FILE)
}

fn crsf_cmd_sock() -> PathBuf {
    run_dir().join(CMD_SOCK_FILE)
}

// ---------------------------------------------------------------------------
// Profile gate (mirrors the sibling ground-station routes).
// ---------------------------------------------------------------------------

/// True when the node resolves to the ground-station profile.
fn is_ground_station(state: &AppState) -> bool {
    let cfg = crate::config::PairingConfig::load_from(&state.pairing_paths.config);
    let (profile, _role) = crate::profile::current_profile_and_role(&cfg.agent.profile);
    profile == "ground-station"
}

/// The `404` profile-mismatch response, byte-identical to the sibling gates.
fn profile_mismatch() -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(json!({"detail": {"error": {"code": "E_PROFILE_MISMATCH"}}})),
    )
        .into_response()
}

// ---------------------------------------------------------------------------
// GET /api/v1/ground-station/crsf
// ---------------------------------------------------------------------------

/// `GET /api/v1/ground-station/crsf` → the lane's latest state sidecar.
pub async fn get_crsf_status(State(state): State<AppState>) -> Response {
    if !is_ground_station(&state) {
        return profile_mismatch();
    }
    read_status(&sidecar_path(), SystemTime::now())
}

/// The read logic against an explicit path + a reference "now", so a test can
/// point it at a temp file and drive the staleness check deterministically.
/// Absent / stale / unreadable / malformed all read `404` — never a `500`,
/// and never a stale body served as current.
fn read_status(path: &Path, now: SystemTime) -> Response {
    let Ok(meta) = std::fs::metadata(path) else {
        return status_not_found();
    };
    if let Ok(modified) = meta.modified() {
        if let Ok(age) = now.duration_since(modified) {
            if age > STALE_AFTER {
                return status_not_found();
            }
        }
    }
    let Ok(text) = std::fs::read_to_string(path) else {
        return status_not_found();
    };
    let Ok(doc) = serde_json::from_str::<Value>(&text) else {
        return status_not_found();
    };
    if !doc.is_object() {
        return status_not_found();
    }
    // Best-effort drift signal: warn (never reject) on a producer/reader
    // version mismatch, then serve the sidecar anyway. The contract registry
    // is the source of truth for the expected version.
    let version = doc.get("v").and_then(Value::as_u64).unwrap_or(0) as u16;
    let expected = ados_protocol::contracts::sidecar_version("crsf-stats").unwrap_or(0);
    ados_protocol::sidecar::check_sidecar_version("crsf-stats", version, expected);
    (StatusCode::OK, Json(doc)).into_response()
}

fn status_not_found() -> Response {
    detail(
        StatusCode::NOT_FOUND,
        "no CRSF lane status (the RC lane is not running on this node)".to_string(),
    )
}

// ---------------------------------------------------------------------------
// The command-socket round-trip.
// ---------------------------------------------------------------------------

/// One newline-JSON round-trip to the lane daemon's command socket. `None`
/// only on a TRANSPORT failure (unreachable socket / unparseable reply) — a
/// daemon-side `ok:false` rejection comes back as `Some(reply)` so the route
/// can report the daemon's error code to the caller as a `400`, not a `503`.
async fn crsf_cmd(socket: &Path, request: &Value) -> Option<Value> {
    /// A lane reply is a few hundred bytes; bound the read.
    const MAX_REPLY_BYTES: usize = 64 * 1024;

    let mut stream = tokio::net::UnixStream::connect(socket).await.ok()?;
    let line = format!("{}\n", serde_json::to_string(request).ok()?);
    stream.write_all(line.as_bytes()).await.ok()?;
    stream.flush().await.ok()?;

    let mut raw = Vec::new();
    let mut buf = [0u8; 8 * 1024];
    loop {
        let n = stream.read(&mut buf).await.ok()?;
        if n == 0 {
            break; // EOF: the server replies once then closes.
        }
        if raw.len() + n > MAX_REPLY_BYTES {
            return None;
        }
        raw.extend_from_slice(&buf[..n]);
        if raw.contains(&b'\n') {
            break;
        }
    }
    let text = String::from_utf8(raw).ok()?;
    serde_json::from_str(text.lines().next()?).ok()
}

/// Map a command round-trip outcome onto the HTTP reply: transport failure →
/// `503` (the lane is not running), a daemon `ok:false` → `400` with the
/// daemon's error code, success → the daemon's reply verbatim.
fn command_response(outcome: Option<Value>) -> Response {
    match outcome {
        None => detail(
            StatusCode::SERVICE_UNAVAILABLE,
            "crsf command socket unavailable (the RC lane is not running)".to_string(),
        ),
        Some(reply) if reply.get("ok") == Some(&Value::Bool(false)) => {
            let code = reply
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("E_CRSF_COMMAND_FAILED")
                .to_string();
            detail(StatusCode::BAD_REQUEST, code)
        }
        Some(reply) => (StatusCode::OK, Json(reply)).into_response(),
    }
}

// ---------------------------------------------------------------------------
// POST /api/v1/ground-station/crsf/channels
// ---------------------------------------------------------------------------

/// The channel-injection request body. The daemon validates the values
/// (16 channels, each 172..=1811) and clamps the TTL; `client_id` names the
/// injector for the hybrid PIC-holder authority decision.
#[derive(Debug, Deserialize)]
pub struct CrsfChannelsBody {
    pub channels: Vec<u16>,
    #[serde(default)]
    pub ttl_ms: Option<u64>,
    #[serde(default)]
    pub client_id: Option<String>,
}

/// `POST /api/v1/ground-station/crsf/channels` → inject the transmitted
/// channel set (with its TTL) through the lane daemon.
pub async fn post_crsf_channels(
    State(state): State<AppState>,
    Json(body): Json<CrsfChannelsBody>,
) -> Response {
    if !is_ground_station(&state) {
        return profile_mismatch();
    }
    let mut request = json!({"op": "set_channels", "channels": body.channels});
    if let Some(ttl_ms) = body.ttl_ms {
        request["ttl_ms"] = json!(ttl_ms);
    }
    if let Some(client_id) = body.client_id {
        request["client_id"] = json!(client_id);
    }
    command_response(crsf_cmd(&crsf_cmd_sock(), &request).await)
}

// ---------------------------------------------------------------------------
// POST /api/v1/ground-station/crsf/params
// ---------------------------------------------------------------------------

/// The parameter-write request body: the module's parameter field index plus
/// the raw value bytes (parameter-specific encoding, carried transparently).
#[derive(Debug, Deserialize)]
pub struct CrsfParamWriteBody {
    pub field_index: u8,
    #[serde(default)]
    pub data: Vec<u8>,
}

/// `POST /api/v1/ground-station/crsf/params` → queue an RC-module parameter
/// write on the transmit lane through the lane daemon.
pub async fn post_crsf_param_write(
    State(state): State<AppState>,
    Json(body): Json<CrsfParamWriteBody>,
) -> Response {
    if !is_ground_station(&state) {
        return profile_mismatch();
    }
    let request = json!({
        "op": "param_write",
        "field_index": body.field_index,
        "data": body.data,
    });
    command_response(crsf_cmd(&crsf_cmd_sock(), &request).await)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::UnixListener;

    async fn body_json(resp: Response) -> Value {
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    /// Stand up a one-shot command socket that captures the request and
    /// replies with `canned`.
    async fn fake_crsf_socket(
        dir: &Path,
        canned: Value,
    ) -> (PathBuf, tokio::sync::oneshot::Receiver<Value>) {
        let sock = dir.join("crsf-cmd.sock");
        let listener = UnixListener::bind(&sock).unwrap();
        let (tx, rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            if let Ok((mut stream, _)) = listener.accept().await {
                let mut buf = [0u8; 4096];
                let n = stream.read(&mut buf).await.unwrap_or(0);
                let line = String::from_utf8_lossy(&buf[..n]);
                if let Some(first) = line.lines().next() {
                    if let Ok(v) = serde_json::from_str::<Value>(first) {
                        let _ = tx.send(v);
                    }
                }
                let mut body = serde_json::to_vec(&canned).unwrap();
                body.push(b'\n');
                let _ = stream.write_all(&body).await;
                let _ = stream.flush().await;
            }
        });
        (sock, rx)
    }

    // ── the profile gate shape ───────────────────────────────────────────────

    #[tokio::test]
    async fn profile_mismatch_golden_body() {
        let resp = profile_mismatch();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        let body = body_json(resp).await;
        assert_eq!(
            body,
            json!({"detail": {"error": {"code": "E_PROFILE_MISMATCH"}}})
        );
    }

    // ── the staleness-gated status read ──────────────────────────────────────

    #[tokio::test]
    async fn serves_a_fresh_sidecar_verbatim() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("crsf-stats.json");
        let body = json!({
            "v": 1,
            "state": "link_ok",
            "rssi_dbm": -51,
            "lq_uplink": 99,
            "rf_unverified": false,
            "flyable": true,
            "channel_source": "inject",
        });
        std::fs::write(&path, serde_json::to_string(&body).unwrap()).unwrap();
        let resp = read_status(&path, SystemTime::now());
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(body_json(resp).await, body);
    }

    #[tokio::test]
    async fn an_absent_sidecar_is_a_404() {
        let dir = tempfile::tempdir().unwrap();
        let resp = read_status(&dir.path().join("nope.json"), SystemTime::now());
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn a_stale_sidecar_is_a_404_not_a_stale_reading() {
        // The lane daemon died and left the file behind: past the window the
        // route must refuse to serve the orphan as a current reading.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("crsf-stats.json");
        std::fs::write(&path, r#"{"v":1,"state":"link_ok","flyable":true}"#).unwrap();
        let future = SystemTime::now() + STALE_AFTER + Duration::from_secs(5);
        let resp = read_status(&path, future);
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn a_malformed_sidecar_is_a_404_not_a_500() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("crsf-stats.json");
        std::fs::write(&path, b"not json {{{").unwrap();
        let resp = read_status(&path, SystemTime::now());
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    // ── the command round-trip + response mapping ────────────────────────────

    #[tokio::test]
    async fn crsf_cmd_forwards_the_request_and_reads_the_reply() {
        let dir = tempfile::tempdir().unwrap();
        let (sock, seen) = fake_crsf_socket(
            dir.path(),
            json!({"ok": true, "channels": [992], "authority": "inject"}),
        )
        .await;
        let request = json!({"op": "set_channels", "channels": [992], "ttl_ms": 500});
        let reply = crsf_cmd(&sock, &request).await.unwrap();
        assert_eq!(reply["ok"], true);
        assert_eq!(reply["authority"], "inject");
        // The daemon saw the request verbatim.
        let seen = seen.await.unwrap();
        assert_eq!(seen, request);
    }

    #[tokio::test]
    async fn crsf_cmd_returns_the_daemon_rejection_for_the_400_map() {
        let dir = tempfile::tempdir().unwrap();
        let (sock, _seen) = fake_crsf_socket(
            dir.path(),
            json!({"ok": false, "error": "E_BAD_CHANNEL_VALUE: 2000"}),
        )
        .await;
        let outcome = crsf_cmd(&sock, &json!({"op": "set_channels"})).await;
        // The rejection is Some(reply), NOT a transport failure…
        assert!(outcome.is_some());
        // …and maps to a 400 carrying the daemon's code.
        let resp = command_response(outcome);
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let body = body_json(resp).await;
        assert_eq!(body["detail"], "E_BAD_CHANNEL_VALUE: 2000");
    }

    #[tokio::test]
    async fn an_unreachable_socket_maps_to_a_503() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("crsf-cmd.sock");
        let outcome = crsf_cmd(&sock, &json!({"op": "status"})).await;
        assert!(outcome.is_none());
        let resp = command_response(outcome);
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = body_json(resp).await;
        assert!(body["detail"].as_str().unwrap().contains("not running"));
    }

    #[tokio::test]
    async fn a_successful_reply_passes_through_verbatim() {
        let reply = json!({"ok": true, "field_index": 3, "queued": true});
        let resp = command_response(Some(reply.clone()));
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(body_json(resp).await, reply);
    }
}
