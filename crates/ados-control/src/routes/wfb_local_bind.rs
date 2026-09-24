//! The operator local-radio bind and the unpair.
//!
//! * `POST /api/wfb/pair/local-bind` opens a bind window through the supervisor
//!   control socket's `start_bind` op, which answers once the rendezvous ends.
//!   The rendezvous waits for a peer with no deadline of its own, so the route
//!   caps the wait at [`LOCAL_BIND_CAP`]; when the cap elapses it sends
//!   `cancel_bind` on a second connection and returns the terminal session the
//!   first connection then reports.
//! * `GET /api/wfb/pair/local-bind` is the latest session (`bind_status`), or `{}`
//!   when none has run or the socket is unreachable.
//! * `POST /api/wfb/pair/unpair` wipes both key files and the relay peer secret,
//!   clears the persisted pair fields, disarms auto-pair (so the rig does not
//!   silently re-bind) and restarts the role's radio unit.

use std::path::Path;
use std::time::Duration;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

use crate::config_store::{section, section_path, update_config};
use crate::routes::gs_mesh_write::supervisor_sock;
use crate::state::AppState;

/// How long one local-bind request waits for the rendezvous before cancelling
/// it, so a browser or proxy never cuts the request mid-flight.
const LOCAL_BIND_CAP: Duration = Duration::from_secs(300);

/// Connecting to the local socket is near-instant; a short cap keeps an absent
/// or refusing socket from stalling the request.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);

/// How long to wait for the terminal session after a cancel.
const POST_CANCEL_READ: Duration = Duration::from_secs(10);

/// Bound on the radio unit restart after an unpair.
const RESTART_TIMEOUT: Duration = Duration::from_secs(30);

fn nested_error(status: StatusCode, code: &str, message: impl Into<String>) -> Response {
    (
        status,
        Json(json!({"detail": {"error": {"code": code, "message": message.into()}}})),
    )
        .into_response()
}

// ---------------------------------------------------------------------------
// Local bind.
// ---------------------------------------------------------------------------

/// The `POST .../local-bind` body. `role` defaults to the node's own bind role.
#[derive(Debug, Default, Deserialize)]
pub struct LocalBindRequest {
    #[serde(default)]
    pub role: Option<String>,
    #[serde(default)]
    pub peer_device_id: Option<String>,
}

/// Why a start_bind produced no session.
#[derive(Debug, PartialEq)]
enum BindFailure {
    /// The control socket is not reachable; it is the sole producer of sessions.
    Unavailable,
    /// Another bind session is in flight.
    Busy,
    /// The socket reported, or the exchange hit, another failure.
    Failed(String),
}

/// `POST /api/wfb/pair/local-bind` → the bind session.
pub async fn post_local_bind(
    State(state): State<AppState>,
    Json(req): Json<LocalBindRequest>,
) -> Response {
    let role = match req.role.as_deref() {
        Some(r @ ("drone" | "gs")) => r,
        _ => crate::wfb_pair_state::bind_role(&state.pairing_paths),
    };
    let request = json!({
        "op": "start_bind",
        "role": role,
        "peer_device_id": req.peer_device_id,
        "source": "operator",
    });
    match start_bind(&supervisor_sock(), &request, LOCAL_BIND_CAP).await {
        Ok(session) => Json(session).into_response(),
        Err(BindFailure::Busy) => nested_error(
            StatusCode::CONFLICT,
            "E_BIND_IN_PROGRESS",
            "a bind session is already in progress",
        ),
        Err(BindFailure::Unavailable) => nested_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "E_BIND_UNAVAILABLE",
            "bind control socket unavailable",
        ),
        Err(BindFailure::Failed(message)) => {
            nested_error(StatusCode::INTERNAL_SERVER_ERROR, "E_BIND_FAILED", message)
        }
    }
}

/// Run one `start_bind` exchange, capped at `cap`. On the cap, cancel the
/// session on a second connection and return the terminal session the first
/// connection reports (or the latest status snapshot if it reports none).
async fn start_bind(sock: &Path, request: &Value, cap: Duration) -> Result<Value, BindFailure> {
    let stream = match tokio::time::timeout(CONNECT_TIMEOUT, UnixStream::connect(sock)).await {
        Ok(Ok(stream)) => stream,
        _ => {
            tracing::warn!(socket = %sock.display(), "bind control socket unreachable");
            return Err(BindFailure::Unavailable);
        }
    };
    let mut reader = BufReader::new(stream);
    let mut line = serde_json::to_vec(request).map_err(|e| BindFailure::Failed(e.to_string()))?;
    line.push(b'\n');
    reader
        .get_mut()
        .write_all(&line)
        .await
        .map_err(|e| BindFailure::Failed(e.to_string()))?;

    let mut reply = String::new();
    match tokio::time::timeout(cap, reader.read_line(&mut reply)).await {
        Ok(Ok(_)) => return parse_start_reply(&reply),
        Ok(Err(e)) => return Err(BindFailure::Failed(e.to_string())),
        Err(_) => {}
    }

    // The cap elapsed: abort the in-flight session, then collect the terminal
    // session the server answers the original connection with.
    let cancel =
        crate::ipc::cmd::roundtrip(sock, &json!({"op": "cancel_bind"}), CONNECT_TIMEOUT).await;
    if let Err(e) = cancel {
        tracing::debug!(?e, "bind cancel failed");
    }
    reply.clear();
    match tokio::time::timeout(POST_CANCEL_READ, reader.read_line(&mut reply)).await {
        Ok(Ok(_)) => match parse_start_reply(&reply) {
            Ok(session) => Ok(session),
            Err(_) => Ok(bind_status(sock).await),
        },
        _ => Ok(bind_status(sock).await),
    }
}

