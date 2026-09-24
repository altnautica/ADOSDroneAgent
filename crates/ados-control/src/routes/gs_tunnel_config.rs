//! Ground-station config-over-radio (relayed-config) routes.
//!
//! The seam the GCS (a later lane) calls to read or write a radio-linked
//! drone's `/api/config` when the drone is reachable only over the WFB link
//! (WFB carries no IP). The ground node's `ados-tunnel-config` injector owns
//! the bearer; this route is a thin client that forwards one request over its
//! command socket and returns the reassembled reply, exactly like the sibling
//! `gs_crsf` / `gs_mesh_write` routes forward to their data-plane services.
//!
//! - **`GET /api/v1/ground-station/relayed/config`** — the channel's state
//!   sidecar (`tunnel-config.json`), staleness-gated: absent / stale /
//!   malformed all read `404`, never a stale reading served as current.
//! - **`POST /api/v1/ground-station/relayed/config`** — forward a config
//!   request (`{"request":{"op":"get"|"put",…},"timeout_ms":…}`) to the drone
//!   over the bearer and return its reply.
//!
//! ## Error posture
//!
//! Profile gate first (`404 E_PROFILE_MISMATCH` off a ground station). An
//! unreachable command socket is a `503` (the channel is opt-in and only
//! serves its socket while enabled). A gated refusal (`E_TUNNEL_DISABLED` /
//! `E_WRITE_DISABLED`) is a `409`; a bearer timeout (`E_TIMEOUT`) is a `504`;
//! any other daemon-side `ok:false` is a `400` carrying the `E_*` code. A
//! successful relay is a `200` carrying `{is_error, response}` — `is_error`
//! honestly marks whether the drone returned an error envelope.
//!
//! The drone refuses any request that does not carry a relay ticket minted
//! from the per-pair secret for that drone, because the radio key is shared by
//! the whole fleet. The route mints it for `device_id` (or, when the body names
//! none, the one registered drone) and adds it to the forwarded op as `ticket`.
//! With no target it can name, the route refuses with `400`.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::routes::detail;
use crate::state::AppState;

/// The channel's state sidecar filename under the run dir.
const SIDECAR_FILE: &str = "tunnel-config.json";
/// The injector's command socket filename under the run dir.
const CMD_SOCK_FILE: &str = "tunnel-config-cmd.sock";
/// How stale the sidecar may be before it reads as absent (the service
/// rewrites it ~1 Hz running / ~5 s idling).
const STALE_AFTER: Duration = Duration::from_secs(30);

fn run_dir() -> PathBuf {
    PathBuf::from(std::env::var("ADOS_RUN_DIR").unwrap_or_else(|_| "/run/ados".to_string()))
}

fn is_ground_station(state: &AppState) -> bool {
    let cfg = crate::config::PairingConfig::load_from(&state.pairing_paths.config);
    let (profile, _role) = crate::profile::current_profile_and_role(&cfg.agent.profile);
    profile == "ground-station"
}

fn profile_mismatch() -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(json!({"detail": {"error": {"code": "E_PROFILE_MISMATCH"}}})),
    )
        .into_response()
}

// ---------------------------------------------------------------------------
// GET /api/v1/ground-station/relayed/config
// ---------------------------------------------------------------------------

pub async fn get_relayed_config_status(State(state): State<AppState>) -> Response {
    if !is_ground_station(&state) {
        return profile_mismatch();
    }
    read_status(&run_dir().join(SIDECAR_FILE), SystemTime::now())
}

fn read_status(path: &Path, now: SystemTime) -> Response {
    let Ok(meta) = std::fs::metadata(path) else {
        return status_not_found();
    };
    if let Ok(modified) = meta.modified() {
        if let Ok(age) = now.duration_since(modified) {
            if age > STALE_AFTER {
                return status_not_found();
            }
        }
    }
    let Ok(text) = std::fs::read_to_string(path) else {
        return status_not_found();
    };
    let Ok(doc) = serde_json::from_str::<Value>(&text) else {
        return status_not_found();
    };
    if !doc.is_object() {
        return status_not_found();
    }
    let version = doc.get("v").and_then(Value::as_u64).unwrap_or(0) as u16;
    let expected = ados_protocol::contracts::sidecar_version("tunnel-config").unwrap_or(0);
    ados_protocol::sidecar::check_sidecar_version("tunnel-config", version, expected);
    (StatusCode::OK, Json(doc)).into_response()
}

