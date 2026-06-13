//! Ground-station mesh-pairing + PIC + captive-token read routes.
//!
//! Three read-only routes the ground-station GCS surface polls. Every one gates
//! on the node's RESOLVED profile being a ground station; a drone-profile node
//! gets `404` with a stable error code so the GCS can tell "wrong profile" from
//! "endpoint missing":
//!
//! - **`GET /api/v1/ground-station/pair/pending`** — the mesh pairing snapshot
//!   (the open Accept window + the list of pending relay join requests). The
//!   pairing state lives in `ados-mesh-pairing.service` when the split topology is
//!   enabled (`ADOS_PAIRING_VIA_DAEMON=1`): the route reaches it over the pairing
//!   daemon's Unix socket (`/run/ados/pairing.sock`) with a single-shot newline-
//!   JSON `snapshot` op and relays the result. A daemon that is unreachable yields
//!   `503` with `E_PAIR_DAEMON_UNAVAILABLE`, the same status the FastAPI route
//!   raises when its own socket round-trip fails. When the split topology is off,
//!   the snapshot is process-local state with no window opened yet, so the route
//!   reports the same `{"open": false}` a freshly-started agent does.
//! - **`GET /api/v1/ground-station/pic`** — the pilot-in-command arbiter state.
//!   The arbiter is in-process state that starts unclaimed on every process start
//!   and is never persisted, so this front (a separate process with no in-process
//!   arbiter and no on-disk PIC state to read) reports the same unclaimed default
//!   a freshly-started agent reports before any client claims PIC.
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

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};

use crate::config::PairingConfig;
use crate::profile::current_profile_and_role;

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

/// Whether the split pairing topology is on (`ADOS_PAIRING_VIA_DAEMON` truthy).
/// Mirrors the Python `use_daemon`: the REST path proxies to the pairing daemon
/// only when the operator has opted into the split topology. Default off.
fn pairing_via_daemon() -> bool {
    truthy_env("ADOS_PAIRING_VIA_DAEMON")
}

/// True when an env var holds one of the Python-truthy strings (`1`/`true`/`yes`,
/// case-insensitive), matching the Python `use_daemon` membership check.
fn truthy_env(name: &str) -> bool {
    matches!(
        std::env::var(name)
            .unwrap_or_default()
            .to_ascii_lowercase()
            .as_str(),
        "1" | "true" | "yes"
    )
}

/// `true` when the node's RESOLVED profile (read from `config_path` + the on-disk
/// profile/role sentinels) is a ground station. Mirrors the FastAPI
/// `is_ground_station` gate: an explicit config value wins, `"auto"`/empty falls
/// back to `/etc/ados/profile.conf`.
fn is_ground_station() -> bool {
    is_ground_station_at(&config_path())
}

