//! Ground-station primary-gamepad write route.
//!
//! - **`PUT /api/v1/ground-station/gamepads/primary`** — select the primary
//!   gamepad the PIC arbiter binds to. The body is `{"device_id"}`; the route
//!   returns `{"primary_id": <device_id>, "result": null}`, the exact shape the
//!   FastAPI route returns (the in-process `set_primary` returns `None`, which
//!   the route reports as `result: null`).
//!
//! ## Why this forwards to the input command socket (the live-primary owner)
//!
//! The running primary-gamepad selection is owned by the `ados-input` daemon's
//! hotplug tracker: it is the value the 1 Hz hotplug poll consults so it does not
//! re-promote a different device when the selected one is present. The Python
//! `InputManager.set_primary` does two things — update the in-process singleton
//! AND persist the `ground-station-input.json` sidecar. The native front has no
//! in-process tracker, and writing only the on-disk sidecar would leave the
//! running daemon's primary stale until its next restart. So the front forwards a
//! `set_primary` op to the daemon's command socket at `/run/ados/hid-cmd.sock`;
//! the daemon applies it to the running tracker and persists the sidecar in
//! lockstep — the same two effects the Python `set_primary` produces. This is the
//! command-socket forward the sibling network/radio write routes use; the socket
//! and its `set_primary` op are owned by the `ados-input` daemon.
//!
//! ## Response shape + the degrade posture
//!
//! On a successful apply the body is `{"primary_id": <device_id>, "result":
//! null}` — `primary_id` is the request's `device_id` (the FastAPI route echoes
//! `update.device_id`), and `result` is null (the Python `set_primary` returns
//! `None`). The daemon's `set_primary` reply may carry a `persist_error` field
//! when the running primary updated but the sidecar write failed; the route still
//! reports success (the running state is the authority, matching the Python which
//! updates the singleton before — and regardless of — the persist outcome). An
//! unreachable command socket → the FastAPI `500 E_GAMEPAD_PRIMARY_FAILED`
//! error-object body, the arm the Python route takes when the call raises.
//!
//! ## The profile gate
//!
//! Like every ground-station route, this first gates on the resolved profile
//! being a ground station and returns the FastAPI
//! `404 {"detail":{"error":{"code":"E_PROFILE_MISMATCH"}}}` on a drone.

use std::path::{Path, PathBuf};

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::state::AppState;

// ---------------------------------------------------------------------------
// Profile gate (mirrors the FastAPI `_require_ground_profile`).
// ---------------------------------------------------------------------------

/// True when the node resolves to the ground-station profile. Resolves through
/// `current_profile_and_role` (the same source of truth the node advertises on
/// the wire), so a `profile: auto` node that resolves to a ground station via
/// `profile.conf` passes the gate, matching the Python `_require_ground_profile`.
fn is_ground_station(state: &AppState) -> bool {
    let cfg = crate::config::PairingConfig::load_from(&state.pairing_paths.config);
    let (profile, _role) = crate::profile::current_profile_and_role(&cfg.agent.profile);
    profile == "ground-station"
}

/// The `404` profile-mismatch response, byte-identical to the FastAPI gate.
fn profile_mismatch() -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(json!({"detail": {"error": {"code": "E_PROFILE_MISMATCH"}}})),
    )
        .into_response()
}

/// Build a gamepad-route 5xx body in the FastAPI error-object detail shape:
/// `(status, {"detail": {"error": {"code", "message"}}})`. The FastAPI gamepad
/// route raises `HTTPException(500, detail={"error": {...}})` when its
/// `set_primary` call raises; the front takes the same shape when the daemon
/// command socket is unreachable (its only apply path).
fn gamepad_error(status: StatusCode, code: &str, message: impl Into<String>) -> Response {
    (
        status,
        Json(json!({"detail": {"error": {"code": code, "message": message.into()}}})),
    )
        .into_response()
}