fn status_not_found() -> Response {
    detail(
        StatusCode::NOT_FOUND,
        "config-over-radio channel not running",
    )
}

// ---------------------------------------------------------------------------
// POST /api/v1/ground-station/relayed/config
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct RelayedConfigRequest {
    /// The config op to relay: `{"op":"get"}` or
    /// `{"op":"put","key":"…","value":"…"}`.
    request: Value,
    /// The drone the ticket is minted for. Optional only while exactly one
    /// drone is registered.
    #[serde(default)]
    device_id: Option<String>,
    /// Per-request deadline in milliseconds; the daemon clamps it.
    #[serde(default)]
    timeout_ms: Option<u64>,
}

pub async fn post_relayed_config(
    State(state): State<AppState>,
    Json(body): Json<RelayedConfigRequest>,
) -> Response {
    if !is_ground_station(&state) {
        return profile_mismatch();
    }
    if !body.request.is_object() {
        return detail(
            StatusCode::BAD_REQUEST,
            "request must be a config op object",
        );
    }
    let slots = crate::routes::gs_fleet_slot::registered_slots();
    let Some(target) = ticket_target(body.device_id.as_deref(), &slots) else {
        return detail(
            StatusCode::BAD_REQUEST,
            "device_id is required when more than one drone is registered",
        );
    };
    let mut request = body.request;
    request["ticket"] = json!(crate::routes::gs_fleet_slot::mint_ticket(&slots, &target));
    let mut forward = json!({
        "op": "config_request",
        "request": request,
    });
    if let Some(ms) = body.timeout_ms {
        forward["timeout_ms"] = json!(ms);
    }
    match cmd_roundtrip(&forward).await {
        Some(reply) => map_cmd_reply(&reply),
        // The injector is not serving its socket: the channel is opt-in and
        // only listens while enabled. Honest 503, not a fabricated success.
        None => detail(
            StatusCode::SERVICE_UNAVAILABLE,
            "config-over-radio channel not available",
        ),
    }
}

/// The drone a relayed config request's ticket is minted for: the named one,
/// or the only registered drone when none is named.
fn ticket_target(named: Option<&str>, slots: &[ados_groundlink::FleetSlot]) -> Option<String> {
    match named.map(str::trim).filter(|s| !s.is_empty()) {
        Some(id) => Some(id.to_string()),
        None => match slots {
            [only] => Some(only.device_id.clone()),
            _ => None,
        },
    }
}

/// Map the daemon's command-socket reply to an HTTP response. Pure.
fn map_cmd_reply(reply: &Value) -> Response {
    if reply.get("ok").and_then(Value::as_bool) == Some(true) {
        // A successful relay: return the drone's reply verbatim (is_error marks
        // whether the drone itself returned an error envelope).
        return (StatusCode::OK, Json(reply.clone())).into_response();
    }
    let code = reply
        .get("error")
        .and_then(Value::as_str)
        .unwrap_or("E_UNKNOWN");
    let status = match code {
        "E_TUNNEL_DISABLED" | "E_WRITE_DISABLED" => StatusCode::CONFLICT,
        "E_TIMEOUT" => StatusCode::GATEWAY_TIMEOUT,
        "E_BEARER_SEND_FAILED" => StatusCode::BAD_GATEWAY,
        _ => StatusCode::BAD_REQUEST,
    };
    (status, Json(json!({"detail": {"error": {"code": code}}}))).into_response()
}

