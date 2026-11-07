//! One `WS /api/v1/pilot/stream` connection: JSON client frames to the pilot
//! socket, pilot messages back as JSON.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use ados_protocol::pilot;
use axum::extract::ws::{Message, WebSocket};
use serde_json::{json, Value};
use tokio::net::UnixStream;
use tokio::sync::mpsc;

use crate::routes::gs_ws::{client_still_open, send_json};

/// Nothing sent for this long: send a keepalive.
const KEEPALIVE: Duration = Duration::from_secs(1);

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// The JSON frame a pilot message becomes, or `None` for messages a surface
/// does not see (sticks, lease events, obstacles: those reach surfaces through
/// the nav stream or the plugin host).
pub fn to_client(op: &str, body: rmpv::Value) -> Option<Value> {
    let mut v = serde_json::to_value(&body).ok()?;
    if let Some(m) = v.as_object_mut() {
        m.remove("op");
    }
    match op {
        "state" => Some(json!({"kind": "pilot", "ts_ms": now_ms(), "data": v})),
        "action_result" => {
            let mut out = json!({"kind": "action_result"});
            if let (Some(o), Some(src)) = (out.as_object_mut(), v.as_object()) {
                o.extend(src.clone());
            }
            Some(out)
        }
        _ => None,
    }
}

/// A rejected action, answered without reaching the pilot.
fn action_error(frame: &Value, err: &str) -> Option<Value> {
    (frame.get("op").and_then(Value::as_str) == Some("action")).then(|| {
        json!({"kind": "action_result", "name": frame.get("name").cloned().unwrap_or(Value::Null),
               "id": frame.get("id").cloned().unwrap_or(Value::Null), "ok": false, "err": err})
    })
}

/// Ops a surface may send. Lease, goal, swarm, feed, view and setup ops belong
/// to on-box services, not to an operator socket.
const SURFACE_OPS: &[&str] = &["input", "release", "session.take", "hb", "action"];

pub(super) async fn pilot_stream_loop(mut socket: WebSocket, path: PathBuf) {
    let stream = match UnixStream::connect(&path).await {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(error = %e, "pilot_stream_no_pilot");
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
    let (to_pilot, mut to_pilot_rx) = mpsc::channel::<Vec<u8>>(128);
    let (from_pilot, mut from_pilot_rx) = mpsc::channel::<Value>(128);
    let writer = tokio::spawn(async move {
        while let Some(b) = to_pilot_rx.recv().await {
            if pilot::write_msg(&mut wr, &b).await.is_err() {
                break;
            }
        }
    });
    let reader = tokio::spawn(async move {
        while let Ok(Some(body)) = pilot::read_msg(&mut rd).await {
            let Ok((op, v)) = pilot::peek_op(&body) else {
                continue;
            };
            if let Some(frame) = to_client(&op, v) {
                if from_pilot.send(frame).await.is_err() {
                    break;
                }
            }
        }
    });
    let _ = to_pilot
        .send(pilot::encode_op("subscribe", &()).unwrap_or_default())
        .await;
    let mut last_sent = Instant::now();
    let mut tick = tokio::time::interval(Duration::from_millis(250));
    loop {
        tokio::select! {
            incoming = socket.recv() => {
                if !client_still_open(&incoming) {
                    break;
                }
                let Some(Ok(Message::Text(text))) = incoming else { continue };
                let Ok(frame) = serde_json::from_str::<Value>(&text) else { continue };
                let op = frame.get("op").and_then(Value::as_str).unwrap_or("");
                if !SURFACE_OPS.contains(&op) {
                    if let Some(e) = action_error(&frame, "unknown_op") {
                        let _ = send_json(&mut socket, &e).await;
                    }
                    continue;
                }
                match pilot::json_to_request_body(&frame) {
                    Ok((_, body)) => {
                        if to_pilot.send(body).await.is_err() {
                            break;
                        }
                    }
                    Err(e) => {
                        tracing::info!(error = %e, op, "pilot_stream_frame_rejected");
                        if let Some(r) = action_error(&frame, "malformed") {
                            if !send_json(&mut socket, &r).await { break; }
                        }
                    }
                }
            }
            frame = from_pilot_rx.recv() => {
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
    writer.abort();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_becomes_a_pilot_frame_and_action_results_keep_their_fields() {
        let state = rmpv::ext::to_value(json!({"op": "state", "owner": "pilot"})).unwrap();
        let f = to_client("state", state).unwrap();
        assert_eq!(f["kind"], "pilot");
        assert_eq!(f["data"]["owner"], "pilot");
        assert!(f["data"].get("op").is_none());
        let res = rmpv::ext::to_value(
            json!({"op": "action_result", "name": "take_control", "id": "x", "ok": true}),
        )
        .unwrap();
        let f = to_client("action_result", res).unwrap();
        assert_eq!(
            f,
            json!({"kind": "action_result", "name": "take_control", "id": "x", "ok": true})
        );
        assert!(to_client("sticks", rmpv::Value::Nil).is_none());
    }

    #[test]
    fn rejected_actions_are_answered() {
        let frame = json!({"op": "action", "name": "arm", "id": "1"});
        assert_eq!(
            action_error(&frame, "malformed").unwrap()["err"],
            "malformed"
        );
        assert!(action_error(&json!({"op": "input"}), "malformed").is_none());
    }
}
