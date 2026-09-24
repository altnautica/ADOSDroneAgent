//! Ground-station mesh-pairing + PIC + captive-token read routes.
//!
//! Three read-only routes the ground-station GCS surface polls. Every one gates
//! on the node's RESOLVED profile being a ground station; a drone-profile node
//! gets `404` with a stable error code so the GCS can tell "wrong profile" from
//! "endpoint missing":
//!
//! - **`GET /api/v1/ground-station/pair/pending`** — the mesh pairing snapshot
//!   (the open Accept window + the list of pending relay join requests). The
//!   pairing state lives in `ados-mesh-pairing.service`, its only owner: the
//!   route reaches it over the pairing daemon's Unix socket (`/run/ados/pairing.sock`) with a single-shot newline-
//!   JSON `snapshot` op and relays the result. A daemon that is unreachable yields
//!   `503` with `E_PAIR_DAEMON_UNAVAILABLE`, the same status the FastAPI route
//!   raises when its own socket round-trip fails.
//! - **`GET /api/v1/ground-station/pic`** — served by [`crate::routes::gs_pic`],
//!   which owns the PIC control socket. It is NOT in this module: it used to be,
//!   answering from a hardcoded unclaimed body.
//! - **`GET /api/v1/ground-station/captive-token`** — the captive-portal token
//!   mint for the setup webapp. The FastAPI handler gates on the request peer
//!   being on the AP hotspot subnet (`192.168.4.0/24`) or loopback, and otherwise
//!   raises `403 E_CAPTIVE_ONLY`. When this front owns the LAN port the residual
//!   FastAPI is bound to an internal Unix socket, where a request has no peer IP
//!   (`request.client` is `None`), so that AP-subnet/loopback gate can never pass
//!   and the residual handler raises `403 E_CAPTIVE_ONLY` for every caller. The
//!   real `192.168.4.x` hotspot client reaches the token mint through the setup
//!   webapp, which stays on the residual Python. So this front matches the
//!   residual's observable behavior exactly: a ground station always answers
//!   `403 E_CAPTIVE_ONLY` here, and never mints a token of its own.
//!
//! Every read is fault-tolerant: an absent daemon socket / config degrades to the
//! same status + body the FastAPI route returns when its own source is
//! unavailable, never a `500`/panic. The routes carry no path params and never
//! mutate, so they are safe to serve natively while the pair-window writes
//! (`/pair/accept`, `/pair/approve/{id}`, the `/pic/*` claim/release writes) and
//! the `/pic/events` websocket stay on the residual surface.
//!
//! Error bodies use the ground-station routes' nested
//! `{"detail": {"error": {"code": ...}}}` shape (NOT the flat `{"detail": "..."}`
//! the other native routes use), because the FastAPI ground-station handlers
//! raise that nested shape and the GCS parses it for the stable error code.

use std::path::{Path, PathBuf};

use ados_config::profile_conf_path;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};

use crate::config::PairingConfig;
use crate::profile::{current_profile_and_role_at, mesh_role_path};

/// The pairing daemon's Unix socket basename under the runtime dir. Mirrors the
/// Python `PAIRING_SOCK` (`ADOS_RUN_DIR / "pairing.sock"`).
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

/// The runtime dir (`ADOS_RUN_DIR`, default `/run/ados`), the same override the
/// sidecar-reading routes honour. The pairing daemon socket lives under it.
fn run_dir() -> PathBuf {
    PathBuf::from(std::env::var("ADOS_RUN_DIR").unwrap_or_else(|_| "/run/ados".to_string()))
}

/// `true` when the node's RESOLVED profile (read from `config_path` + the on-disk
/// profile/role sentinels) is a ground station. Mirrors the FastAPI
/// `is_ground_station` gate: an explicit config value wins, `"auto"`/empty falls
/// back to `/etc/ados/profile.conf`.
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
/// {"code": code}}})`. The nested shape the FastAPI ground-station handlers raise
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
    pair_pending_body(&run_dir().join(PAIRING_SOCK_NAME)).await
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
/// `ok != true`, is an error — the caller maps that to the `503` the FastAPI
/// route raises on a `PairingRpcError`. Mirrors the Python single-shot `_call`.
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
    // `result or {}` in the Python: a missing / non-object result is the empty
    // object.
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
// GET /api/v1/ground-station/captive-token
// ---------------------------------------------------------------------------

