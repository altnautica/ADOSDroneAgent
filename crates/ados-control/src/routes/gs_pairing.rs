//! Ground-station mesh-pairing routes.
//!
//! The pairing snapshot read and the Accept-window writes the ground-station GCS
//! surface drives. Every one gates on the node's RESOLVED profile being a ground
//! station; a drone-profile node gets `404` with a stable error code so the GCS
//! can tell "wrong profile" from "endpoint missing":
//!
//! - **`GET /api/v1/ground-station/pair/pending`** — the mesh pairing snapshot
//!   (the open Accept window + the list of pending relay join requests). The
//!   pairing state lives in `ados-mesh-pairing.service` (`ados-groundlink
//!   --pairing`), its only owner: the route reaches it over the pairing
//!   socket (`/run/ados/pairing.sock`) with a single-shot newline-JSON
//!   `snapshot` op and relays the result. A daemon that is unreachable yields
//!   `503` with `E_PAIR_DAEMON_UNAVAILABLE`.
//! - **`GET /api/v1/ground-station/pic`** — served by [`crate::routes::gs_pic`],
//!   which owns the PIC control socket. It is NOT in this module: it used to be,
//!   answering from a hardcoded unclaimed body.
//!
//! Every read is fault-tolerant: an absent daemon socket / config degrades to a
//! stable status + body, never a `500`/panic.
//!
//! Error bodies use the ground-station routes' nested
//! `{"detail": {"error": {"code": ...}}}` shape (NOT the flat `{"detail": "..."}`
//! the other native routes use); the GCS parses it for the stable error code.

use std::path::{Path, PathBuf};

use ados_config::{mesh_role_path, profile_conf_path};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};

use crate::config::PairingConfig;
use crate::profile::current_profile_and_role_at;

/// The pairing service's Unix socket basename under the runtime dir.
const PAIRING_SOCK_NAME: &str = "pairing.sock";

// ---------------------------------------------------------------------------
// Path / flag seams (env-resolved at request time, injectable in tests).
// ---------------------------------------------------------------------------

/// The agent config path the profile gate reads (`ADOS_CONFIG`, default
/// `/etc/ados/config.yaml`). The same override the pairing-info route honours.
fn config_path() -> PathBuf {
    PathBuf::from(
        std::env::var("ADOS_CONFIG").unwrap_or_else(|_| crate::config::CONFIG_YAML.to_string()),
    )
}

/// The pairing daemon socket under the runtime dir (`ADOS_RUN_DIR`, default
/// `/run/ados`), resolved per request so a unit override is honoured.
fn pairing_socket() -> PathBuf {
    ados_protocol::paths::run_dir().join(PAIRING_SOCK_NAME)
}

/// `true` when the node's RESOLVED profile (read from `config_path` + the on-disk
/// profile/role sentinels) is a ground station: an explicit config value wins,
/// `"auto"`/empty falls back to `/etc/ados/profile.conf`.
fn is_ground_station() -> bool {
    is_ground_station_at(&config_path(), &profile_conf_path(), &mesh_role_path())
}

/// The path-injectable core of the profile gate: resolve the wire profile off
/// explicit config + profile.conf + role-sentinel paths and return whether it is a
/// ground station. Every path is threaded in, so a test drives it against a
/// tempdir without mutating the process environment.
fn is_ground_station_at(config: &Path, profile_conf: &Path, role_path: &Path) -> bool {
    let cfg = PairingConfig::load_from(config);
    let (profile, _role) = current_profile_and_role_at(&cfg.agent.profile, profile_conf, role_path);
    profile == "ground-station"
}

// ---------------------------------------------------------------------------
// Error helpers.
// ---------------------------------------------------------------------------

/// Build a ground-station error response: `(status, {"detail": {"error":
/// {"code": code}}})`. The nested ground-station error shape
/// (NOT the flat `{"detail": "..."}` the other native routes use).
fn gs_error(status: StatusCode, code: &str) -> Response {
    let body = json!({ "detail": { "error": { "code": code } } });
    (status, Json(body)).into_response()
}

/// The shared `404 E_PROFILE_MISMATCH` a drone-profile node gets on every
/// ground-station route. Matches `_require_ground_profile`.
fn profile_mismatch() -> Response {
    gs_error(StatusCode::NOT_FOUND, "E_PROFILE_MISMATCH")
}

// ---------------------------------------------------------------------------
// GET /api/v1/ground-station/pair/pending
// ---------------------------------------------------------------------------

