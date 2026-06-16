//! Ground-station pilot-in-command (PIC) write routes.
//!
//! The four operator writes that drive the PIC arbiter on a ground station:
//!
//! - **`POST /api/v1/ground-station/pic/claim`** — claim PIC for a client. The
//!   body is `{"client_id", "confirm_token"?, "force"?}`; the route returns the
//!   arbiter outcome dict (`{claimed, claimed_by, claim_counter, ...}` on success,
//!   `{claimed:false, error, current_pic, needs_confirm:true, status:409}` on a
//!   soft-reject) at HTTP 200 throughout — the same shape + status the FastAPI
//!   route returns (the arbiter NEVER raises, so the FastAPI route's `409`/`400`
//!   exception arms are unreachable; the soft-reject rides in the 200 body).
//! - **`POST /api/v1/ground-station/pic/release`** — release PIC held by a
//!   client; returns `{released, previous_pic}` or `{released:false, error,
//!   current_pic, status:403}`, HTTP 200 throughout.
//! - **`POST /api/v1/ground-station/pic/confirm-token`** — mint a takeover
//!   confirm token; returns `{token, ttl_seconds}` (ttl is always 2).
//! - **`POST /api/v1/ground-station/pic/heartbeat`** — refresh the PIC session;
//!   returns the heartbeat ok dict (HTTP 200) or `410 E_PIC_NO_ACTIVE_CLAIM`
//!   when the caller does not hold PIC.
//!
//! ## Why these forward to the PIC control socket (the single arbiter owner)
//!
//! The PIC arbiter is in-process state with a single owner: the `ados-pic`
//! daemon, which holds the one `PicArbiter` instance and serves it over the
//! control socket at `/run/ados/pic.sock`. The daemon also fans PIC transition
//! events to subscribers (the residual `/pic/events` WebSocket, the display
//! layer) over that same socket and mirrors each transition to the
//! `/run/ados/pic-state.json` sidecar. So this front MUST NOT spin its own
//! arbiter (that would split-brain the state the WS + display read) — it forwards
//! each write to the daemon's socket with one newline-terminated JSON request,
//! reads one newline-terminated JSON reply, and translates the reply back into
//! the exact FastAPI route body. The ops are `claim` / `release` /
//! `confirm_token` / `heartbeat`, the same ops the daemon's dispatch implements.
//!
//! The socket reply carries a richer shape than the FastAPI body (a transport
//! `ok` flag plus a `mode` discriminant on claim, an `ok_heartbeat` flag on
//! heartbeat); each handler maps that reply to the byte-exact dict the Python
//! arbiter returned and the FastAPI route relayed.
//!
//! ## Degrade posture (the daemon socket is the only path)
//!
//! The FastAPI route reaches an in-process arbiter that is always present. This
//! front cannot — it has no in-process arbiter — so an unreachable / non-replying
//! socket degrades to the FastAPI `500 E_PIC_*_FAILED` error-object body (the
//! arm the Python route takes when the arbiter call raises). The write is never
//! silently dropped.
//!
//! ## The profile gate
//!
//! Like every ground-station route, this first gates on the resolved profile
//! being a ground station and returns the FastAPI
//! `404 {"detail":{"error":{"code":"E_PROFILE_MISMATCH"}}}` on a drone.

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Map, Value};

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

/// The `404` profile-mismatch response, byte-identical to the FastAPI
/// `HTTPException(status_code=404, detail={"error": {"code":
/// "E_PROFILE_MISMATCH"}})` (FastAPI wraps the `detail` dict under a top-level
/// `"detail"` key).
fn profile_mismatch() -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(json!({"detail": {"error": {"code": "E_PROFILE_MISMATCH"}}})),
    )
        .into_response()
}

/// Build a PIC route 4xx/5xx body in the FastAPI error-object detail shape:
/// `(status, {"detail": {"error": {"code", "message"}}})`. The PIC routes raise
/// `HTTPException(detail={"error": {...}})`, NOT the bare-string `detail` the rest
/// of the front uses, so these reproduce that nested shape.
fn pic_error(status: StatusCode, code: &str, message: impl Into<String>) -> Response {
    (
        status,
        Json(json!({"detail": {"error": {"code": code, "message": message.into()}}})),
    )
        .into_response()
}

// ---------------------------------------------------------------------------
// The PIC control socket seam.
// ---------------------------------------------------------------------------