/// Send one newline-JSON request to the injector's command socket and read one
/// newline-JSON reply. `None` on any transport failure so the caller takes its
/// 503 no-link posture.
async fn cmd_roundtrip(request: &Value) -> Option<Value> {
    /// The injector bounds the radio round trip at 30 s at most, then replies;
    /// the front waits a little longer so it hears that reply.
    const TUNNEL_CMD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(35);
    crate::ipc::cmd::roundtrip_object(&run_dir().join(CMD_SOCK_FILE), request, TUNNEL_CMD_TIMEOUT)
        .await
        .ok()
        .map(Value::Object)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;

    async fn status_of(resp: Response) -> (StatusCode, Value) {
        let status = resp.status();
        let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (status, body)
    }

    #[tokio::test]
    async fn successful_relay_is_200_with_the_reply() {
        let reply = json!({"ok": true, "is_error": false, "response": {"radio": {}}});
        let (status, body) = status_of(map_cmd_reply(&reply)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["is_error"], false);
    }

    #[tokio::test]
    async fn a_drone_error_envelope_still_relays_200() {
        // The relay SUCCEEDED (the drone answered); the drone's own error is
        // carried in is_error, not an HTTP failure.
        let reply =
            json!({"ok": true, "is_error": true, "response": {"error": "E_WRITE_DISABLED"}});
        let (status, body) = status_of(map_cmd_reply(&reply)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["is_error"], true);
    }

    #[tokio::test]
    async fn gated_and_timeout_refusals_map_to_honest_statuses() {
        for (code, want) in [
            ("E_TUNNEL_DISABLED", StatusCode::CONFLICT),
            ("E_WRITE_DISABLED", StatusCode::CONFLICT),
            ("E_TIMEOUT", StatusCode::GATEWAY_TIMEOUT),
            ("E_BEARER_SEND_FAILED", StatusCode::BAD_GATEWAY),
            ("E_MISSING_REQUEST", StatusCode::BAD_REQUEST),
        ] {
            let reply = json!({"ok": false, "error": code});
            let (status, body) = status_of(map_cmd_reply(&reply)).await;
            assert_eq!(status, want, "code {code}");
            assert_eq!(body["detail"]["error"]["code"], code);
        }
    }

    #[test]
    fn stale_or_absent_sidecar_reads_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(SIDECAR_FILE);
        // Absent.
        let resp = read_status(&path, SystemTime::now());
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        // Present but stale.
        std::fs::write(&path, br#"{"v":1,"state":"injector"}"#).unwrap();
        let future = SystemTime::now() + STALE_AFTER + Duration::from_secs(5);
        assert_eq!(read_status(&path, future).status(), StatusCode::NOT_FOUND);
        // Fresh.
        assert_eq!(
            read_status(&path, SystemTime::now()).status(),
            StatusCode::OK
        );
    }

    fn slot(n: u8, id: &str, secret: &str) -> ados_groundlink::FleetSlot {
        ados_groundlink::FleetSlot {
            slot: n,
            device_id: id.to_string(),
            paired_at_ms: 0,
            relay_secret: Some(secret.to_string()),
        }
    }

    #[test]
    fn the_ticket_names_the_addressed_drone_and_verifies_there() {
        const SECRET: &str = "0123456789abcdef0123456789abcdef";
        let one = vec![slot(1, "d1", SECRET)];
        assert_eq!(ticket_target(None, &one).as_deref(), Some("d1"));
        let two = vec![
            slot(1, "d1", SECRET),
            slot(2, "d2", "ffffffffffffffffffffffffffffffff"),
        ];
        assert_eq!(
            ticket_target(None, &two),
            None,
            "with two drones the target must be named, never guessed"
        );
        assert_eq!(ticket_target(Some("d1"), &two).as_deref(), Some("d1"));

        let ticket = crate::routes::gs_fleet_slot::mint_ticket(&two, "d1");
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        ados_protocol::relay_ticket::RelayTicketIssuer::from_secret(SECRET.as_bytes())
            .verify(&ticket, "d1", now)
            .expect("d1 admits a ticket minted from its own secret");
    }
}