/// The mesh pairing snapshot. Ground-station only; a drone-profile node gets
/// `404 E_PROFILE_MISMATCH`.
///
/// The snapshot lives in the pairing daemon: a single-shot newline-JSON `snapshot` op over `/run/ados/pairing.sock` returns
/// the `{"open": ...}` (+ window/pending/approvals when a window is open) body,
/// relayed verbatim. A daemon that is unreachable yields `503
/// E_PAIR_DAEMON_UNAVAILABLE`, never a fabricated `{"open": false}`.
pub async fn get_pair_pending() -> Response {
    if !is_ground_station() {
        return profile_mismatch();
    }
    pair_pending_body(&pairing_socket()).await
}

/// The pair-pending body, with the socket path injected so a test drives every
/// branch without mutating the process environment. The profile gate has already
/// passed when this is called.
async fn pair_pending_body(socket: &Path) -> Response {
    match pairing_daemon_snapshot(socket).await {
        Ok(result) => (StatusCode::OK, Json(result)).into_response(),
        Err(_) => gs_error(StatusCode::SERVICE_UNAVAILABLE, "E_PAIR_DAEMON_UNAVAILABLE"),
    }
}

/// Round-trip the pairing daemon's `snapshot` op over its Unix socket. Sends one
/// newline-terminated JSON request `{"op":"snapshot","args":{}}`, reads one
/// newline-terminated JSON reply `{"ok":bool,"result":{...}}`, and returns the
/// `result` object. Any connect / IO / parse failure, or a reply with
/// `ok != true`, is an error — the caller maps that to `503`.
async fn pairing_daemon_snapshot(socket: &Path) -> Result<Value, crate::ipc::cmd::CmdFailure> {
    let reply = crate::ipc::cmd::roundtrip(
        socket,
        &json!({"op": "snapshot", "args": {}}),
        crate::ipc::cmd::QUICK,
    )
    .await?;
    if reply.get("ok").and_then(Value::as_bool) != Some(true) {
        return Err(crate::ipc::cmd::CmdFailure::BadReply);
    }
    // A missing / non-object result is the empty object.
    Ok(reply
        .get("result")
        .filter(|v| v.is_object())
        .cloned()
        .unwrap_or_else(|| json!({})))
}

// ---------------------------------------------------------------------------
// GET /api/v1/ground-station/pic lives in `gs_pic`, which owns the PIC control
// socket. It used to be served here from a hardcoded `unclaimed` body; see that
// module for why that was wrong.
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// POST /api/v1/ground-station/pair/{accept,close,approve/{id},revoke/{id}}
// ---------------------------------------------------------------------------
//
// The Accept-window writes. Each forwards one op to the pairing daemon, the
// single owner of pairing state, and is receiver-only: any other mesh role gets
// `409 E_WRONG_ROLE`. A daemon that cannot be reached, or that refuses the op, is
// `503 E_PAIR_DAEMON_UNAVAILABLE` with its reason.

/// The `POST .../pair/accept` body: how long the window stays open, 5..=300 s.
#[derive(Debug, serde::Deserialize)]
pub struct PairAcceptRequest {
    #[serde(default = "default_window_s")]
    pub duration_s: i64,
}

fn default_window_s() -> i64 {
    60
}

/// Why a pairing-daemon op produced no result: the daemon's own error string,
/// or the transport failure.
async fn pairing_rpc(socket: &Path, op: &str, args: Value) -> Result<Value, String> {
    let reply = crate::ipc::cmd::roundtrip(
        socket,
        &json!({"op": op, "args": args}),
        crate::ipc::cmd::QUICK,
    )
    .await
    .map_err(|e| format!("pairing daemon unreachable: {e:?}"))?;
    if reply.get("ok").and_then(Value::as_bool) != Some(true) {
        return Err(reply
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("unknown error")
            .to_string());
    }
    Ok(reply
        .get("result")
        .filter(|v| v.is_object())
        .cloned()
        .unwrap_or_else(|| json!({})))
}

fn daemon_unavailable(message: String) -> Response {
    let body =
        json!({"detail": {"error": {"code": "E_PAIR_DAEMON_UNAVAILABLE", "message": message}}});
    (StatusCode::SERVICE_UNAVAILABLE, Json(body)).into_response()
}