/// The path-injectable core of the profile gate: resolve the wire profile off an
/// explicit config path (plus the profile/role sentinels `current_profile_and_role`
/// reads via their own env overrides) and return whether it is a ground station.
fn is_ground_station_at(config: &Path) -> bool {
    let cfg = PairingConfig::load_from(config);
    let (profile, _role) = current_profile_and_role(&cfg.agent.profile);
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
/// When the split topology is on, the snapshot lives in the pairing daemon: a
/// single-shot newline-JSON `snapshot` op over `/run/ados/pairing.sock` returns
/// the `{"open": ...}` (+ window/pending/approvals when a window is open) body,
/// relayed verbatim. A daemon that is unreachable yields `503
/// E_PAIR_DAEMON_UNAVAILABLE`, matching the FastAPI socket-failure path. When the
/// split topology is off, the snapshot is process-local state with no window
/// opened on this front, so the route reports `{"open": false}` (the same body a
/// freshly-started agent's in-process manager returns).
pub async fn get_pair_pending() -> Response {
    if !is_ground_station() {
        return profile_mismatch();
    }
    pair_pending_body(pairing_via_daemon(), &run_dir().join(PAIRING_SOCK_NAME)).await
}

/// The pair-pending body, with the daemon flag + socket path injected so a test
/// drives every branch without mutating the process environment. The profile gate
/// has already passed when this is called.
async fn pair_pending_body(via_daemon: bool, socket: &Path) -> Response {
    if !via_daemon {
        // No daemon: a fresh in-process manager has no window open, so the
        // snapshot is the `{"open": false}` default.
        return (StatusCode::OK, Json(json!({ "open": false }))).into_response();
    }
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
async fn pairing_daemon_snapshot(socket: &Path) -> std::io::Result<Value> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// A hard ceiling on the reply read; a pairing snapshot is a few hundred
    /// bytes (a small pending list), so this only guards a runaway body.
    const MAX_READ_BYTES: usize = 1024 * 1024;

    let mut stream = tokio::net::UnixStream::connect(socket).await?;
    let request = "{\"op\":\"snapshot\",\"args\":{}}\n";
    stream.write_all(request.as_bytes()).await?;
    stream.flush().await?;

    // Read until the first newline (the reply terminator) or EOF, bounded.
    let mut raw = Vec::new();
    let mut buf = [0u8; 8 * 1024];
    loop {
        let n = stream.read(&mut buf).await?;
        if n == 0 {
            break; // EOF (the daemon closes after one reply).
        }
        if raw.len() + n > MAX_READ_BYTES {
            return Err(std::io::Error::other("pairing daemon reply too large"));
        }
        raw.extend_from_slice(&buf[..n]);
        if raw.contains(&b'\n') {
            break;
        }
    }
    if raw.is_empty() {
        return Err(std::io::Error::other(
            "pairing daemon closed connection early",
        ));
    }

    let line_end = raw.iter().position(|b| *b == b'\n').unwrap_or(raw.len());
    let reply: Value = serde_json::from_slice(&raw[..line_end])
        .map_err(|e| std::io::Error::other(format!("pairing daemon reply not JSON: {e}")))?;

    let ok = reply.get("ok").and_then(Value::as_bool).unwrap_or(false);
    if !ok {
        return Err(std::io::Error::other("pairing daemon returned not-ok"));
    }
    // `result or {}` in the Python: a missing / non-object result is the empty
    // object.
    let result = reply
        .get("result")
        .filter(|v| v.is_object())
        .cloned()
        .unwrap_or_else(|| json!({}));
    Ok(result)
}

// ---------------------------------------------------------------------------
// GET /api/v1/ground-station/pic
// ---------------------------------------------------------------------------

/// The pilot-in-command arbiter state. Ground-station only; a drone-profile node
/// gets `404 E_PROFILE_MISMATCH`.
///
/// The arbiter is in-process state that starts unclaimed on every process start
/// and is never persisted to disk. This front is a separate process with no
/// in-process arbiter and no on-disk PIC state, so it reports the same unclaimed
/// default a freshly-started agent's arbiter reports before any client claims
/// PIC: `state` is `"unclaimed"`, the holder / since / counter / primary-gamepad
/// fields are all `null`/`0`. The field set + insertion order match
/// `PicArbiter.get_state`.
pub async fn get_pic_state() -> Response {
    if !is_ground_station() {
        return profile_mismatch();
    }
    (StatusCode::OK, Json(pic_default_state())).into_response()
}

/// The unclaimed default PIC state: the body a freshly-started arbiter reports
/// before any claim. The field set + insertion order match `PicArbiter.get_state`
/// (`state`, `claimed_by`, `claimed_since`, `claim_counter`, `primary_gamepad_id`).
fn pic_default_state() -> Value {
    json!({
        "state": "unclaimed",
        "claimed_by": Value::Null,
        "claimed_since": Value::Null,
        "claim_counter": 0,
        "primary_gamepad_id": Value::Null,
    })
}

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
    if !is_ground_station() {
        return profile_mismatch();
    }

    // The residual FastAPI is reached over its internal Unix socket, where a
    // request has no peer IP, so its AP-subnet/loopback gate never passes and it
    // raises `403 E_CAPTIVE_ONLY` for every caller. Match that exactly.
    gs_error(StatusCode::FORBIDDEN, "E_CAPTIVE_ONLY")
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
        assert!(!is_ground_station_at(&cfg));
    }

    #[test]
    fn ground_station_profile_is_a_ground_station() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = config_with_profile(dir.path(), "ground_station");
        assert!(is_ground_station_at(&cfg));
    }

    #[test]
    fn an_absent_config_is_not_a_ground_station() {
        // A missing config loads the all-defaults (`profile: auto`); with no
        // profile.conf, `auto` falls back to the drone default → not a GS.
        assert!(!is_ground_station_at(Path::new(
            "/nonexistent/ados/config.yaml"
        )));
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
    async fn pair_pending_reports_no_open_window_without_a_daemon() {
        // The split topology is off → the in-process snapshot default.
        let (status, body) =
            body_json(pair_pending_body(false, Path::new("/run/ados/pairing.sock")).await).await;
        assert_eq!(status, StatusCode::OK);
        // GOLDEN FIXTURE: the freshly-started in-process pairing manager has no
        // window open, so the snapshot is exactly `{"open": false}`.
        assert_eq!(body, json!({ "open": false }));
    }

    #[tokio::test]
    async fn pair_pending_503s_when_the_daemon_socket_is_absent() {
        let dir = tempfile::tempdir().unwrap();
        // Opt into the split topology but point at an absent socket: the
        // round-trip fails, so the route reports the daemon-unavailable status.
        let socket = dir.path().join("pairing.sock");
        let (status, body) = body_json(pair_pending_body(true, &socket).await).await;
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

        let (status, body) = body_json(pair_pending_body(true, &socket).await).await;
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

    #[tokio::test]
    async fn pic_default_state_is_the_unclaimed_body() {
        let (status, body) =
            body_json((StatusCode::OK, Json(pic_default_state())).into_response()).await;
        assert_eq!(status, StatusCode::OK);
        // GOLDEN FIXTURE: a freshly-started arbiter is unclaimed, matching
        // PicArbiter.get_state's field set + values before any claim.
        assert_eq!(
            body,
            json!({
                "state": "unclaimed",
                "claimed_by": null,
                "claimed_since": null,
                "claim_counter": 0,
                "primary_gamepad_id": null,
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

    /// Drive the real handler with `ADOS_CONFIG` pointed at a config that carries
    /// an explicit `agent.profile`, which resolves straight to the wire profile
    /// without consulting profile.conf. `ADOS_CONFIG` is read by no other test in
    /// this module, so the set/remove pair does not collide with a parallel test.
    async fn captive_token_with_profile(profile: &str) -> (StatusCode, Value) {
        let dir = tempfile::tempdir().unwrap();
        let cfg = config_with_profile(dir.path(), profile);
        std::env::set_var("ADOS_CONFIG", &cfg);
        let resp = get_captive_token().await;
        std::env::remove_var("ADOS_CONFIG");
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

    #[test]
    fn truthy_env_reads_the_python_truthy_set() {
        std::env::set_var("ADOS_TEST_TRUTHY", "yes");
        assert!(truthy_env("ADOS_TEST_TRUTHY"));
        std::env::set_var("ADOS_TEST_TRUTHY", "TRUE");
        assert!(truthy_env("ADOS_TEST_TRUTHY"));
        std::env::set_var("ADOS_TEST_TRUTHY", "0");
        assert!(!truthy_env("ADOS_TEST_TRUTHY"));
        std::env::remove_var("ADOS_TEST_TRUTHY");
        assert!(!truthy_env("ADOS_TEST_TRUTHY"));
    }
}