/// The runtime dir (`ADOS_RUN_DIR`, default `/run/ados`), the same override the
/// sibling sockets + sidecars resolve under.
fn run_dir() -> std::path::PathBuf {
    std::path::PathBuf::from(
        std::env::var("ADOS_RUN_DIR").unwrap_or_else(|_| "/run/ados".to_string()),
    )
}

/// The `ados-pic` daemon's control socket (`/run/ados/pic.sock`), which applies
/// the `claim` / `release` / `confirm_token` / `heartbeat` ops through the single
/// `PicArbiter` instance the daemon owns.
fn pic_sock() -> std::path::PathBuf {
    run_dir().join("pic.sock")
}

/// The outcome of a PIC control-socket round-trip: the parsed reply object, or
/// the unavailable case (socket unreachable / no reply / unparseable).
enum PicReply {
    /// A parsed JSON-object reply from the daemon.
    Obj(Map<String, Value>),
    /// The socket was unreachable / did not reply / replied unparseably: the
    /// `500 E_PIC_*_FAILED` arm.
    Unavailable,
}

/// Send one newline-terminated JSON request to the PIC control socket and read
/// one newline-terminated JSON reply. An unreachable socket / a read error / an
/// unparseable or non-object reply all yield [`PicReply::Unavailable`] so the
/// caller can take the front's no-arbiter `500` posture. The read is bounded so a
/// runaway reply cannot exhaust memory.
async fn pic_request(request: &Value) -> PicReply {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// An arbiter reply is a few hundred bytes; bound the read to guard a runaway.
    const MAX_REPLY_BYTES: usize = 64 * 1024;

    let mut stream = match tokio::net::UnixStream::connect(pic_sock()).await {
        Ok(s) => s,
        Err(_) => return PicReply::Unavailable,
    };
    let mut line = match serde_json::to_vec(request) {
        Ok(b) => b,
        Err(_) => return PicReply::Unavailable,
    };
    line.push(b'\n');
    if stream.write_all(&line).await.is_err() || stream.flush().await.is_err() {
        return PicReply::Unavailable;
    }

    let mut raw = Vec::new();
    let mut buf = [0u8; 8 * 1024];
    loop {
        let n = match stream.read(&mut buf).await {
            Ok(n) => n,
            Err(_) => return PicReply::Unavailable,
        };
        if n == 0 {
            break; // EOF: the server replies once then closes.
        }
        if raw.len() + n > MAX_REPLY_BYTES {
            return PicReply::Unavailable;
        }
        raw.extend_from_slice(&buf[..n]);
        if raw.contains(&b'\n') {
            break;
        }
    }
    if raw.is_empty() {
        return PicReply::Unavailable;
    }
    let text = match String::from_utf8(raw) {
        Ok(t) => t,
        Err(_) => return PicReply::Unavailable,
    };
    let Some(first) = text.lines().next() else {
        return PicReply::Unavailable;
    };
    match serde_json::from_str::<Value>(first) {
        Ok(Value::Object(m)) => {
            // A transport-level `ok:false` (a malformed-request / unknown-op error
            // from the daemon's dispatch) is not a normal arbiter outcome; treat
            // it as the unavailable arm so the route surfaces the 500 rather than
            // a partial body. The arbiter outcomes always carry `ok:true`.
            if m.get("ok") == Some(&Value::Bool(false)) {
                PicReply::Unavailable
            } else {
                PicReply::Obj(m)
            }
        }
        _ => PicReply::Unavailable,
    }
}

// ---------------------------------------------------------------------------
// POST /api/v1/ground-station/pic/claim
// ---------------------------------------------------------------------------

/// The `pic/claim` request body. Mirrors the FastAPI `PicClaimRequest`: a
/// required `client_id`, an optional `confirm_token`, and an optional `force`
/// flag (defaulting false, matching the Pydantic `bool | None = False`).
#[derive(Debug, Deserialize)]
pub struct PicClaimRequest {
    pub client_id: String,
    #[serde(default)]
    pub confirm_token: Option<String>,
    #[serde(default)]
    pub force: Option<bool>,
}