/// The profile + receiver-role gate every window write shares. `None` = pass.
fn receiver_gate(config: &Path, profile_conf: &Path, role_path: &Path) -> Option<Response> {
    let cfg = PairingConfig::load_from(config);
    let (profile, role) = current_profile_and_role_at(&cfg.agent.profile, profile_conf, role_path);
    if profile != "ground-station" {
        return Some(profile_mismatch());
    }
    (role.as_deref() != Some("receiver")).then(|| {
        let body = json!({"detail": {"error": {"code": "E_WRONG_ROLE", "required": "receiver"}}});
        (StatusCode::CONFLICT, Json(body)).into_response()
    })
}

fn receiver_gate_live() -> Option<Response> {
    receiver_gate(&config_path(), &profile_conf_path(), &mesh_role_path())
}

fn as_i64(v: Option<&Value>) -> i64 {
    v.and_then(|v| v.as_i64().or_else(|| v.as_f64().map(|f| f as i64)))
        .unwrap_or(0)
}

/// `POST .../pair/accept` → `{opened_at_ms, closes_at_ms, duration_s, code}`.
pub async fn post_pair_accept(Json(req): Json<PairAcceptRequest>) -> Response {
    if let Some(refused) = receiver_gate_live() {
        return refused;
    }
    pair_accept_at(&pairing_socket(), req.duration_s).await
}

async fn pair_accept_at(socket: &Path, duration_s: i64) -> Response {
    if !(5..=300).contains(&duration_s) {
        let body = json!({"detail": {"error": {"code": "E_INVALID_DURATION", "message": "duration_s must be between 5 and 300"}}});
        return (StatusCode::UNPROCESSABLE_ENTITY, Json(body)).into_response();
    }
    let result = match pairing_rpc(socket, "open_window", json!({"duration_s": duration_s})).await {
        Ok(r) => r,
        Err(e) => return daemon_unavailable(e),
    };
    // The window's code is what the joining relay must present; a reply without
    // one is not an open window.
    let Some(code) = result.get("code").filter(|c| !c.is_null()) else {
        return daemon_unavailable("the pairing daemon opened no window code".to_string());
    };
    let code = code
        .as_str()
        .map_or_else(|| code.to_string(), str::to_string);
    Json(json!({
        "opened_at_ms": as_i64(result.get("opened_at_ms")),
        "closes_at_ms": as_i64(result.get("closes_at_ms")),
        "duration_s": duration_s,
        "code": code,
    }))
    .into_response()
}

/// `POST .../pair/close` → `{closed}`; `false` when no window was open.
pub async fn post_pair_close() -> Response {
    if let Some(refused) = receiver_gate_live() {
        return refused;
    }
    match pairing_rpc(&pairing_socket(), "close_window", json!({})).await {
        Ok(r) => Json(json!({"closed": r.get("closed").and_then(Value::as_bool).unwrap_or(false)}))
            .into_response(),
        Err(e) => daemon_unavailable(e),
    }
}

/// `POST .../pair/approve/{device_id}` → the invite for a pending relay.
pub async fn post_pair_approve(
    axum::extract::Path(device_id): axum::extract::Path<String>,
) -> Response {
    if let Some(refused) = receiver_gate_live() {
        return refused;
    }
    pair_approve_at(&pairing_socket(), &device_id).await
}

async fn pair_approve_at(socket: &Path, device_id: &str) -> Response {
    let open = match pairing_rpc(socket, "is_window_open", json!({})).await {
        Ok(r) => r.get("open").and_then(Value::as_bool).unwrap_or(false),
        Err(e) => return daemon_unavailable(e),
    };
    if !open {
        return gs_error(StatusCode::GONE, "E_PAIR_WINDOW_EXPIRED");
    }
    match pairing_rpc(socket, "approve", json!({"device_id": device_id})).await {
        // An approval without an invite is one the relay cannot use: report the
        // daemon as failing rather than a success with an empty blob.
        Ok(r) => match r
            .get("invite_blob_hex")
            .and_then(Value::as_str)
            .filter(|hex| !hex.is_empty())
        {
            Some(invite) => Json(json!({
                "device_id": device_id,
                "invite_blob_hex": invite,
                "issued_at_ms": as_i64(r.get("issued_at_ms")),
                "expires_at_ms": as_i64(r.get("expires_at_ms")),
            }))
            .into_response(),
            None => daemon_unavailable("approval returned no invite".to_string()),
        },
        Err(e) if e.contains("not found") || e.contains("window closed") => {
            gs_error(StatusCode::NOT_FOUND, "E_PAIR_REQUEST_NOT_FOUND")
        }
        Err(e) if e.contains("mesh not initialized") => {
            gs_error(StatusCode::SERVICE_UNAVAILABLE, "E_MESH_NOT_INITIALIZED")
        }
        Err(e) => daemon_unavailable(e),
    }
}

