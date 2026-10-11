//! Ground-station pilot-in-command (PIC) routes.
//!
//! The live arbiter read plus the four operator writes that drive it on a
//! ground station:
//!
//! - **`GET /api/v1/ground-station/pic`** — the live arbiter state
//!   (`{state, claimed_by, claimed_since, claim_counter, primary_gamepad_id}`),
//!   read through the same control socket the writes use. `503
//!   E_PIC_ARBITER_UNAVAILABLE` when the arbiter cannot be reached — never a
//!   fabricated `unclaimed`, which would read as "nobody has the sticks".
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
//! the exact FastAPI route body. The ops are `get_state` / `claim` / `release` /
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
//! arm the Python route takes when the arbiter call raises) for the writes, and
//! to `503 E_PIC_ARBITER_UNAVAILABLE` for the read. The write is never silently
//! dropped and the read never substitutes a plausible state for an absent one.
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

mod token_heartbeat;

pub use token_heartbeat::post_pic_confirm_token;
pub use token_heartbeat::post_pic_heartbeat;
pub use token_heartbeat::PicConfirmTokenRequest;
pub use token_heartbeat::PicHeartbeatRequest;
#[cfg(test)]
use token_heartbeat::*;

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

/// The `ados-pic` daemon's control socket (`/run/ados/pic.sock`), which applies
/// the `claim` / `release` / `confirm_token` / `heartbeat` ops through the single
/// `PicArbiter` instance the daemon owns.
fn pic_sock() -> std::path::PathBuf {
    ados_protocol::paths::run_path("pic")
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
    pic_request_at(&pic_sock(), request).await
}

/// The path-injectable core of [`pic_request`]: round-trip against an explicit
/// control-socket path. Threaded so a test drives the socket seam against a
/// tempdir without mutating the process-global `ADOS_RUN_DIR`.
async fn pic_request_at(sock: &std::path::Path, request: &Value) -> PicReply {
    match crate::ipc::cmd::roundtrip_object(sock, request, crate::ipc::cmd::QUICK).await {
        // A transport-level `ok:false` (a malformed-request / unknown-op error
        // from the daemon's dispatch) is not a normal arbiter outcome; treat it as
        // the unavailable arm so the route surfaces the 500 rather than a partial
        // body. The arbiter outcomes always carry `ok:true`.
        Ok(m) if m.get("ok") == Some(&Value::Bool(false)) => PicReply::Unavailable,
        Ok(m) => PicReply::Obj(m),
        Err(_) => PicReply::Unavailable,
    }
}

// ---------------------------------------------------------------------------
// GET /api/v1/ground-station/pic
// ---------------------------------------------------------------------------

/// The PIC arbiter's live state fields, in `PicArbiter.get_state`'s insertion
/// order, matching the daemon's `get_state` reply, which carries exactly
/// these plus the transport `ok` flag.
const PIC_STATE_FIELDS: [&str; 5] = [
    "state",
    "claimed_by",
    "claimed_since",
    "claim_counter",
    "primary_gamepad_id",
];

/// `GET /api/v1/ground-station/pic` → the live pilot-in-command arbiter state.
///
/// `404 E_PROFILE_MISMATCH` off a ground station. Otherwise forwards a
/// `get_state` op to the PIC control socket and returns the arbiter's own fields
/// (`state`, `claimed_by`, `claimed_since`, `claim_counter`,
/// `primary_gamepad_id`) with the transport `ok` dropped. An unreachable /
/// non-replying socket is `503 E_PIC_ARBITER_UNAVAILABLE` — "we cannot see the
/// arbiter", which is a different fact from "nobody holds PIC".
///
/// ## Why this reads the socket and never answers from a default
///
/// This route used to return a hardcoded `{"state":"unclaimed", ...}`, justified
/// by there being "no in-process arbiter and no on-disk PIC state to read". Both
/// halves of that are false in this very crate: `ados-pic` owns the single
/// arbiter on `/run/ados/pic.sock` (the four writes above already reach it) and
/// mirrors every transition to the `pic-state` sidecar, which
/// `ados-crsf`'s channel packer and the router's injector gate already read to
/// decide who has stick authority.
///
/// So an operator polling this endpoint while another client held PIC was told
/// "no pilot in command" — while the router was refusing their injection on the
/// basis of the real holder. Taking over on that reading is a mid-flight control
/// conflict, which is why an unreadable arbiter degrades to a 503 the GCS can
/// render as unknown rather than to a value that reads as safe.
pub async fn get_pic_state(State(state): State<AppState>) -> Response {
    if !is_ground_station(&state) {
        return profile_mismatch();
    }
    match pic_request(&json!({"op": "get_state"})).await {
        PicReply::Obj(reply) => Json(pic_state_body(&reply)).into_response(),
        PicReply::Unavailable => pic_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "E_PIC_ARBITER_UNAVAILABLE",
            "PIC control socket unavailable",
        ),
    }
}