/// `POST /api/v1/ground-station/pic/claim` → the arbiter outcome dict at HTTP 200.
///
/// `404 E_PROFILE_MISMATCH` off a ground station. Otherwise forwards a `claim` op
/// to the PIC control socket and translates the reply to the exact Python arbiter
/// dict: a fresh / idempotent / forced / transferred grant, or the soft-reject
/// (`{claimed:false, error, current_pic, needs_confirm:true, status:409}`) — all
/// at HTTP 200, the same status the FastAPI route returns (its arbiter never
/// raises, so the 409/400 exception arms never fire). An unreachable socket →
/// `500 E_PIC_CLAIM_FAILED`.
pub async fn post_pic_claim(
    State(state): State<AppState>,
    Json(req): Json<PicClaimRequest>,
) -> Response {
    if !is_ground_station(&state) {
        return profile_mismatch();
    }
    let request = json!({
        "op": "claim",
        "client_id": req.client_id,
        "confirm_token": req.confirm_token,
        "force": req.force.unwrap_or(false),
    });
    match pic_request(&request).await {
        PicReply::Obj(reply) => Json(claim_body(&reply)).into_response(),
        PicReply::Unavailable => pic_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "E_PIC_CLAIM_FAILED",
            "PIC control socket unavailable",
        ),
    }
}

/// Translate the daemon's `claim` reply into the byte-exact Python arbiter dict.
///
/// The socket reply carries a transport `ok` flag plus a `mode` discriminant on a
/// grant; the Python arbiter dict drops both and instead carries the
/// per-outcome flag (`idempotent` / `forced` / `transferred_from`). The
/// soft-reject reply (`claimed:false`) maps to the arbiter's reject dict verbatim
/// minus the transport `ok`. The field insertion order matches the Python dicts:
/// `claimed`, `claimed_by`, `claim_counter`, then the per-mode field; or
/// `claimed`, `error`, `current_pic`, `needs_confirm`, `status` for the reject.
fn claim_body(reply: &Map<String, Value>) -> Value {
    let claimed = reply
        .get("claimed")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if !claimed {
        // The soft-reject (already_claimed / invalid_confirm_token): the arbiter
        // dict is the reply minus the transport `ok`.
        return json!({
            "claimed": false,
            "error": reply.get("error").cloned().unwrap_or(Value::Null),
            "current_pic": reply.get("current_pic").cloned().unwrap_or(Value::Null),
            "needs_confirm": reply.get("needs_confirm").cloned().unwrap_or(Value::Bool(true)),
            "status": reply.get("status").cloned().unwrap_or(json!(409)),
        });
    }

    let claimed_by = reply.get("claimed_by").cloned().unwrap_or(Value::Null);
    let claim_counter = reply.get("claim_counter").cloned().unwrap_or(json!(0));
    let mode = reply.get("mode").and_then(Value::as_str).unwrap_or("fresh");
    match mode {
        // A fresh claim: just claimed / claimed_by / claim_counter.
        "fresh" => json!({
            "claimed": true,
            "claimed_by": claimed_by,
            "claim_counter": claim_counter,
        }),
        // A same-client re-claim carries the `idempotent: true` flag.
        "idempotent" => json!({
            "claimed": true,
            "claimed_by": claimed_by,
            "claim_counter": claim_counter,
            "idempotent": true,
        }),
        // A force takeover carries `forced: true` + the previous holder.
        "forced" => json!({
            "claimed": true,
            "claimed_by": claimed_by,
            "claim_counter": claim_counter,
            "forced": true,
            "previous_pic": reply.get("previous_pic").cloned().unwrap_or(Value::Null),
        }),
        // A confirm-token transfer carries the transfer source.
        "transferred" => json!({
            "claimed": true,
            "claimed_by": claimed_by,
            "claim_counter": claim_counter,
            "transferred_from": reply.get("transferred_from").cloned().unwrap_or(Value::Null),
        }),
        // An unknown mode degrades to the fresh shape (defensive; the daemon only
        // emits the four above on a grant).
        _ => json!({
            "claimed": true,
            "claimed_by": claimed_by,
            "claim_counter": claim_counter,
        }),
    }
}

// ---------------------------------------------------------------------------
// POST /api/v1/ground-station/pic/release
// ---------------------------------------------------------------------------

/// The `pic/release` request body. Mirrors the FastAPI `PicReleaseRequest`: a
/// required `client_id`.
#[derive(Debug, Deserialize)]
pub struct PicReleaseRequest {
    pub client_id: String,
}