/// `POST .../pair/revoke/{device_id}` → `{device_id, revoked}`.
pub async fn post_pair_revoke(
    axum::extract::Path(device_id): axum::extract::Path<String>,
) -> Response {
    if let Some(refused) = receiver_gate_live() {
        return refused;
    }
    match pairing_rpc(&pairing_socket(), "revoke", json!({"device_id": device_id})).await {
        Ok(r) => Json(json!({
            "device_id": device_id,
            "revoked": r.get("revoked").and_then(Value::as_bool).unwrap_or(false),
        }))
        .into_response(),
        Err(e) => daemon_unavailable(e),
    }
}

// ---------------------------------------------------------------------------
// POST /api/v1/ground-station/pair/join
// ---------------------------------------------------------------------------
//
// Relay side of the mesh pairing. The pairing daemon runs the exchange (ECDH
// over UDP, invite decrypt under the receiver's window code, mesh identity
// persisted), so a restart of any other unit cannot cut a join short. Refused on
// a receiver, which issues invites instead of requesting them.

/// How long a join may take: the daemon waits up to 45 s for the invite after
/// resolving the receiver, so the forward outlasts it.
const JOIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// The `POST .../pair/join` body.
#[derive(Debug, serde::Deserialize)]
pub struct PairJoinRequest {
    /// The six-digit code the receiver shows while its window is open.
    pub code: String,
    #[serde(default)]
    pub receiver_host: Option<String>,
    #[serde(default)]
    pub receiver_port: Option<u16>,
}

/// `POST .../pair/join` → `{mesh_id, receiver_host, ok}`.
pub async fn post_pair_join(Json(req): Json<PairJoinRequest>) -> Response {
    if let Some(refused) = join_gate(&config_path(), &profile_conf_path(), &mesh_role_path()) {
        return refused;
    }
    pair_join_at(&pairing_socket(), &req).await
}

/// The profile + non-receiver role gate. `None` = pass.
fn join_gate(config: &Path, profile_conf: &Path, role_path: &Path) -> Option<Response> {
    let cfg = PairingConfig::load_from(config);
    let (profile, role) = current_profile_and_role_at(&cfg.agent.profile, profile_conf, role_path);
    if profile != "ground-station" {
        return Some(profile_mismatch());
    }
    (role.as_deref() == Some("receiver")).then(|| {
        let body = json!({"detail": {"error": {
            "code": "E_WRONG_ROLE",
            "required": "direct_or_relay",
            "current": "receiver",
        }}});
        (StatusCode::CONFLICT, Json(body)).into_response()
    })
}

async fn pair_join_at(socket: &Path, req: &PairJoinRequest) -> Response {
    if req.code.len() != 6 || !req.code.bytes().all(|b| b.is_ascii_digit()) {
        let body = json!({"detail": {"error": {"code": "E_INVALID_CODE", "message": "code must be six digits"}}});
        return (StatusCode::UNPROCESSABLE_ENTITY, Json(body)).into_response();
    }
    let args = json!({
        "code": req.code,
        "receiver_host": req.receiver_host.as_deref().filter(|h| !h.is_empty()),
        "receiver_port": req.receiver_port.filter(|p| *p != 0),
    });
    let reply = match crate::ipc::cmd::roundtrip(
        socket,
        &json!({"op": "join", "args": args}),
        JOIN_TIMEOUT,
    )
    .await
    {
        Ok(reply) => reply,
        Err(e) => return daemon_unavailable(format!("pairing daemon unreachable: {e:?}")),
    };
    if reply.get("ok").and_then(Value::as_bool) != Some(true) {
        let text = |k: &str, fallback: &str| {
            reply
                .get(k)
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .unwrap_or(fallback)
                .to_string()
        };
        let body = json!({"detail": {"error": {
            "code": text("error_code", "E_JOIN_FAILED"),
            "message": text("error", "join failed"),
        }}});
        return (StatusCode::SERVICE_UNAVAILABLE, Json(body)).into_response();
    }
    let result = reply.get("result").cloned().unwrap_or_else(|| json!({}));
    Json(json!({
        "mesh_id": result.get("mesh_id").cloned().unwrap_or(Value::Null),
        "receiver_host": result.get("receiver_host").cloned().unwrap_or(Value::Null),
        "ok": true,
    }))
    .into_response()
}

#[cfg(test)]
mod tests;
