//! Flight-controller setup and tuning, served by the pilot daemon's setup
//! engine over `pilot.sock` (`setup.*` ops).
//!
//! * `GET  /api/v1/fc-setup`: the wizards, their steps and the setup report.
//! * `POST /api/v1/fc-setup/{wizard}/start`
//! * `POST /api/v1/fc-setup/{wizard}/steps/{step}/actions/{action}` (`{args}`)
//! * `POST /api/v1/fc-setup/{wizard}/steps/{step}/run`
//! * `POST /api/v1/fc-setup/{wizard}/abort`
//! * `POST /api/v1/fc-setup/revert` (`{snapshot?}`)
//! * `WS   /api/v1/fc-setup/stream` (ticket scope [`SCOPE_FC_SETUP`]): `{kind:
//!   "step"|"wizard", ts_ms, data}` while a wizard runs, `{kind:"keepalive",
//!   ts_ms}` once a second otherwise.
//!
//! The REST routes take the pairing key like every other write; MCP tokens
//! cannot reach them (the setup flies the aircraft and spins motors).

use std::time::{Duration, Instant};

use ados_protocol::pairing_posture::CallerClass;
use ados_protocol::pilot;
use axum::body::Bytes;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Extension, Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};
use tokio::net::UnixStream;

use crate::routes::gs_ws::{
    caller_of, client_still_open, decide_ws_auth, send_json, upgrade_with, ws_reject,
};
use crate::routes::pilot::pilot_socket;
use crate::state::AppState;

/// The ticket scope a `/api/v1/fc-setup/stream` upgrade must carry.
pub const SCOPE_FC_SETUP: &str = "fc.setup";

/// How long a `setup.*` request may take (a wizard start downloads nothing:
/// the engine already holds the parameter table).
const REPLY_TIMEOUT: Duration = Duration::from_secs(5);
const KEEPALIVE: Duration = Duration::from_secs(1);

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// One request/reply on the pilot socket.
async fn request(op: &str, body: Value) -> std::io::Result<Value> {
    let stream = UnixStream::connect(pilot_socket()).await?;
    let (mut rd, mut wr) = stream.into_split();
    let bytes = pilot::encode_op(op, &body).map_err(std::io::Error::other)?;
    pilot::write_msg(&mut wr, &bytes)
        .await
        .map_err(std::io::Error::other)?;
    let wait = async {
        loop {
            let Some(b) = pilot::read_msg(&mut rd)
                .await
                .map_err(std::io::Error::other)?
            else {
                return Err(std::io::Error::other("pilot closed"));
            };
            if let Ok((rop, v)) = pilot::peek_op(&b) {
                if rop == op {
                    let mut v = serde_json::to_value(&v).map_err(std::io::Error::other)?;
                    if let Some(m) = v.as_object_mut() {
                        m.remove("op");
                    }
                    return Ok(v);
                }
            }
        }
    };
    tokio::time::timeout(REPLY_TIMEOUT, wait)
        .await
        .map_err(|_| std::io::Error::other("pilot timeout"))?
}

/// HTTP status for an engine error code.
pub fn status_for(err: &str) -> StatusCode {
    match err {
        "bad_args" | "bad_wizard" | "unknown_step" | "unknown_action" | "unknown_op"
        | "unsupported" | "unsupported_frame" => StatusCode::UNPROCESSABLE_ENTITY,
        "setup_unavailable" => StatusCode::SERVICE_UNAVAILABLE,
        "no_snapshot" => StatusCode::NOT_FOUND,
        _ => StatusCode::CONFLICT,
    }
}

fn respond(reply: std::io::Result<Value>) -> Response {
    match reply {
        Ok(v) => {
            let err = (v.get("ok") == Some(&Value::Bool(false))).then(|| {
                v.get("err")
                    .and_then(Value::as_str)
                    .unwrap_or("refused")
                    .to_string()
            });
            match err {
                Some(e) => (status_for(&e), Json(v)).into_response(),
                None => (StatusCode::OK, Json(v)).into_response(),
            }
        }
        Err(e) => {
            tracing::warn!(error = %e, "fc_setup_pilot_unreachable");
            crate::routes::detail(StatusCode::SERVICE_UNAVAILABLE, "ADOS pilot not reachable")
        }
    }
}

fn valid_wizard(w: &str) -> bool {
    matches!(w, "ground" | "air")
}

fn bad_wizard() -> Response {
    (
        StatusCode::UNPROCESSABLE_ENTITY,
        Json(json!({"ok": false, "err": "bad_wizard"})),
    )
        .into_response()
}

fn body_json(body: &Bytes) -> Value {
    if body.is_empty() {
        return json!({});
    }
    serde_json::from_slice(body).unwrap_or(json!({}))
}

/// `GET /api/v1/fc-setup`
pub async fn get_fc_setup() -> Response {
    respond(request("setup.list", json!({})).await)
}

/// `POST /api/v1/fc-setup/{wizard}/start`
pub async fn post_start(Path(wizard): Path<String>) -> Response {
    if !valid_wizard(&wizard) {
        return bad_wizard();
    }
    respond(request("setup.start", json!({"wizard": wizard})).await)
}

/// `POST /api/v1/fc-setup/{wizard}/abort`
pub async fn post_abort(Path(wizard): Path<String>) -> Response {
    if !valid_wizard(&wizard) {
        return bad_wizard();
    }
    respond(request("setup.abort", json!({"wizard": wizard})).await)
}