/// `POST /api/v1/ground-station/pic/release` → the release outcome dict at HTTP
/// 200.
///
/// `404 E_PROFILE_MISMATCH` off a ground station. Otherwise forwards a `release`
/// op and returns `{released:true, previous_pic}` on success or `{released:false,
/// error:"not_current_pic", current_pic, status:403}` when the caller does not
/// hold PIC — both at HTTP 200 (the FastAPI route returns the arbiter dict as-is;
/// the arbiter never raises). An unreachable socket → `500 E_PIC_RELEASE_FAILED`.
pub async fn post_pic_release(
    State(state): State<AppState>,
    Json(req): Json<PicReleaseRequest>,
) -> Response {
    if !is_ground_station(&state) {
        return profile_mismatch();
    }
    let request = json!({"op": "release", "client_id": req.client_id});
    match pic_request(&request).await {
        PicReply::Obj(reply) => Json(release_body(&reply)).into_response(),
        PicReply::Unavailable => pic_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "E_PIC_RELEASE_FAILED",
            "PIC control socket unavailable",
        ),
    }
}

/// Translate the daemon's `release` reply into the Python arbiter dict (drop the
/// transport `ok`). A `released:true` carries `previous_pic`; a `released:false`
/// carries the not-current-pic reject (`error`, `current_pic`, `status:403`),
/// matching the field set + insertion order of the Python `release` return.
fn release_body(reply: &Map<String, Value>) -> Value {
    let released = reply
        .get("released")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if released {
        json!({
            "released": true,
            "previous_pic": reply.get("previous_pic").cloned().unwrap_or(Value::Null),
        })
    } else {
        json!({
            "released": false,
            "error": reply.get("error").cloned().unwrap_or(Value::Null),
            "current_pic": reply.get("current_pic").cloned().unwrap_or(Value::Null),
            "status": reply.get("status").cloned().unwrap_or(json!(403)),
        })
    }
}

// ---------------------------------------------------------------------------
// POST /api/v1/ground-station/pic/confirm-token
// ---------------------------------------------------------------------------

/// The `pic/confirm-token` request body. Mirrors the FastAPI
/// `PicConfirmTokenRequest`: a required `client_id`.
#[derive(Debug, Deserialize)]
pub struct PicConfirmTokenRequest {
    pub client_id: String,
}

/// `POST /api/v1/ground-station/pic/confirm-token` → `{token, ttl_seconds}`.
///
/// `404 E_PROFILE_MISMATCH` off a ground station. Otherwise forwards a
/// `confirm_token` op and returns `{token: <32-hex>, ttl_seconds: 2}` — the
/// arbiter mints a plain string token, and the FastAPI route reports the fixed
/// `ttl_seconds: 2` it hard-codes for the string-token path. An unreachable
/// socket → `500 E_PIC_TOKEN_FAILED`.
pub async fn post_pic_confirm_token(
    State(state): State<AppState>,
    Json(req): Json<PicConfirmTokenRequest>,
) -> Response {
    if !is_ground_station(&state) {
        return profile_mismatch();
    }
    let request = json!({"op": "confirm_token", "client_id": req.client_id});
    match pic_request(&request).await {
        PicReply::Obj(reply) => {
            let token = reply
                .get("token")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            // The FastAPI route hard-codes ttl_seconds=2 for the string-token path
            // (the arbiter's create_confirm_token returns a plain string, so the
            // `isinstance(token, dict)` branch never fires).
            Json(json!({"token": token, "ttl_seconds": 2})).into_response()
        }
        PicReply::Unavailable => pic_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "E_PIC_TOKEN_FAILED",
            "PIC control socket unavailable",
        ),
    }
}

// ---------------------------------------------------------------------------
// POST /api/v1/ground-station/pic/heartbeat
// ---------------------------------------------------------------------------

/// The `pic/heartbeat` request body. Mirrors the FastAPI `PicHeartbeatRequest`: a
/// required `client_id`.
#[derive(Debug, Deserialize)]
pub struct PicHeartbeatRequest {
    pub client_id: String,
}