/// The captive-portal token mint. Ground-station only; a drone-profile node gets
/// `404 E_PROFILE_MISMATCH`, and a ground station always gets `403
/// E_CAPTIVE_ONLY`.
///
/// The FastAPI handler gates on the request peer being on the AP hotspot subnet
/// (`192.168.4.0/24`) or loopback (`_is_ap_subnet_client`), and raises `403
/// E_CAPTIVE_ONLY` for anything else. When this front owns the LAN port, the
/// residual FastAPI is bound to an internal Unix socket where a request carries
/// no peer IP (`request.client` is `None`), so that gate can never pass and the
/// residual handler raises `403 E_CAPTIVE_ONLY` for every caller. The real
/// `192.168.4.x` hotspot client reaches the token mint through the setup webapp,
/// which stays on the residual Python. So this front never mints a token itself:
/// after the profile gate it returns the same `403 E_CAPTIVE_ONLY` the residual
/// returns for every request reaching it over its Unix socket.
pub async fn get_captive_token() -> Response {
    captive_token_at(&config_path(), &profile_conf_path(), &mesh_role_path())
}

/// The path-injectable core of [`get_captive_token`]: the profile gate off explicit
/// config + sentinel paths, then the fixed `403 E_CAPTIVE_ONLY`. Threading the
/// paths lets a test drive the gate + response shape without touching the env.
fn captive_token_at(config: &Path, profile_conf: &Path, role_path: &Path) -> Response {
    if !is_ground_station_at(config, profile_conf, role_path) {
        return profile_mismatch();
    }

    // The residual FastAPI is reached over its internal Unix socket, where a
    // request has no peer IP, so its AP-subnet/loopback gate never passes and it
    // raises `403 E_CAPTIVE_ONLY` for every caller. Match that exactly.
    gs_error(StatusCode::FORBIDDEN, "E_CAPTIVE_ONLY")
}

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
    pair_accept_at(&run_dir().join(PAIRING_SOCK_NAME), req.duration_s).await
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
    match pairing_rpc(
        &run_dir().join(PAIRING_SOCK_NAME),
        "close_window",
        json!({}),
    )
    .await
    {
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
    pair_approve_at(&run_dir().join(PAIRING_SOCK_NAME), &device_id).await
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
        Ok(r) => Json(json!({
            "device_id": device_id,
            "invite_blob_hex": r.get("invite_blob_hex").and_then(Value::as_str).unwrap_or(""),
            "issued_at_ms": as_i64(r.get("issued_at_ms")),
            "expires_at_ms": as_i64(r.get("expires_at_ms")),
        }))
        .into_response(),
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
    match pairing_rpc(
        &run_dir().join(PAIRING_SOCK_NAME),
        "revoke",
        json!({"device_id": device_id}),
    )
    .await
    {
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
    pair_join_at(&run_dir().join(PAIRING_SOCK_NAME), &req).await
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
mod tests {
    use super::*;
    use axum::body::to_bytes;
    use std::io::Write;

    /// Write a config.yaml carrying an explicit `agent.profile` into a tempdir and
    /// return its path. An explicit value resolves straight to the wire profile
    /// without consulting profile.conf, so the gate is deterministic with no env.
    fn config_with_profile(dir: &Path, profile: &str) -> PathBuf {
        let cfg = dir.join("config.yaml");
        let mut f = std::fs::File::create(&cfg).unwrap();
        write!(f, "agent:\n  profile: {profile}\n").unwrap();
        cfg
    }

    async fn body_json(resp: Response) -> (StatusCode, Value) {
        let status = resp.status();
        let bytes = to_bytes(resp.into_body(), 1024 * 1024).await.unwrap();
        let value: Value = serde_json::from_slice(&bytes).unwrap();
        (status, value)
    }

    // -------------------------------------------------------------------
    // Profile gate: drone resolves to not-ground-station, ground_station
    // resolves to ground-station. Path-injectable, no env mutation.
    // -------------------------------------------------------------------

    #[test]
    fn drone_profile_is_not_a_ground_station() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = config_with_profile(dir.path(), "drone");
        assert!(!is_ground_station_at(
            &cfg,
            &dir.path().join("absent.conf"),
            &dir.path().join("absent.role"),
        ));
    }

    #[test]
    fn ground_station_profile_is_a_ground_station() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = config_with_profile(dir.path(), "ground_station");
        assert!(is_ground_station_at(
            &cfg,
            &dir.path().join("absent.conf"),
            &dir.path().join("absent.role"),
        ));
    }

    #[test]
    fn an_absent_config_is_not_a_ground_station() {
        // A missing config loads the all-defaults (`profile: auto`); with no
        // profile.conf, `auto` falls back to the drone default → not a GS.
        assert!(!is_ground_station_at(
            Path::new("/nonexistent/ados/config.yaml"),
            Path::new("/nonexistent/ados/profile.conf"),
            Path::new("/nonexistent/ados/mesh-role"),
        ));
    }

    // -------------------------------------------------------------------
    // Profile-mismatch error shape: the nested 404 body every route returns
    // for a drone-profile node.
    // -------------------------------------------------------------------

    #[tokio::test]
    async fn profile_mismatch_is_the_nested_404_body() {
        let (status, body) = body_json(profile_mismatch()).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(
            body,
            json!({ "detail": { "error": { "code": "E_PROFILE_MISMATCH" } } })
        );
    }

    // -------------------------------------------------------------------
    // Golden-fixture parity: the steady-state ground-station bodies.
    // -------------------------------------------------------------------

    #[tokio::test]
    async fn pair_pending_503s_when_the_daemon_socket_is_absent() {
        let dir = tempfile::tempdir().unwrap();
        // An absent daemon socket: the round-trip fails, so the route reports
        // the daemon-unavailable status rather than a closed window.
        let socket = dir.path().join("pairing.sock");
        let (status, body) = body_json(pair_pending_body(&socket).await).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            body,
            json!({ "detail": { "error": { "code": "E_PAIR_DAEMON_UNAVAILABLE" } } })
        );
    }

    #[tokio::test]
    async fn pair_pending_relays_a_daemon_snapshot_with_an_open_window() {
        // Stand up a one-shot daemon stub on a Unix socket that answers the
        // snapshot op with an open-window result, and assert the route relays the
        // inner `result` object verbatim.
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("pairing.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let server = tokio::spawn(async move {
            let (mut conn, _) = listener.accept().await.unwrap();
            // Read the request line (we don't need to parse it for the stub).
            let mut buf = [0u8; 256];
            let _ = conn.read(&mut buf).await.unwrap();
            let reply = "{\"ok\":true,\"result\":{\"open\":true,\"opened_at_ms\":100,\"closes_at_ms\":160,\"pending\":[],\"approvals\":{}}}\n";
            conn.write_all(reply.as_bytes()).await.unwrap();
            conn.flush().await.unwrap();
        });

        let (status, body) = body_json(pair_pending_body(&socket).await).await;
        server.await.unwrap();
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body,
            json!({
                "open": true,
                "opened_at_ms": 100,
                "closes_at_ms": 160,
                "pending": [],
                "approvals": {},
            })
        );
    }

    // -------------------------------------------------------------------
    // Captive-token: the residual FastAPI is reached over its internal Unix
    // socket, where a request has no peer IP, so its AP-subnet/loopback gate
    // never passes and it raises 403 E_CAPTIVE_ONLY for every caller. The native
    // handler must match that exactly: a ground station always answers 403, never
    // mints a token of its own.
    // -------------------------------------------------------------------

    /// Drive the captive-token core against a config that carries an explicit
    /// `agent.profile` (which resolves straight to the wire profile without
    /// consulting profile.conf). Every path is threaded into the core, so the test
    /// never mutates the process environment and cannot race a sibling test.
    async fn captive_token_with_profile(profile: &str) -> (StatusCode, Value) {
        let dir = tempfile::tempdir().unwrap();
        let cfg = config_with_profile(dir.path(), profile);
        let resp = captive_token_at(
            &cfg,
            &dir.path().join("absent.conf"),
            &dir.path().join("absent.role"),
        );
        body_json(resp).await
    }

    #[tokio::test]
    async fn captive_token_403s_on_a_ground_station() {
        // GOLDEN FIXTURE: a ground station always answers 403 E_CAPTIVE_ONLY here
        // (the residual Python's Unix-socket behavior — no peer IP, gate never
        // passes), with the nested ground-station error body.
        let (status, body) = captive_token_with_profile("ground_station").await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(
            body,
            json!({ "detail": { "error": { "code": "E_CAPTIVE_ONLY" } } })
        );
    }

    #[tokio::test]
    async fn captive_token_404s_on_a_drone_profile() {
        // The profile gate runs first: a drone-profile node gets the nested 404
        // E_PROFILE_MISMATCH every ground-station route returns, never the 403.
        let (status, body) = captive_token_with_profile("drone").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(
            body,
            json!({ "detail": { "error": { "code": "E_PROFILE_MISMATCH" } } })
        );
    }

    /// A fake pairing daemon answering each op with `answer(op)`.
    fn fake_daemon(dir: &Path, answer: fn(&str) -> Value) -> PathBuf {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        let path = dir.join("pairing.sock");
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        tokio::spawn(async move {
            while let Ok((conn, _)) = listener.accept().await {
                let mut conn = BufReader::new(conn);
                let mut line = String::new();
                conn.read_line(&mut line).await.unwrap();
                let req: Value = serde_json::from_str(line.trim()).unwrap();
                let mut out = serde_json::to_vec(&answer(req["op"].as_str().unwrap())).unwrap();
                out.push(b'\n');
                conn.get_mut().write_all(&out).await.unwrap();
            }
        });
        path
    }

    #[test]
    fn window_writes_are_receiver_only() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = config_with_profile(dir.path(), "ground_station");
        let role = dir.path().join("role");
        std::fs::write(&role, "relay\n").unwrap();
        let refused = receiver_gate(&cfg, &dir.path().join("profile.conf"), &role).unwrap();
        assert_eq!(refused.status(), StatusCode::CONFLICT);
        std::fs::write(&role, "receiver\n").unwrap();
        assert!(receiver_gate(&cfg, &dir.path().join("profile.conf"), &role).is_none());
        let drone = config_with_profile(dir.path(), "drone");
        let refused = receiver_gate(&drone, &dir.path().join("profile.conf"), &role).unwrap();
        assert_eq!(refused.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn accept_relays_the_window_code_and_a_refusal_is_a_503() {
        let dir = tempfile::tempdir().unwrap();
        let sock = fake_daemon(
            dir.path(),
            |_| json!({"ok": true, "result": {"opened_at_ms": 1000, "closes_at_ms": 61000, "code": "123456"}}),
        );
        let (status, body) = body_json(pair_accept_at(&sock, 60).await).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body,
            json!({"opened_at_ms": 1000, "closes_at_ms": 61000, "duration_s": 60, "code": "123456"})
        );
        let (status, _) = body_json(pair_accept_at(&sock, 301).await).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

        let absent = dir.path().join("absent.sock");
        let (status, body) = body_json(pair_accept_at(&absent, 60).await).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            body["detail"]["error"]["code"],
            json!("E_PAIR_DAEMON_UNAVAILABLE")
        );
    }

    #[tokio::test]
    async fn approve_maps_a_closed_window_and_an_unknown_request() {
        let dir = tempfile::tempdir().unwrap();
        let (a, b) = (dir.path().join("a"), dir.path().join("b"));
        std::fs::create_dir(&a).unwrap();
        std::fs::create_dir(&b).unwrap();
        let closed = fake_daemon(&a, |_| json!({"ok": true, "result": {"open": false}}));
        let (status, body) = body_json(pair_approve_at(&closed, "relay-1").await).await;
        assert_eq!(status, StatusCode::GONE);
        assert_eq!(
            body["detail"]["error"]["code"],
            json!("E_PAIR_WINDOW_EXPIRED")
        );

        let unknown = fake_daemon(&b, |op| match op {
            "is_window_open" => json!({"ok": true, "result": {"open": true}}),
            _ => json!({"ok": false, "error": "request not found"}),
        });
        let (status, body) = body_json(pair_approve_at(&unknown, "relay-1").await).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(
            body["detail"]["error"]["code"],
            json!("E_PAIR_REQUEST_NOT_FOUND")
        );
    }

    #[test]
    fn a_receiver_cannot_join_a_mesh_but_a_direct_node_can() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = config_with_profile(dir.path(), "ground_station");
        let role = dir.path().join("role");
        std::fs::write(&role, "receiver\n").unwrap();
        let refused = join_gate(&cfg, &dir.path().join("profile.conf"), &role).unwrap();
        assert_eq!(refused.status(), StatusCode::CONFLICT);
        std::fs::write(&role, "direct\n").unwrap();
        assert!(join_gate(&cfg, &dir.path().join("profile.conf"), &role).is_none());
    }

    #[tokio::test]
    async fn a_join_relays_the_mesh_and_carries_the_daemon_error_code() {
        let dir = tempfile::tempdir().unwrap();
        let (a, b) = (dir.path().join("a"), dir.path().join("b"));
        std::fs::create_dir(&a).unwrap();
        std::fs::create_dir(&b).unwrap();
        let req = PairJoinRequest {
            code: "123456".into(),
            receiver_host: None,
            receiver_port: None,
        };
        let joined = fake_daemon(&a, |op| {
            assert_eq!(op, "join");
            json!({"ok": true, "result": {"mesh_id": "mesh-1", "receiver_host": "gs-a.local"}})
        });
        let (status, body) = body_json(pair_join_at(&joined, &req).await).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body,
            json!({"mesh_id": "mesh-1", "receiver_host": "gs-a.local", "ok": true})
        );

        let refused = fake_daemon(
            &b,
            |_| json!({"ok": false, "error": "bad code", "error_code": "E_INVITE_DECRYPT"}),
        );
        let (status, body) = body_json(pair_join_at(&refused, &req).await).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["detail"]["error"]["code"], json!("E_INVITE_DECRYPT"));

        let bad = PairJoinRequest {
            code: "12a456".into(),
            receiver_host: None,
            receiver_port: None,
        };
        let (status, _) = body_json(pair_join_at(&refused, &bad).await).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    }
}