/// Project the daemon's `get_state` reply to the arbiter dict: the five state
/// fields in order, transport `ok` dropped. A field the reply omits is carried as
/// an explicit `null` rather than dropped from the object, so a consumer reads
/// "the arbiter did not report this" instead of inferring a value from a missing
/// key.
fn pic_state_body(reply: &Map<String, Value>) -> Value {
    let mut out = Map::new();
    for key in PIC_STATE_FIELDS {
        out.insert(
            key.to_string(),
            reply.get(key).cloned().unwrap_or(Value::Null),
        );
    }
    Value::Object(out)
}

// ---------------------------------------------------------------------------
// POST /api/v1/ground-station/pic/claim
// ---------------------------------------------------------------------------

/// The `pic/claim` request body: a required `client_id`, an optional `confirm_token`, and an
/// optional `force` flag (defaulting false, matching the Pydantic `bool | None = False`).
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
        PicReply::Obj(reply) if has_outcome(&reply, "claimed") => {
            let mut body = claim_body(&reply);
            attach_injector_ticket(&state, &mut body);
            Json(body).into_response()
        }
        PicReply::Obj(_) => pic_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "E_PIC_CLAIM_FAILED",
            "the PIC arbiter reply carried no claim outcome",
        ),
        PicReply::Unavailable => pic_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "E_PIC_CLAIM_FAILED",
            "PIC control socket unavailable",
        ),
    }
}

/// Whether an arbiter reply carries its outcome flag as a boolean. A reply
/// without one decided nothing, so it must not be read as a refusal (another
/// pilot holds PIC) or as a grant.
fn has_outcome(reply: &Map<String, Value>, flag: &str) -> bool {
    reply.get(flag).is_some_and(Value::is_boolean)
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

/// The `pic/release` request body: a required `client_id`.
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
        PicReply::Obj(reply) if has_outcome(&reply, "released") => {
            Json(release_body(&reply)).into_response()
        }
        PicReply::Obj(_) => pic_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "E_PIC_RELEASE_FAILED",
            "the PIC arbiter reply carried no release outcome",
        ),
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

/// How long the injector attestation handed to the PIC holder lasts. It is
/// re-issued on every claim and heartbeat, so it only has to outlive the gap
/// between heartbeats.
const PIC_INJECTOR_TICKET_TTL_SECONDS: i64 = 120;

/// Add `injector_ticket` to a successful claim or heartbeat body: the RC-lane
/// attestation for the client the arbiter confirms holds PIC (`claimed_by`).
///
/// This is the only place an attestation for a client id is minted, so the
/// RC injection route can no longer attest whatever name a request body
/// carries: a caller has to have been granted PIC under that name. On an
/// unpaired node there is no key to sign with and nothing is added.
fn attach_injector_ticket(state: &AppState, body: &mut Value) {
    if body.get("claimed").and_then(Value::as_bool) == Some(false) {
        return;
    }
    let Some(holder) = body
        .get("claimed_by")
        .and_then(Value::as_str)
        .filter(|h| !h.is_empty())
    else {
        return;
    };
    if let Some(ticket) = ados_protocol::ws_ticket::mint_scoped_ticket(
        &state.pairing_paths.pairing_json,
        &ados_protocol::ws_ticket::crsf_inject_scope(holder),
        PIC_INJECTOR_TICKET_TTL_SECONDS,
    ) {
        body["injector_ticket"] = json!(ticket);
    }
}

#[cfg(test)]
mod tests;