/// Parse a `start_bind` reply line into the session object.
fn parse_start_reply(line: &str) -> Result<Value, BindFailure> {
    if line.trim().is_empty() {
        return Err(BindFailure::Failed(
            "control socket closed connection before replying".to_string(),
        ));
    }
    let reply: Value =
        serde_json::from_str(line.trim()).map_err(|e| BindFailure::Failed(e.to_string()))?;
    if reply.get("ok") == Some(&Value::Bool(false)) {
        let error = reply
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("unknown bind error");
        return Err(if error == "E_BIND_IN_PROGRESS" {
            BindFailure::Busy
        } else {
            BindFailure::Failed(error.to_string())
        });
    }
    Ok(session_of(&reply))
}

/// A reply's `session`, or `{}` when it carries none.
fn session_of(reply: &Value) -> Value {
    match reply.get("session") {
        Some(Value::Object(s)) if !s.is_empty() => Value::Object(s.clone()),
        _ => json!({}),
    }
}

/// The latest bind session, or `{}` when none ran or the socket is unreachable.
async fn bind_status(sock: &Path) -> Value {
    match crate::ipc::cmd::roundtrip(sock, &json!({"op": "bind_status"}), CONNECT_TIMEOUT).await {
        Ok(reply) => session_of(&reply),
        Err(_) => json!({}),
    }
}

/// `GET /api/wfb/pair/local-bind` → the latest bind session, or `{}`.
pub async fn get_local_bind() -> Json<Value> {
    Json(bind_status(&supervisor_sock()).await)
}

// ---------------------------------------------------------------------------
// Unpair.
// ---------------------------------------------------------------------------

/// `POST /api/wfb/pair/unpair` → `{paired: false, role}`.
pub async fn post_unpair(State(state): State<AppState>) -> Response {
    let paths = state.pairing_paths.clone();
    let role = crate::wfb_pair_state::bind_role(&paths);
    let cleared = tokio::task::spawn_blocking(move || {
        unpair_at(&paths.config, &paths.wfb_key_dir, &paths.relay_secret, role)
    })
    .await;
    match cleared {
        Ok(Ok(())) => {}
        Ok(Err(e)) => return nested_error(StatusCode::INTERNAL_SERVER_ERROR, "E_UNPAIR_FAILED", e),
        Err(e) => {
            return nested_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "E_UNPAIR_FAILED",
                e.to_string(),
            )
        }
    }
    let unit = if role == "drone" {
        "ados-wfb.service"
    } else {
        "ados-wfb-rx.service"
    };
    if !crate::probe::capture_systemctl(&["restart", unit], RESTART_TIMEOUT)
        .await
        .is_ok()
    {
        tracing::warn!(unit, "radio unit restart after unpair failed");
    }
    tracing::warn!(role, "unpair_complete");
    Json(json!({"paired": false, "role": role})).into_response()
}