/// `POST /api/v1/ground-station/pic/heartbeat` → the heartbeat ok dict (HTTP 200)
/// or `410 E_PIC_NO_ACTIVE_CLAIM`.
///
/// `404 E_PROFILE_MISMATCH` off a ground station. Otherwise forwards a
/// `heartbeat` op. On a held claim the reply's `ok_heartbeat` is true and the
/// route returns `{ok:true, claimed_by, claim_counter, last_heartbeat_ts}` at
/// HTTP 200. When the caller does not hold PIC the reply's `ok_heartbeat` is
/// false and the route raises the FastAPI `410` carrying the
/// `E_PIC_NO_ACTIVE_CLAIM` error object with the reply's `error` message + the
/// `current_pic`. An unreachable socket → `500 E_PIC_HEARTBEAT_FAILED`.
pub async fn post_pic_heartbeat(
    State(state): State<AppState>,
    Json(req): Json<PicHeartbeatRequest>,
) -> Response {
    if !is_ground_station(&state) {
        return profile_mismatch();
    }
    let request = json!({"op": "heartbeat", "client_id": req.client_id});
    match pic_request(&request).await {
        PicReply::Obj(reply) => heartbeat_response(&reply),
        PicReply::Unavailable => pic_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "E_PIC_HEARTBEAT_FAILED",
            "PIC control socket unavailable",
        ),
    }
}