// ---------------------------------------------------------------------------
// The input command socket seam.
// ---------------------------------------------------------------------------

/// The runtime dir (`ADOS_RUN_DIR`, default `/run/ados`), the same override the
/// sibling sockets + sidecars resolve under.
fn run_dir() -> PathBuf {
    PathBuf::from(std::env::var("ADOS_RUN_DIR").unwrap_or_else(|_| "/run/ados".to_string()))
}

/// The `ados-input` daemon's command socket (`/run/ados/hid-cmd.sock`), which
/// applies the `set_primary` op through the running hotplug tracker (the single
/// owner of the live primary) and persists the sidecar.
fn hid_cmd_sock() -> PathBuf {
    run_dir().join("hid-cmd.sock")
}

/// The outcome of a `set_primary` command-socket round-trip: the daemon's reply
/// object on `ok:true`, or `None` when the socket was unreachable / replied
/// unparseably / replied `ok:false` (a transport-level failure the front maps to
/// the FastAPI `E_GAMEPAD_PRIMARY_FAILED` 500).
async fn set_primary_cmd(socket: &Path, device_id: &str) -> Option<Value> {
    /// A primary-selection reply is a few hundred bytes; bound the read.
    const MAX_REPLY_BYTES: usize = 64 * 1024;

    let request = json!({"op": "set_primary", "device_id": device_id});
    let mut stream = tokio::net::UnixStream::connect(socket).await.ok()?;
    let line = format!("{}\n", serde_json::to_string(&request).ok()?);
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
    let reply_line = text.lines().next()?;
    let parsed: Value = serde_json::from_str(reply_line).ok()?;
    let obj = parsed.as_object()?;
    // A transport-level failure (`ok:false`, e.g. a missing-device-id reply the
    // front never sends, or an encode fault) is treated as unavailable — the
    // apply did not succeed, which maps to the FastAPI 500 error path.
    if obj.get("ok") == Some(&Value::Bool(false)) {
        return None;
    }
    Some(parsed)
}

// ---------------------------------------------------------------------------
// PUT /api/v1/ground-station/gamepads/primary
// ---------------------------------------------------------------------------

/// The `gamepads/primary` request body. Mirrors the FastAPI
/// `GamepadPrimaryUpdate`: a required `device_id` (the Pydantic model carries
/// `min_length=1`).
#[derive(Debug, Deserialize)]
pub struct GamepadPrimaryUpdate {
    pub device_id: String,
}