/// Remove the key material and clear the persisted pair fields. Both key files
/// go whatever the role: a stale key of the other role is never used but still
/// leaks key material. A key that cannot be removed fails the unpair rather than
/// reporting a rig unpaired while it still holds its key.
fn unpair_at(
    config_path: &Path,
    key_dir: &Path,
    relay_secret: &Path,
    role: &str,
) -> Result<(), String> {
    for path in [
        key_dir.join("tx.key"),
        key_dir.join("rx.key"),
        relay_secret.to_path_buf(),
    ] {
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(format!("{}: {e}", path.display())),
        }
    }
    update_config(config_path, |root| {
        use serde_norway::Value as Yaml;
        let wfb = section_path(root, &["video", "wfb"]);
        wfb.remove("paired_with_device_id");
        wfb.remove("paired_at");
        wfb.insert(
            Yaml::String("auto_pair_enabled".to_string()),
            Yaml::Bool(false),
        );
        if role == "gs" {
            let gs = section(root, "ground_station");
            gs.remove("paired_drone_id");
            gs.remove("paired_at");
        }
        Ok(())
    })
    .map(|_| ())
    .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::UnixListener;

    /// A one-connection-at-a-time fake supervisor: answers each request line
    /// with the reply `answer` computes, after `delay` for `start_bind`.
    fn fake_supervisor(
        dir: &Path,
        delay: Duration,
        answer: fn(&Value) -> Option<Value>,
    ) -> std::path::PathBuf {
        let path = dir.join("supervisor.sock");
        let listener = UnixListener::bind(&path).unwrap();
        tokio::spawn(async move {
            let cancelled = std::sync::Arc::new(tokio::sync::Notify::new());
            loop {
                let Ok((conn, _)) = listener.accept().await else {
                    return;
                };
                let cancelled = cancelled.clone();
                tokio::spawn(async move {
                    let mut conn = BufReader::new(conn);
                    let mut line = String::new();
                    if conn.read_line(&mut line).await.unwrap_or(0) == 0 {
                        return;
                    }
                    let req: Value = serde_json::from_str(line.trim()).unwrap();
                    if req["op"] == "cancel_bind" {
                        cancelled.notify_waiters();
                    }
                    if req["op"] == "start_bind" {
                        tokio::select! {
                            _ = tokio::time::sleep(delay) => {}
                            _ = cancelled.notified() => {}
                        }
                    }
                    if let Some(reply) = answer(&req) {
                        let mut out = serde_json::to_vec(&reply).unwrap();
                        out.push(b'\n');
                        let _ = conn.get_mut().write_all(&out).await;
                    }
                });
            }
        });
        path
    }

    fn session_reply(req: &Value) -> Option<Value> {
        Some(match req["op"].as_str() {
            Some("start_bind") => {
                json!({"ok": true, "session": {"state": "done", "role": req["role"]}})
            }
            Some("bind_status") => json!({"ok": true, "session": {"state": "aborted"}}),
            _ => json!({"ok": true}),
        })
    }

    #[tokio::test]
    async fn a_completed_bind_returns_its_session() {
        let dir = tempfile::tempdir().unwrap();
        let sock = fake_supervisor(dir.path(), Duration::ZERO, session_reply);
        let session = start_bind(
            &sock,
            &json!({"op": "start_bind", "role": "gs"}),
            LOCAL_BIND_CAP,
        )
        .await
        .unwrap();
        assert_eq!(session, json!({"state": "done", "role": "gs"}));
    }

    #[tokio::test]
    async fn the_cap_cancels_the_session_and_returns_its_terminal_state() {
        let dir = tempfile::tempdir().unwrap();
        let sock = fake_supervisor(dir.path(), Duration::from_secs(60), session_reply);
        let session = start_bind(
            &sock,
            &json!({"op": "start_bind", "role": "drone"}),
            Duration::from_millis(200),
        )
        .await
        .unwrap();
        // The cancel released the rendezvous, which then answered the original
        // connection with its terminal session.
        assert_eq!(session["role"], json!("drone"));
    }

    #[tokio::test]
    async fn a_busy_supervisor_is_a_busy_failure_and_an_absent_one_unavailable() {
        let dir = tempfile::tempdir().unwrap();
        let sock = fake_supervisor(dir.path(), Duration::ZERO, |_| {
            Some(json!({"ok": false, "error": "E_BIND_IN_PROGRESS"}))
        });
        assert_eq!(
            start_bind(&sock, &json!({"op": "start_bind"}), LOCAL_BIND_CAP).await,
            Err(BindFailure::Busy)
        );
        let absent = dir.path().join("absent.sock");
        assert_eq!(
            start_bind(&absent, &json!({"op": "start_bind"}), LOCAL_BIND_CAP).await,
            Err(BindFailure::Unavailable)
        );
        assert_eq!(bind_status(&absent).await, json!({}));
    }

    #[test]
    fn unpair_removes_the_keys_and_disarms_auto_pair() {
        let dir = tempfile::tempdir().unwrap();
        let keys = dir.path().join("wfb");
        std::fs::create_dir_all(&keys).unwrap();
        std::fs::write(keys.join("tx.key"), [1u8; 64]).unwrap();
        std::fs::write(keys.join("rx.key"), [2u8; 64]).unwrap();
        let secret = dir.path().join("relay-peer-secret");
        std::fs::write(&secret, b"s").unwrap();
        let cfg = dir.path().join("config.yaml");
        std::fs::write(
            &cfg,
            "agent:\n  name: gs\nvideo:\n  wfb:\n    channel: 149\n    paired_with_device_id: d1\n    paired_at: '2026-01-01'\n    auto_pair_enabled: true\nground_station:\n  paired_drone_id: d1\n  paired_at: '2026-01-01'\n",
        )
        .unwrap();

        unpair_at(&cfg, &keys, &secret, "gs").unwrap();

        assert!(!keys.join("tx.key").exists() && !keys.join("rx.key").exists());
        assert!(!secret.exists());
        let doc: serde_norway::Value =
            serde_norway::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
        let wfb = &doc["video"]["wfb"];
        assert!(wfb.get("paired_with_device_id").is_none());
        assert!(wfb.get("paired_at").is_none());
        assert_eq!(wfb["auto_pair_enabled"], serde_norway::Value::Bool(false));
        assert_eq!(wfb["channel"].as_i64(), Some(149));
        assert!(doc["ground_station"].get("paired_drone_id").is_none());
        assert_eq!(doc["agent"]["name"].as_str(), Some("gs"));
    }
}