/// Map the daemon's `heartbeat` reply to the FastAPI response. The reply's
/// `ok_heartbeat` flag is the arbiter's `ok`: when true the route returns the
/// `{ok:true, claimed_by, claim_counter, last_heartbeat_ts}` dict at 200; when
/// false the FastAPI route raises `HTTPException(status, detail={"error":
/// {"code":"E_PIC_NO_ACTIVE_CLAIM","message":<error>,"current_pic":<cp>}})` — the
/// status from the reply (410), the error object carrying the reply's `error`
/// string and the current holder.
fn heartbeat_response(reply: &Map<String, Value>) -> Response {
    let ok = reply
        .get("ok_heartbeat")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if ok {
        // The arbiter `heartbeat` ok dict: ok / claimed_by / claim_counter /
        // last_heartbeat_ts, in that insertion order.
        return Json(json!({
            "ok": true,
            "claimed_by": reply.get("claimed_by").cloned().unwrap_or(Value::Null),
            "claim_counter": reply.get("claim_counter").cloned().unwrap_or(json!(0)),
            "last_heartbeat_ts": reply.get("last_heartbeat_ts").cloned().unwrap_or(Value::Null),
        }))
        .into_response();
    }
    // The not-holder path: the FastAPI route raises the 410 with the error object.
    let status = reply
        .get("status")
        .and_then(Value::as_u64)
        .and_then(|s| StatusCode::from_u16(s as u16).ok())
        .unwrap_or(StatusCode::GONE);
    let message = reply
        .get("error")
        .and_then(Value::as_str)
        .unwrap_or("no active claim")
        .to_string();
    let current_pic = reply.get("current_pic").cloned().unwrap_or(Value::Null);
    (
        status,
        Json(json!({
            "detail": {
                "error": {
                    "code": "E_PIC_NO_ACTIVE_CLAIM",
                    "message": message,
                    "current_pic": current_pic,
                }
            }
        })),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::UnixListener;

    /// Read a response body as JSON.
    async fn body_json(resp: Response) -> Value {
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    fn obj(v: Value) -> Map<String, Value> {
        v.as_object().unwrap().clone()
    }

    /// Spin a one-shot PIC control socket at `/run/ados/pic.sock` (under a temp
    /// `ADOS_RUN_DIR`) that reads one request line and replies with `reply`, then
    /// runs the handler `run`. Returns `{request, status, body}` so the test can
    /// assert both the op forwarded and the response. Serializes behind the
    /// crate-wide env lock (the `ADOS_RUN_DIR` override is process-wide).
    async fn with_socket<F>(reply: Value, run: F) -> Value
    where
        F: std::future::Future<Output = Response>,
    {
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("ADOS_RUN_DIR", dir.path());
        let sock = dir.path().join("pic.sock");
        let listener = UnixListener::bind(&sock).unwrap();

        let server = tokio::spawn(async move {
            let (mut conn, _addr) = listener.accept().await.unwrap();
            let mut raw = Vec::new();
            let mut buf = [0u8; 1024];
            loop {
                let n = conn.read(&mut buf).await.unwrap();
                if n == 0 {
                    break;
                }
                raw.extend_from_slice(&buf[..n]);
                if raw.contains(&b'\n') {
                    break;
                }
            }
            let line: Vec<u8> = raw.split(|&b| b == b'\n').next().unwrap().to_vec();
            let request: Value = serde_json::from_slice(&line).unwrap();
            let mut body = serde_json::to_vec(&reply).unwrap();
            body.push(b'\n');
            conn.write_all(&body).await.unwrap();
            conn.flush().await.unwrap();
            request
        });

        let resp = run.await;
        let request = server.await.unwrap();
        std::env::remove_var("ADOS_RUN_DIR");
        let status = resp.status().as_u16();
        let body = body_json(resp).await;
        drop(dir);
        json!({ "request": request, "status": status, "body": body })
    }

    // ── claim_body translation ───────────────────────────────────────────────

    #[test]
    fn claim_body_fresh_drops_ok_and_mode() {
        let reply = obj(json!({
            "ok": true, "claimed": true, "mode": "fresh",
            "claimed_by": "op-a", "claim_counter": 1
        }));
        assert_eq!(
            claim_body(&reply),
            json!({"claimed": true, "claimed_by": "op-a", "claim_counter": 1})
        );
    }

    #[test]
    fn claim_body_idempotent_carries_the_flag() {
        let reply = obj(json!({
            "ok": true, "claimed": true, "mode": "idempotent",
            "claimed_by": "op-a", "claim_counter": 1
        }));
        assert_eq!(
            claim_body(&reply),
            json!({"claimed": true, "claimed_by": "op-a", "claim_counter": 1, "idempotent": true})
        );
    }

    #[test]
    fn claim_body_forced_carries_previous_pic() {
        let reply = obj(json!({
            "ok": true, "claimed": true, "mode": "forced",
            "claimed_by": "op-b", "claim_counter": 2, "previous_pic": "op-a"
        }));
        assert_eq!(
            claim_body(&reply),
            json!({
                "claimed": true, "claimed_by": "op-b", "claim_counter": 2,
                "forced": true, "previous_pic": "op-a"
            })
        );
    }

    #[test]
    fn claim_body_transferred_carries_the_source() {
        let reply = obj(json!({
            "ok": true, "claimed": true, "mode": "transferred",
            "claimed_by": "op-b", "claim_counter": 2, "transferred_from": "op-a"
        }));
        assert_eq!(
            claim_body(&reply),
            json!({
                "claimed": true, "claimed_by": "op-b", "claim_counter": 2,
                "transferred_from": "op-a"
            })
        );
    }

    #[test]
    fn claim_body_soft_reject_maps_to_the_409_dict() {
        let reply = obj(json!({
            "ok": true, "claimed": false, "error": "already_claimed",
            "current_pic": "op-a", "needs_confirm": true, "status": 409
        }));
        assert_eq!(
            claim_body(&reply),
            json!({
                "claimed": false, "error": "already_claimed",
                "current_pic": "op-a", "needs_confirm": true, "status": 409
            })
        );
    }

    // ── release_body translation ─────────────────────────────────────────────

    #[test]
    fn release_body_success_and_reject() {
        let ok = obj(json!({"ok": true, "released": true, "previous_pic": "op-a"}));
        assert_eq!(
            release_body(&ok),
            json!({"released": true, "previous_pic": "op-a"})
        );
        let reject = obj(json!({
            "ok": true, "released": false, "error": "not_current_pic",
            "current_pic": "op-a", "status": 403
        }));
        assert_eq!(
            release_body(&reject),
            json!({
                "released": false, "error": "not_current_pic",
                "current_pic": "op-a", "status": 403
            })
        );
    }

    // ── heartbeat translation ────────────────────────────────────────────────

    #[tokio::test]
    async fn heartbeat_ok_returns_the_200_dict() {
        let reply = obj(json!({
            "ok": true, "ok_heartbeat": true, "claimed_by": "op-a",
            "claim_counter": 1, "last_heartbeat_ts": 12.5
        }));
        let resp = heartbeat_response(&reply);
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            body_json(resp).await,
            json!({
                "ok": true, "claimed_by": "op-a",
                "claim_counter": 1, "last_heartbeat_ts": 12.5
            })
        );
    }

    #[tokio::test]
    async fn heartbeat_not_holder_is_a_410_error_object() {
        let reply = obj(json!({
            "ok": true, "ok_heartbeat": false, "error": "no_active_claim",
            "current_pic": "op-a", "status": 410
        }));
        let resp = heartbeat_response(&reply);
        assert_eq!(resp.status(), StatusCode::GONE);
        assert_eq!(
            body_json(resp).await,
            json!({"detail": {"error": {
                "code": "E_PIC_NO_ACTIVE_CLAIM",
                "message": "no_active_claim",
                "current_pic": "op-a",
            }}})
        );
    }

    // ── error envelopes ──────────────────────────────────────────────────────

    #[tokio::test]
    async fn pic_error_is_the_error_object_shape() {
        let resp = pic_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "E_PIC_CLAIM_FAILED",
            "PIC control socket unavailable",
        );
        assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(
            body_json(resp).await,
            json!({"detail": {"error": {
                "code": "E_PIC_CLAIM_FAILED",
                "message": "PIC control socket unavailable",
            }}})
        );
    }

    #[tokio::test]
    async fn profile_mismatch_is_the_fastapi_404_shape() {
        let resp = profile_mismatch();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            body_json(resp).await,
            json!({"detail": {"error": {"code": "E_PROFILE_MISMATCH"}}})
        );
    }

    // ── the no-socket degrade arms (the 500 path) ────────────────────────────
    //
    // These set the process-wide ADOS_RUN_DIR, so they serialize behind the
    // crate-wide env lock held across the handler's await for the whole run. They
    // exercise the unavailable path WITHOUT the profile gate (the gate reads the
    // real config and would 404 on a dev host); each calls pic_request directly so
    // the socket seam is covered independent of the gate.

    #[tokio::test]
    async fn pic_request_with_no_socket_is_unavailable() {
        let _guard = crate::lock_env().await;
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("ADOS_RUN_DIR", dir.path());
        let out = pic_request(&json!({"op": "claim", "client_id": "op-a"})).await;
        std::env::remove_var("ADOS_RUN_DIR");
        assert!(matches!(out, PicReply::Unavailable));
    }

    #[tokio::test]
    async fn pic_request_treats_ok_false_as_unavailable() {
        // A transport ok:false (a malformed-request error from the dispatch) is
        // not an arbiter outcome; the route surfaces it as the 500, never a body.
        let _guard = crate::lock_env().await;
        let out = with_socket(
            json!({"ok": false, "error": "E_MISSING_CLIENT_ID"}),
            async {
                match pic_request(&json!({"op": "claim"})).await {
                    PicReply::Unavailable => {
                        pic_error(StatusCode::INTERNAL_SERVER_ERROR, "E_PIC_CLAIM_FAILED", "x")
                    }
                    PicReply::Obj(_) => Json(json!({"unexpected": true})).into_response(),
                }
            },
        )
        .await;
        assert_eq!(out["status"], json!(500));
        assert_eq!(
            out["body"]["detail"]["error"]["code"],
            json!("E_PIC_CLAIM_FAILED")
        );
    }

    // ── the full forward path against a live mock socket ─────────────────────

    #[tokio::test]
    async fn claim_forwards_the_op_and_returns_the_fresh_body() {
        let _guard = crate::lock_env().await;
        let out = with_socket(
            json!({"ok": true, "claimed": true, "mode": "fresh", "claimed_by": "op-a", "claim_counter": 1}),
            async {
                // Drive the socket seam + translation directly (the profile gate
                // reads the real config and would 404 on a dev host).
                match pic_request(&json!({
                    "op": "claim", "client_id": "op-a", "confirm_token": null, "force": false
                }))
                .await
                {
                    PicReply::Obj(reply) => Json(claim_body(&reply)).into_response(),
                    PicReply::Unavailable => Json(json!({"unavailable": true})).into_response(),
                }
            },
        )
        .await;
        // The exact op + fields forwarded.
        assert_eq!(out["request"]["op"], json!("claim"));
        assert_eq!(out["request"]["client_id"], json!("op-a"));
        assert_eq!(out["request"]["force"], json!(false));
        assert_eq!(out["status"], json!(200));
        assert_eq!(
            out["body"],
            json!({"claimed": true, "claimed_by": "op-a", "claim_counter": 1})
        );
    }

    #[tokio::test]
    async fn confirm_token_returns_token_and_fixed_ttl() {
        let _guard = crate::lock_env().await;
        let out = with_socket(json!({"ok": true, "token": "a".repeat(32)}), async {
            match pic_request(&json!({"op": "confirm_token", "client_id": "op-b"})).await {
                PicReply::Obj(reply) => {
                    let token = reply
                        .get("token")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    Json(json!({"token": token, "ttl_seconds": 2})).into_response()
                }
                PicReply::Unavailable => Json(json!({"unavailable": true})).into_response(),
            }
        })
        .await;
        assert_eq!(out["request"]["op"], json!("confirm_token"));
        assert_eq!(out["status"], json!(200));
        assert_eq!(out["body"]["ttl_seconds"], json!(2));
        assert_eq!(out["body"]["token"].as_str().unwrap().len(), 32);
    }
}