/// `POST /api/v1/fc-setup/{wizard}/steps/{step}/run`
pub async fn post_run_step(Path((wizard, step)): Path<(String, String)>) -> Response {
    if !valid_wizard(&wizard) {
        return bad_wizard();
    }
    respond(request("setup.run_step", json!({"wizard": wizard, "step": step})).await)
}

/// `POST /api/v1/fc-setup/{wizard}/steps/{step}/actions/{action}`
pub async fn post_action(
    Path((wizard, step, action)): Path<(String, String, String)>,
    body: Bytes,
) -> Response {
    if !valid_wizard(&wizard) {
        return bad_wizard();
    }
    let args = body_json(&body).get("args").cloned().unwrap_or(json!({}));
    if !args.is_object() {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({"ok": false, "err": "bad_args"})),
        )
            .into_response();
    }
    respond(
        request(
            "setup.action",
            json!({"wizard": wizard, "step": step, "action": action, "args": args}),
        )
        .await,
    )
}

/// `POST /api/v1/fc-setup/revert`
pub async fn post_revert(body: Bytes) -> Response {
    let snapshot = body_json(&body)
        .get("snapshot")
        .cloned()
        .unwrap_or(Value::Null);
    respond(request("setup.revert", json!({"snapshot": snapshot})).await)
}

/// `WS /api/v1/fc-setup/stream`
pub async fn ws_fc_setup_stream(
    State(state): State<AppState>,
    headers: HeaderMap,
    caller: Option<Extension<CallerClass>>,
    ws: WebSocketUpgrade,
) -> Response {
    let auth = decide_ws_auth(&state, &headers, SCOPE_FC_SETUP, caller_of(caller));
    let Some((ws, _auth)) = upgrade_with(ws, auth) else {
        return ws_reject();
    };
    ws.on_upgrade(stream_loop)
}

/// A setup-stream message from the pilot as a client frame.
pub fn to_client(op: &str, body: rmpv::Value) -> Option<Value> {
    if op != "setup.stream" {
        return None;
    }
    let mut v = serde_json::to_value(&body).ok()?;
    v.as_object_mut()?.remove("op");
    Some(v)
}

async fn stream_loop(mut socket: WebSocket) {
    let stream = match UnixStream::connect(pilot_socket()).await {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(error = %e, "fc_setup_stream_no_pilot");
            let _ = send_json(
                &mut socket,
                &json!({"kind": "error", "err": "pilot_unavailable"}),
            )
            .await;
            let _ = socket.send(Message::Close(None)).await;
            return;
        }
    };
    let (mut rd, mut wr) = stream.into_split();
    let Ok(sub) = pilot::encode_op("setup.subscribe", &json!({})) else {
        return;
    };
    if pilot::write_msg(&mut wr, &sub).await.is_err() {
        return;
    }
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Value>(64);
    let reader = tokio::spawn(async move {
        while let Ok(Some(body)) = pilot::read_msg(&mut rd).await {
            let Ok((op, v)) = pilot::peek_op(&body) else {
                continue;
            };
            if let Some(frame) = to_client(&op, v) {
                if tx.send(frame).await.is_err() {
                    break;
                }
            }
        }
    });
    let mut last_sent = Instant::now();
    let mut tick = tokio::time::interval(Duration::from_millis(250));
    loop {
        tokio::select! {
            incoming = socket.recv() => {
                if !client_still_open(&incoming) {
                    break;
                }
            }
            frame = rx.recv() => {
                let Some(frame) = frame else {
                    let _ = send_json(&mut socket, &json!({"kind": "error", "err": "pilot_unavailable"})).await;
                    break;
                };
                if !send_json(&mut socket, &frame).await {
                    break;
                }
                last_sent = Instant::now();
            }
            _ = tick.tick() => {
                if last_sent.elapsed() >= KEEPALIVE {
                    if !send_json(&mut socket, &json!({"kind": "keepalive", "ts_ms": now_ms()})).await {
                        break;
                    }
                    last_sent = Instant::now();
                }
            }
        }
    }
    reader.abort();
    drop(wr);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn engine_errors_map_to_http_statuses() {
        assert_eq!(status_for("bad_args"), StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(status_for("wizard_running"), StatusCode::CONFLICT);
        assert_eq!(status_for("armed"), StatusCode::CONFLICT);
        assert_eq!(
            status_for("setup_unavailable"),
            StatusCode::SERVICE_UNAVAILABLE
        );
        assert_eq!(status_for("no_snapshot"), StatusCode::NOT_FOUND);
    }

    #[test]
    fn only_stream_frames_reach_the_client() {
        let f = rmpv::ext::to_value(
            json!({"op": "setup.stream", "kind": "step", "ts_ms": 1, "data": {"step": "accel"}}),
        )
        .unwrap();
        let v = to_client("setup.stream", f).unwrap();
        assert_eq!(
            v,
            json!({"kind": "step", "ts_ms": 1, "data": {"step": "accel"}})
        );
        assert!(to_client("state", rmpv::Value::Nil).is_none());
    }

    #[test]
    fn request_bodies_parse_leniently() {
        assert_eq!(body_json(&Bytes::new()), json!({}));
        assert_eq!(
            body_json(&Bytes::from_static(b"{\"args\":{\"cells\":4}}"))["args"]["cells"],
            json!(4)
        );
        assert_eq!(body_json(&Bytes::from_static(b"not json")), json!({}));
    }
}
