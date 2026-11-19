//! `POST /api/v1/ground-station/uplink` and `GET /api/v1/ground-station/uplink`:
//! the uplink handover between this ground station and a phone.
//!
//! One ground transmits for a fleet at a time. A phone that holds an approved
//! ground-role invite takes the uplink from the box (`take`) and hands it back
//! (`give_back`); the box's own operator takes it back (`take_back`) from any
//! box surface. The route authenticates the caller and forwards to
//! `ados-groundlink`'s command socket, which stops or starts the box's two
//! ground transmitters and owns the state; the reply is that socket's.
//!
//! - `take` / `give_back` must carry the phone's key ([`sign::PHONE_KEY_HEADER`]),
//!   a timestamp and an HMAC under the uplink key the phone's invite delivered
//!   ([`phones`], [`sign`]); the last timestamp accepted from each phone is
//!   kept on disk, so a captured request stays a replay across a restart.
//!   `take` may carry `yield_port`: the box calls
//!   `http://<the phone's address>:<yield_port>/uplink/yield` before a
//!   take-back, signed under its own MAC context.
//! - `take_back` is the box's operator: any caller that passed this listener's
//!   own auth (on-box, or holding the node's key). The phone is told first and
//!   given [`sign::YIELD_TIMEOUT`] to stop and confirm; the box transmits
//!   either way.
//!
//! Ground-station only; listed in [`crate::auth::RELAY_FORBIDDEN_PATHS`], so no
//! request that crossed the radio can move the uplink.
//!
//! Replies: `200 {ok: true, state}` or `{error: "E_…", detail: "<one sentence>"}`.

use std::path::Path;
use std::time::{Duration, SystemTime};

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use serde::Deserialize;
use serde_json::{json, Map, Value};

use crate::routes::gs_cmd::groundlink_cmd_roundtrip;
use crate::serve::PeerAddr;
use crate::state::AppState;

pub mod phones;
pub mod sign;

#[cfg(test)]
mod tests;

/// The request body.
#[derive(Debug, Deserialize)]
pub struct UplinkRequest {
    pub op: String,
    #[serde(default)]
    pub holder_device_id: Option<String>,
    #[serde(default)]
    pub holder_name: Option<String>,
    /// The phone's take-back listener port, on the address its request came from.
    #[serde(default)]
    pub yield_port: Option<u16>,
}

/// One refusal: `{error, detail}`.
pub fn refusal(status: StatusCode, code: &str, detail: &str) -> Response {
    (status, Json(json!({"error": code, "detail": detail}))).into_response()
}

/// The HTTP status for an apply-time error from the command socket.
pub fn status_for(code: &str) -> StatusCode {
    match code {
        "E_BAD_REQUEST" | "E_BAD_HOLDER" => StatusCode::BAD_REQUEST,
        "E_NOT_HOLDER" => StatusCode::FORBIDDEN,
        "E_TX_STOP_FAILED" => StatusCode::SERVICE_UNAVAILABLE,
        _ => StatusCode::CONFLICT,
    }
}

/// Map a command-socket reply to the route's reply.
pub fn reply_from(socket: Option<Value>) -> Response {
    let Some(v) = socket else {
        return refusal(
            StatusCode::SERVICE_UNAVAILABLE,
            "E_GROUNDLINK_UNAVAILABLE",
            "The ground-station radio service did not answer.",
        );
    };
    if v.get("ok").and_then(Value::as_bool) == Some(true) {
        let mut body = Map::new();
        body.insert("ok".into(), json!(true));
        body.insert(
            "state".into(),
            v.get("state").cloned().unwrap_or(Value::Null),
        );
        if let Some(u) = v.get("uplink") {
            body.insert("uplink".into(), u.clone());
        }
        return Json(Value::Object(body)).into_response();
    }
    let code = v.get("error").and_then(Value::as_str).unwrap_or("E_UPLINK");
    let detail = v
        .get("detail")
        .and_then(Value::as_str)
        .unwrap_or("The ground-station radio service refused the request.");
    refusal(status_for(code), code, detail)
}

fn is_ground_station() -> bool {
    let cfg = crate::config::PairingConfig::load();
    let (profile, _role) = crate::profile::current_profile_and_role(&cfg.agent.profile);
    profile == "ground-station"
}

fn not_ground_station() -> Response {
    refusal(
        StatusCode::NOT_FOUND,
        "E_PROFILE_MISMATCH",
        "This node is not a ground station.",
    )
}

fn now_ms() -> i64 {
    ados_groundlink::pairing::crypto::now_ms()
}

/// The `uplink` field of a fresh radio sidecar under `run_dir`, or null when
/// the radio service is not reporting: a stale file says nothing about which
/// ground transmits now.
pub fn uplink_view(run_dir: &Path) -> Value {
    let path = run_dir.join(ados_protocol::paths::WFB_STATS_JSON);
    let window = Some(Duration::from_secs_f64(
        crate::routes::gs_status::SNAPSHOT_FRESH_S,
    ));
    ados_protocol::sidecar::read_fresh_at::<Map<String, Value>>(&path, window, SystemTime::now())
        .fresh()
        .and_then(|m| m.get("uplink").cloned())
        .filter(Value::is_object)
        .unwrap_or(Value::Null)
}

/// `GET /api/v1/ground-station/uplink` → `{uplink: {...} | null}`.
pub async fn get_uplink(State(state): State<AppState>) -> Response {
    if !is_ground_station() {
        return not_ground_station();
    }
    Json(json!({"uplink": uplink_view(&state.run_dir)})).into_response()
}

