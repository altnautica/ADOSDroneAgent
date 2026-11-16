//! `POST /api/command` through the pilot: in `ados` control mode the flight
//! primitives become pilot actions, so they go through the pilot's readiness
//! checks and owner rules instead of reaching the flight controller behind
//! its back. A take-off is the pilot's `launch` in every control mode while
//! the pilot is live, so every take-off lifts off under the pilot (level on an
//! ArduPilot multirotor) and climbs to the height asked for.

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
        "takeoff" => "launch",
        "land" => "land",
        "rtl" => "rth",
        "pausemission" => "pause",
        "resumemission" => "resume",
        "killswitch" => "kill",
        // A pilot-only action: with no pilot in control the raw command
        // path has no such command and answers 400.
        "missionskip" => "mission_skip",
        // The place class, set from the terminal UI or a planner.
        "placeclass_open" => "place_class:open",
        "placeclass_confined" => "place_class:confined",
        // "I rebalanced it": the pilot forgets the airframe it learned.
        "airframereset" => "airframe_reset",
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

/// Whether `action` goes to the pilot under the live control mode `mode`
/// (`None`: no fresh pilot state). The flight primitives go only while the
/// pilot owns motion (`ados`). A take-off (`launch`), the place class and the
/// airframe reset go in every control mode, so long as the pilot is there:
/// the pilot runs the take-off whoever flies after it, and the others are
/// its ground settings.
fn routes_to_pilot(action: &str, mode: Option<&str>) -> bool {
    match mode {
        None => false,
        Some(m) => {
            m == "ados"
                || action == "launch"
                || action == "airframe_reset"
                || action.starts_with("place_class:")
        }
    }
}

/// The pilot's `launch` args for a take-off: hold at `alt_m` once there, or
/// at the pilot's own launch height for the vehicle when none was asked for.
pub fn launch_args(alt_m: Option<f64>) -> Value {
    match alt_m {
        Some(alt) => json!({"after": "hold", "alt_m": alt}),
        None => json!({"after": "hold"}),
    }
}

/// Route `cmd` (with the pilot action's `args`) through the pilot when the
/// pilot owns motion, or for a take-off or a pilot ground setting; `None`
/// keeps the raw COMMAND_LONG path.
pub async fn via_pilot(state: &AppState, cmd: &str, args: Option<Value>) -> Option<Response> {
    let action = action_for(cmd)?;
    if !routes_to_pilot(action, live_control_mode(state).as_deref()) {
        return None;
    }
    let socket = state.run_dir.join(ados_protocol::paths::PILOT_SOCK);
    Some(match send_action(&socket, action, args).await {
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

async fn send_action(
    socket: &std::path::Path,
    action: &str,
    args: Option<Value>,
) -> std::io::Result<Value> {
    use ados_protocol::pilot;
    let stream = UnixStream::connect(socket).await?;
    let (mut rd, mut wr) = stream.into_split();
    let mut req = json!({"op": "action", "name": action, "src": "api", "id": "api"});
    if let Some(args) = args {
        req["args"] = args;
    }
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
        assert_eq!(action_for("takeoff"), Some("launch"));
        assert_eq!(action_for("pausemission"), Some("pause"));
        assert_eq!(action_for("killswitch"), Some("kill"));
        assert_eq!(action_for("missionskip"), Some("mission_skip"));
        assert_eq!(action_for("placeclass_open"), Some("place_class:open"));
        assert_eq!(
            action_for("placeclass_confined"),
            Some("place_class:confined")
        );
        assert_eq!(action_for("airframereset"), Some("airframe_reset"));
        assert_eq!(action_for("mode"), None);
        assert_eq!(action_for("ekfsourceset"), None);
    }

    #[test]
    fn a_take_off_and_the_airframe_reset_reach_the_pilot_in_every_control_mode() {
        for mode in ["ados", "fc", "gcs"] {
            assert!(routes_to_pilot("launch", Some(mode)), "{mode}");
            assert!(routes_to_pilot("airframe_reset", Some(mode)), "{mode}");
        }
        // No fresh pilot state: the raw path.
        assert!(!routes_to_pilot("launch", None));
        assert_eq!(
            launch_args(Some(5.5)),
            json!({"after": "hold", "alt_m": 5.5})
        );
        assert_eq!(launch_args(None), json!({"after": "hold"}));
    }

    #[test]
    fn the_place_class_reaches_the_pilot_in_every_control_mode() {
        for mode in ["ados", "fc", "gcs"] {
            assert!(
                routes_to_pilot("place_class:confined", Some(mode)),
                "{mode}"
            );
            assert!(routes_to_pilot("place_class:open", Some(mode)), "{mode}");
        }
        // A flight primitive only while the pilot owns motion.
        assert!(routes_to_pilot("rth", Some("ados")));
        assert!(!routes_to_pilot("rth", Some("fc")));
        // No fresh pilot state: the raw path.
        assert!(!routes_to_pilot("place_class:open", None));
    }
}
