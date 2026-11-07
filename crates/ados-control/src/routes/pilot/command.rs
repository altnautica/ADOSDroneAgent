//! `POST /api/command` in `ados` control mode: the flight primitives become
//! pilot actions, so they go through the pilot's readiness checks and owner
//! rules instead of reaching the flight controller behind its back.

use std::time::Duration;

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};
use tokio::net::UnixStream;

use crate::state::AppState;

/// The pilot's state older than this does not decide the routing.
const STATE_FRESH_MS: u64 = 2000;
/// How long to wait for the pilot's `action_result`.
const REPLY_TIMEOUT: Duration = Duration::from_secs(3);

/// The pilot action a `/api/command` name maps to, if any.
pub fn action_for(cmd: &str) -> Option<&'static str> {
    Some(match cmd {
        "arm" => "arm",
        "disarm" => "disarm",
        "takeoff" => "takeoff",
        "land" => "land",
        "rtl" => "rth",
        "pausemission" => "pause",
        "resumemission" => "resume",
        "killswitch" => "kill",
        _ => return None,
    })
}

/// The live control mode from the nav hub's `pilot` kind, when fresh.
fn live_control_mode(state: &AppState) -> Option<String> {
    let status = state.nav.status_body_for(None);
    let pilot = status.get("pilot")?;
    let age = pilot.get("age_ms").and_then(Value::as_u64)?;
    if age > STATE_FRESH_MS {
        return None;
    }
    pilot
        .get("data")?
        .get("control_mode")?
        .as_str()
        .map(str::to_owned)
}

/// Route `cmd` through the pilot when the pilot owns motion; `None` keeps the
/// raw COMMAND_LONG path.
pub async fn via_pilot(state: &AppState, cmd: &str) -> Option<Response> {
    let action = action_for(cmd)?;
    if live_control_mode(state).as_deref() != Some("ados") {
        return None;
    }
    Some(match send_action(action).await {
        Ok(res) => {
            let ok = res.get("ok").and_then(Value::as_bool).unwrap_or(false);
            let body = if ok {
                json!({"status": "ok", "cmd": cmd, "via": "pilot", "action": action})
            } else {
                let err = res.get("err").and_then(Value::as_str).unwrap_or("refused");
                json!({"status": "error", "cmd": cmd, "via": "pilot", "action": action,
                       "message": format!("pilot refused {action}: {err}")})
            };
            (StatusCode::OK, Json(body)).into_response()
        }
        Err(e) => {
            tracing::warn!(error = %e, cmd, "pilot_command_failed");
            crate::routes::detail(StatusCode::SERVICE_UNAVAILABLE, "ADOS pilot not reachable")
        }
    })
}

async fn send_action(action: &str) -> std::io::Result<Value> {
    use ados_protocol::pilot;
    let stream = UnixStream::connect(super::pilot_socket()).await?;
    let (mut rd, mut wr) = stream.into_split();
    let req = json!({"op": "action", "name": action, "src": "api", "id": "api"});
    let (_, body) = pilot::json_to_request_body(&req).map_err(std::io::Error::other)?;
    pilot::write_msg(&mut wr, &body)
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
            if let Ok((op, v)) = pilot::peek_op(&b) {
                if op == "action_result" {
                    return serde_json::to_value(&v).map_err(std::io::Error::other);
                }
            }
        }
    };
    tokio::time::timeout(REPLY_TIMEOUT, wait)
        .await
        .map_err(|_| std::io::Error::other("pilot timeout"))?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flight_primitives_map_to_pilot_actions() {
        assert_eq!(action_for("rtl"), Some("rth"));
        assert_eq!(action_for("pausemission"), Some("pause"));
        assert_eq!(action_for("killswitch"), Some("kill"));
        assert_eq!(action_for("mode"), None);
        assert_eq!(action_for("ekfsourceset"), None);
    }
}