/// The guard every phone request passes, process-wide, loaded from
/// [`sign::replay_path`] on first use.
static REPLAY: parking_lot::Mutex<Option<sign::ReplayGuard>> = parking_lot::Mutex::new(None);

/// Authenticate a phone's `take` / `give_back`: the fingerprint of the
/// approved phone that signed it.
pub fn admit_phone(
    approved: &[phones::ApprovedPhone],
    headers: &HeaderMap,
    body: &[u8],
    now_ms: i64,
    guard: &mut sign::ReplayGuard,
) -> Result<String, sign::MacError> {
    let header = |name: &str| headers.get(name).and_then(|v| v.to_str().ok());
    let (Some(phone_key), Some(ts), Some(mac)) = (
        header(sign::PHONE_KEY_HEADER),
        header(sign::TS_HEADER).and_then(|t| t.trim().parse::<i64>().ok()),
        header(sign::MAC_HEADER),
    ) else {
        return Err(sign::MacError::Missing);
    };
    let phone = phones::by_pub(approved, phone_key).ok_or(sign::MacError::UnknownPhone)?;
    let key = phone.uplink_key().ok_or(sign::MacError::UnknownPhone)?;
    if !sign::verify(&key, ts, body, mac) {
        return Err(sign::MacError::Mismatch);
    }
    guard.admit(&phone.fingerprint, ts, now_ms)?;
    Ok(phone.fingerprint.clone())
}

/// `POST /api/v1/ground-station/uplink`.
pub async fn post_uplink(
    peer: Option<Extension<PeerAddr>>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    if !is_ground_station() {
        return not_ground_station();
    }
    let req: UplinkRequest = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(_) => {
            return refusal(
                StatusCode::BAD_REQUEST,
                "E_BAD_REQUEST",
                "The body must be JSON with an op of take, give_back or take_back.",
            )
        }
    };
    match req.op.as_str() {
        "take" | "give_back" => {
            let approved = phones::load(&phones::phones_path());
            let admitted = {
                let mut guard = REPLAY.lock();
                admit_phone(
                    &approved,
                    &headers,
                    &body,
                    now_ms(),
                    guard.get_or_insert_with(|| sign::ReplayGuard::load(sign::replay_path())),
                )
            };
            let fingerprint = match admitted {
                Ok(f) => f,
                Err(e) => {
                    tracing::warn!(op = %req.op, reason = ?e, "uplink_request_refused");
                    return refusal(e.status(), e.code(), e.detail());
                }
            };
            // A bind stops the radio service, so its own E_BIND_ACTIVE
            // refusal never answers: read the bind sentinel here.
            if req.op == "take" && ados_supervisor::bind::orchestrator::read_sentinel_active() {
                return refusal(
                    status_for("E_BIND_ACTIVE"),
                    "E_BIND_ACTIVE",
                    "A bind session is running on this ground station, and its radios belong to the bind.",
                );
            }
            let peer_ip = peer.map(|Extension(PeerAddr(a))| a.ip());
            let command = phone_command(&req, &fingerprint, peer_ip);
            reply_from(groundlink_cmd_roundtrip(&command).await)
        }
        "take_back" => take_back().await,
        _ => refusal(
            StatusCode::BAD_REQUEST,
            "E_BAD_REQUEST",
            "The op must be take, give_back or take_back.",
        ),
    }
}

/// The command-socket request for an authenticated phone.
pub fn phone_command(
    req: &UplinkRequest,
    fingerprint: &str,
    peer_ip: Option<std::net::IpAddr>,
) -> Value {
    if req.op == "give_back" {
        return json!({
            "op": "uplink_give_back",
            "holder_device_id": req.holder_device_id,
            "phone_key": fingerprint,
        });
    }
    let yield_url = match (peer_ip, req.yield_port) {
        (Some(ip), Some(port)) if port != 0 => Some(sign::yield_url(ip, port)),
        _ => None,
    };
    json!({
        "op": "uplink_take",
        "holder_device_id": req.holder_device_id,
        "holder_name": req.holder_name,
        "phone_key": fingerprint,
        "yield_url": yield_url,
    })
}

/// Transition 3: tell the holder first, wait up to [`sign::YIELD_TIMEOUT`] for
/// it to stop, then transmit.
async fn take_back() -> Response {
    let Some(status) = groundlink_cmd_roundtrip(&json!({"op": "uplink_status"})).await else {
        return reply_from(None);
    };
    let mut phone_confirmed = false;
    if status.get("state").and_then(Value::as_str) == Some("standby") {
        let url = status.get("yield_url").and_then(Value::as_str);
        let fingerprint = status.get("holder_phone_key").and_then(Value::as_str);
        let key = fingerprint.and_then(|f| {
            phones::by_fingerprint(&phones::load(&phones::phones_path()), f)
                .and_then(phones::ApprovedPhone::uplink_key)
        });
        if let (Some(url), Some(key)) = (url, key) {
            let box_id = crate::config::PairingConfig::load().agent.device_id;
            phone_confirmed = sign::call_yield(url, &key, &box_id, now_ms()).await;
        }
        tracing::info!(phone_confirmed, "uplink_take_back_requested");
    }
    reply_from(
        groundlink_cmd_roundtrip(
            &json!({"op": "uplink_take_back", "phone_confirmed": phone_confirmed}),
        )
        .await,
    )
}
