//! The PIC confirm-token and heartbeat routes.

use super::*;

/// The `pic/confirm-token` request body: a required `client_id`.
#[derive(Debug, Deserialize)]
pub struct PicConfirmTokenRequest {
    pub client_id: String,
}

/// `POST /api/v1/ground-station/pic/confirm-token` → `{token, ttl_seconds}`.
///
/// `404 E_PROFILE_MISMATCH` off a ground station. Otherwise forwards a
/// `confirm_token` op and returns `{token: <32-hex>, ttl_seconds: 2}` — the
/// arbiter mints a plain string token, and the route reports a fixed
/// `ttl_seconds: 2`. An unreachable
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
        PicReply::Obj(reply) => match reply.get("token").and_then(Value::as_str) {
            // The arbiter's confirm token lives two seconds.
            Some(token) if !token.is_empty() => {
                Json(json!({"token": token, "ttl_seconds": 2})).into_response()
            }
            // No token was minted: an empty token would read as a usable one.
            _ => pic_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "E_PIC_TOKEN_FAILED",
                "the PIC arbiter did not mint a token",
            ),
        },
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

/// The `pic/heartbeat` request body: a required `client_id`.
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
/// false and the route returns a `410` carrying the
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
        PicReply::Obj(reply) if heartbeat_ok(&reply) => {
            let mut body = heartbeat_ok_body(&reply);
            attach_injector_ticket(&state, &mut body);
            Json(body).into_response()
        }
        PicReply::Obj(reply) => heartbeat_response(&reply),
        PicReply::Unavailable => pic_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "E_PIC_HEARTBEAT_FAILED",
            "PIC control socket unavailable",
        ),
    }
}

/// Map the daemon's `heartbeat` reply to the route response. The reply's
/// `ok_heartbeat` flag is the arbiter's `ok`: when true the route returns the
/// `{ok:true, claimed_by, claim_counter, last_heartbeat_ts}` dict at 200; when
/// false the route returns `(status, {"detail": {"error":
/// {"code":"E_PIC_NO_ACTIVE_CLAIM","message":<error>,"current_pic":<cp>}}})` — the
/// status from the reply (410), the error object carrying the reply's `error`
/// string and the current holder.
pub(super) fn heartbeat_response(reply: &Map<String, Value>) -> Response {
    if heartbeat_ok(reply) {
        return Json(heartbeat_ok_body(reply)).into_response();
    }
    // The not-holder path: the 410 with the error object.
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

/// Whether the arbiter accepted the heartbeat.
pub(super) fn heartbeat_ok(reply: &Map<String, Value>) -> bool {
    reply
        .get("ok_heartbeat")
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

/// The arbiter `heartbeat` ok dict: ok / claimed_by / claim_counter /
/// last_heartbeat_ts, in that insertion order.
pub(super) fn heartbeat_ok_body(reply: &Map<String, Value>) -> Value {
    json!({
        "ok": true,
        "claimed_by": reply.get("claimed_by").cloned().unwrap_or(Value::Null),
        "claim_counter": reply.get("claim_counter").cloned().unwrap_or(json!(0)),
        "last_heartbeat_ts": reply.get("last_heartbeat_ts").cloned().unwrap_or(Value::Null),
    })
}