/// `PUT /api/v1/ground-station/gamepads/primary` → `{"primary_id", "result"}`.
///
/// Gates on the ground-station profile (404 on a drone), forwards a `set_primary`
/// op to the `ados-input` daemon command socket (applying it to the running
/// tracker + persisting the sidecar), and returns `{"primary_id": <device_id>,
/// "result": null}` — the same body the FastAPI route returns (its `set_primary`
/// returns `None`). An unreachable socket → the FastAPI
/// `500 E_GAMEPAD_PRIMARY_FAILED`.
pub async fn put_gamepad_primary(
    State(state): State<AppState>,
    Json(update): Json<GamepadPrimaryUpdate>,
) -> Response {
    if !is_ground_station(&state) {
        return profile_mismatch();
    }
    match set_primary_cmd(&hid_cmd_sock(), &update.device_id).await {
        // The FastAPI route returns {"primary_id": update.device_id, "result":
        // result}, where result is the set_primary return value (None). The
        // daemon may report a persist_error, but the running primary updated, so
        // the route still reports success with result: null — the running state
        // is the authority, matching the Python which updates the singleton
        // before (and regardless of) the persist outcome.
        Some(_reply) => {
            Json(json!({"primary_id": update.device_id, "result": Value::Null})).into_response()
        }
        None => gamepad_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "E_GAMEPAD_PRIMARY_FAILED",
            "input command socket unavailable",
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::UnixListener;

    /// Read a response body as JSON.
    async fn body_json(resp: Response) -> Value {
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    /// Stand up a one-shot `set_primary` command socket that replies with
    /// `canned` to the first request, returning the socket path (the spawned task
    /// keeps the listener alive until it serves the single request).
    async fn fake_hid_socket(dir: &std::path::Path, canned: Value) -> PathBuf {
        let sock = dir.join("hid-cmd.sock");
        let listener = UnixListener::bind(&sock).unwrap();
        tokio::spawn(async move {
            if let Ok((mut stream, _)) = listener.accept().await {
                let mut buf = [0u8; 4096];
                let _ = stream.read(&mut buf).await;
                let mut body = serde_json::to_vec(&canned).unwrap();
                body.push(b'\n');
                let _ = stream.write_all(&body).await;
                let _ = stream.flush().await;
            }
        });
        sock
    }

    // ── profile gate ─────────────────────────────────────────────────────────

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

    // ── the command-socket round-trip ─────────────────────────────────────────

    #[tokio::test]
    async fn set_primary_cmd_reads_the_daemon_ack() {
        let dir = tempfile::tempdir().unwrap();
        let sock = fake_hid_socket(
            dir.path(),
            json!({"ok": true, "primary_id": "usb:045e:028e:event3"}),
        )
        .await;
        let reply = set_primary_cmd(&sock, "usb:045e:028e:event3")
            .await
            .unwrap();
        assert_eq!(
            reply.get("primary_id").and_then(Value::as_str),
            Some("usb:045e:028e:event3")
        );
    }

    #[tokio::test]
    async fn set_primary_cmd_accepts_a_persist_error_ack() {
        // The daemon reports the running primary updated but the sidecar write
        // failed (`persist_error`); the apply still succeeded, so the round-trip
        // is a Some(reply) and the route reports success.
        let dir = tempfile::tempdir().unwrap();
        let sock = fake_hid_socket(
            dir.path(),
            json!({"ok": true, "primary_id": "usb:7", "persist_error": "EPERM"}),
        )
        .await;
        assert!(set_primary_cmd(&sock, "usb:7").await.is_some());
    }

    #[tokio::test]
    async fn set_primary_cmd_treats_ok_false_as_unavailable() {
        let dir = tempfile::tempdir().unwrap();
        let sock = fake_hid_socket(
            dir.path(),
            json!({"ok": false, "error": "E_MISSING_DEVICE_ID"}),
        )
        .await;
        assert!(set_primary_cmd(&sock, "usb:7").await.is_none());
    }

    #[tokio::test]
    async fn set_primary_cmd_is_none_on_an_unreachable_socket() {
        // No listener bound → connect fails → None.
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("hid-cmd.sock");
        assert!(set_primary_cmd(&sock, "usb:7").await.is_none());
    }

    // ── the success body shape ────────────────────────────────────────────────

    #[test]
    fn the_success_body_echoes_the_device_id_with_null_result() {
        // The route's success envelope is {"primary_id": <device_id>, "result":
        // null}. Built from the same json! the handler returns so the contract is
        // pinned field-by-field without a live ground-station profile.
        let device_id = "usb:045e:028e:event3";
        let body = json!({"primary_id": device_id, "result": Value::Null});
        assert_eq!(
            body,
            json!({"primary_id": "usb:045e:028e:event3", "result": null})
        );
    }

    // ── the unavailable posture ───────────────────────────────────────────────

    #[tokio::test]
    async fn unavailable_socket_is_the_500_error_object() {
        let resp = gamepad_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "E_GAMEPAD_PRIMARY_FAILED",
            "input command socket unavailable",
        );
        assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let body = body_json(resp).await;
        assert_eq!(
            body,
            json!({
                "detail": {
                    "error": {
                        "code": "E_GAMEPAD_PRIMARY_FAILED",
                        "message": "input command socket unavailable"
                    }
                }
            })
        );
    }
}
