//! Flight-controller parameter write route.
//!
//! `POST /api/params/{name}` writes a single FC parameter. The body is
//! `{"value": <number>}`; the route turns it into a MAVLink `PARAM_SET` frame
//! and writes it to `/run/ados/mavlink.sock`, which the router forwards to the
//! FC. ArduPilot saves the value to EEPROM on receipt and echoes a `PARAM_VALUE`
//! back; the route then polls the cached param blob for up to two seconds to
//! confirm the new value landed and reports that as the `ack`.
//!
//! ## Why this is the WORKING write path
//!
//! On the Rust-hybrid agent the FastAPI `params.py` route reaches for the FC
//! connection object, which is `None` on the API process because the native
//! router owns the FC serial link. So the FastAPI route always 503s after its
//! known-param check. This native route is the working replacement: it builds the
//! `PARAM_SET` frame itself and writes it to the same socket the router reads, the
//! socket the Python MAVLink IPC client writes to. The parity target is therefore
//! the MAVLink bytes the FastAPI route's `PARAM_SET` send WOULD have produced, plus
//! the FastAPI route's exact guard order and response shapes.
//!
//! ## Guard order (matches the FastAPI route)
//!
//! 1. The value must be a finite number → 400 `"value must be a finite number"`.
//! 2. The parameter must be one the agent has already observed (present in the
//!    cached param blob) → 404 with the FastAPI message when it is not. This
//!    guards against typos pushing garbage params into the FC.
//! 3. The FC must be connected → 503 `"FC not connected"`.
//! 4. The frame send must succeed → 503 `"FC connection unavailable"` when the
//!    MAVLink socket cannot be reached (the native equivalent of the FastAPI
//!    route's `conn is None` / send-raise paths; the command is never silently
//!    dropped).
//!
//! ## The `PARAM_SET` frame
//!
//! The FastAPI route resolves a per-name `param_type`: it reads the type from the
//! in-process param cache when present, else falls back to `0` for a param it has
//! only seen a value for. The native front sits in front of the standalone API
//! process and holds no in-process param cache with type metadata — its only
//! production-reachable source is the state-IPC snapshot's `params` blob, a
//! `{name: value}` map with no type. So the only reachable known-param path here
//! is the value-only fallback (the FastAPI `known_type = 0` branch). MAVLink's
//! `MAV_PARAM_TYPE` enum has no `0` member (values run `UINT8 = 1 .. REAL64 = 10`),
//! and ArduPilot ignores the field on a `PARAM_SET` — it infers the canonical type
//! from its own param table. The frame therefore carries `MAV_PARAM_TYPE_REAL32`
//! (the float type, the same type the router stamps when it re-emits a
//! `PARAM_VALUE`), which the FC accepts and treats identically.
//!
//! ## Source + target identity
//!
//! The frame is forwarded to the FC verbatim (the router does not re-stamp the
//! header), so the header identity matters. `system_id = 1, component_id = 191`
//! is the agent/companion identity the router uses on its own FC send path, so a
//! write from this surface is wire-identical to one the router sent. The target is
//! the single-vehicle ArduPilot default `1/1`.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};

use ados_protocol::mavlink::ardupilotmega::{MavMessage, MavParamType, PARAM_SET_DATA};
use ados_protocol::mavlink::{self, MavHeader};

use crate::routes::detail;
use crate::state::AppState;

/// The source identity stamped on the write frame: the agent/companion identity
/// the router uses on its own FC send path (defaults 1/191), so a write from this
/// surface is wire-identical to one the router sent.
const SOURCE_SYSTEM_ID: u8 = 1;
const SOURCE_COMPONENT_ID: u8 = 191;

/// The target identity: the single-vehicle ArduPilot default (1/1). The state
/// socket carries no target system, so this surface targets 1/1.
const TARGET_SYSTEM: u8 = 1;
const TARGET_COMPONENT: u8 = 1;

/// The width of a MAVLink `param_id` field: a 16-byte, null-padded ASCII name.
const PARAM_ID_LEN: usize = 16;

/// How long to poll the cached param blob for the FC's `PARAM_VALUE` echo before
/// reporting `ack: false`, matching the FastAPI route's 2-second deadline.
const ACK_POLL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// One poll interval between cache reads while waiting for the echo, matching the
/// FastAPI route's `await asyncio.sleep(0.1)`.
const ACK_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(100);

/// The tolerance the cached echo must land within to count as an `ack`, matching
/// the FastAPI route's `abs(cached_value - target) < 1e-6`.
const ACK_TOLERANCE: f64 = 1e-6;

/// The message the route reports when the FC did not echo a `PARAM_VALUE` within
/// the poll window, byte-identical to the FastAPI route's text.
const NO_ACK_MESSAGE: &str = "FC did not echo PARAM_VALUE within 2s";

/// The `POST /api/params/{name}` request body. Mirrors the FastAPI
/// `ParamSetRequest`: a single required numeric `value` to write to the FC.
#[derive(Debug, Deserialize)]
pub struct ParamSetRequest {
    pub value: f64,
}

/// A 4xx/5xx the write path raises before or instead of sending: a non-finite
/// value (400), an unknown param (404), or no FC link (503). Carries the FastAPI
/// status + detail so it renders as the `{"detail"}` shape.
#[derive(Debug)]
struct ParamError {
    status: StatusCode,
    detail: String,
}

impl IntoResponse for ParamError {
    fn into_response(self) -> Response {
        detail(self.status, self.detail)
    }
}

/// `POST /api/params/{name}` → `{"name", "value", "ack", "cached_value", "message"}`.
///
/// Validates the body and the known-param + FC-connected guards in the FastAPI
/// route's order, builds the `PARAM_SET` frame, writes it to the MAVLink socket,
/// then polls the cached param blob for the FC's echo to set `ack`. Degrades to
/// the documented 4xx/5xx `{"detail"}` bodies on each guard; it never panics on a
/// seam error (an absent MAVLink socket maps to a 503, the same no-link posture as
/// the FastAPI route).
pub async fn set_param(
    Path(name): Path<String>,
    State(state): State<AppState>,
    Json(req): Json<ParamSetRequest>,
) -> Response {
    let target = req.value;

    // 1. The value must be finite (the FastAPI `math.isfinite` guard) → 400.
    if !target.is_finite() {
        return ParamError {
            status: StatusCode::BAD_REQUEST,
            detail: "value must be a finite number".to_string(),
        }
        .into_response();
    }

    // 2. The parameter must be one the agent has already observed. The native
    //    front's only param source is the state-IPC snapshot's `params` blob; a
    //    name absent from it is refused with the FastAPI 404 message.
    let snapshot = state.state.snapshot();
    if !param_known(snapshot.as_ref(), &name) {
        return ParamError {
            status: StatusCode::NOT_FOUND,
            detail: format!(
                "Parameter '{name}' not in cache; agent must observe a \
                 PARAM_VALUE for it before writes are allowed"
            ),
        }
        .into_response();
    }

    // 3. The FC must be connected (the FastAPI `fc.connected` guard) → 503.
    if !state.fc_connected() {
        return ParamError {
            status: StatusCode::SERVICE_UNAVAILABLE,
            detail: "FC not connected".to_string(),
        }
        .into_response();
    }

    // Build the PARAM_SET frame and serialize it with the source identity.
    let msg = build_param_set(&name, target);
    let header = MavHeader {
        system_id: SOURCE_SYSTEM_ID,
        component_id: SOURCE_COMPONENT_ID,
        // The router stamps its own sequence on its frames; for a client-written
        // PARAM_SET the sequence is not load-bearing (ArduPilot does not key off
        // it), so 0 is used, mirroring the fire-and-forget send.
        sequence: 0,
    };
    let frame = match mavlink::serialize_v2(header, &msg) {
        Ok(bytes) => bytes,
        Err(e) => {
            tracing::error!(error = %e, param = %name, "param_set frame serialize failed");
            return detail(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to send PARAM_SET: {e}"),
            );
        }
    };

    // 4. Send the frame. An absent or broken MAVLink socket means no live FC link
    //    from this surface's view → 503 (the native equivalent of the FastAPI
    //    `conn is None` / send-raise paths); the write is never silently dropped.
    if let Err(e) = state.mavlink.send(&frame).await {
        tracing::warn!(error = %e, param = %name, "param_set send to the mavlink socket failed");
        return ParamError {
            status: StatusCode::SERVICE_UNAVAILABLE,
            detail: "FC connection unavailable".to_string(),
        }
        .into_response();
    }

    // Poll the cached param blob for up to two seconds for the FC's PARAM_VALUE
    // echo. The router updates the snapshot's `params` blob as PARAM_VALUE frames
    // arrive; this reads the live snapshot each tick, the native equivalent of the
    // FastAPI route polling its in-process cache.
    let (ack, cached_value) = poll_for_ack(&state, &name, target).await;

    tracing::info!(param = %name, value = target, ack, "param_set");
    Json(build_set_response(&name, target, ack, cached_value)).into_response()
}

/// Whether the agent has already observed `name` (it is present in the state-IPC
/// snapshot's `params` blob). The blob is a `{name: value}` object; a missing
/// blob, a non-object blob, or an absent snapshot all read as "not known",
/// mirroring the FastAPI route's refusal to write a param it has never seen.
fn param_known(snapshot: Option<&Value>, name: &str) -> bool {
    snapshot
        .and_then(Value::as_object)
        .and_then(|m| m.get("params"))
        .and_then(Value::as_object)
        .map(|params| params.contains_key(name))
        .unwrap_or(false)
}

/// Build the `PARAM_SET` message for a known param + a finite value.
///
/// The `param_id` is the name as 16-byte null-padded ASCII (a name longer than 16
/// bytes is truncated, as the wire field is fixed-width). The `param_type` is
/// `MAV_PARAM_TYPE_REAL32`: the only reachable known-param path here is the
/// value-only fallback (the FastAPI `known_type = 0` branch), and MAVLink's
/// `MAV_PARAM_TYPE` enum has no `0` member, so the frame carries the float type
/// (the same type the router stamps on a re-emitted `PARAM_VALUE`); ArduPilot
/// ignores the field on a `PARAM_SET` and infers the canonical type from its own
/// table. The value is written as an `f32`, the MAVLink `param_value` width.
fn build_param_set(name: &str, value: f64) -> MavMessage {
    let mut param_id = [0u8; PARAM_ID_LEN];
    let bytes = name.as_bytes();
    let copy = bytes.len().min(PARAM_ID_LEN);
    param_id[..copy].copy_from_slice(&bytes[..copy]);

    MavMessage::PARAM_SET(PARAM_SET_DATA {
        param_value: value as f32,
        target_system: TARGET_SYSTEM,
        target_component: TARGET_COMPONENT,
        param_id,
        param_type: MavParamType::MAV_PARAM_TYPE_REAL32,
    })
}

/// Poll the cached param blob for the FC's `PARAM_VALUE` echo for up to two
/// seconds, returning `(ack, cached_value)`.
///
/// Each tick reads the live state-IPC snapshot's `params[name]`; the echo counts
/// as an `ack` once the cached value lands within [`ACK_TOLERANCE`] of the target.
/// Mirrors the FastAPI route's `while ... < deadline` loop: the cached value is
/// reported even when the ack times out (so the caller sees the last value seen),
/// and the loop sleeps [`ACK_POLL_INTERVAL`] between reads.
async fn poll_for_ack(state: &AppState, name: &str, target: f64) -> (bool, Option<f64>) {
    let deadline = tokio::time::Instant::now() + ACK_POLL_TIMEOUT;
    let mut cached_value: Option<f64> = None;
    loop {
        cached_value = cached_param_value(state.state.snapshot().as_ref(), name).or(cached_value);
        if let Some(v) = cached_value {
            if (v - target).abs() < ACK_TOLERANCE {
                return (true, Some(v));
            }
        }
        if tokio::time::Instant::now() >= deadline {
            return (false, cached_value);
        }
        tokio::time::sleep(ACK_POLL_INTERVAL).await;
    }
}

/// Read `params[name]` out of a snapshot as a number, or `None` when the snapshot
/// is absent / the blob is missing or not an object / the param is absent or
/// non-numeric. Mirrors the FastAPI route reading the cached value back.
fn cached_param_value(snapshot: Option<&Value>, name: &str) -> Option<f64> {
    snapshot
        .and_then(Value::as_object)
        .and_then(|m| m.get("params"))
        .and_then(Value::as_object)
        .and_then(|params| params.get(name))
        .and_then(Value::as_f64)
}

/// Build the success body, mirroring the FastAPI `ParamSetResponse`. The message
/// is empty on an ack, else the FastAPI's no-echo text; `cached_value` is the last
/// value seen in the cache (a JSON number, or `null` when the cache never carried
/// the param).
fn build_set_response(name: &str, value: f64, ack: bool, cached_value: Option<f64>) -> Value {
    json!({
        "name": name,
        "value": value,
        "ack": ack,
        "cached_value": cached_value,
        "message": if ack { "" } else { NO_ACK_MESSAGE },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::PairingState;
    use crate::ipc::{LogdQueryClient, MavlinkIpcClient, StateIpcClient};
    use crate::state::PairingPaths;
    use std::sync::Arc;

    /// Build an `AppState` for a handler test: a disconnected state client (the
    /// test primes its snapshot directly), a MAVLink client pointed at an absent
    /// socket (so a send fails → the 503 path), and inert paths for the rest.
    fn test_state(dir: &std::path::Path) -> AppState {
        let pairing = Arc::new(PairingState::with_path(dir.join("pairing.json")));
        let state = StateIpcClient::disconnected();
        let mavlink = MavlinkIpcClient::new(dir.join("absent-mavlink.sock"));
        let logd = LogdQueryClient::new(dir.join("absent-logd.sock"));
        let pairing_paths = PairingPaths {
            config: dir.join("config.yaml"),
            pairing_json: dir.join("pairing.json"),
            wfb_key_dir: dir.join("wfb"),
            bind_state: dir.join("bind-state.json"),
        };
        AppState::new(
            pairing,
            state,
            mavlink,
            logd,
            dir.join("board.json"),
            pairing_paths,
        )
    }

    /// Decode a built PARAM_SET message back into its data for the parity asserts.
    fn round_trip(msg: &MavMessage) -> PARAM_SET_DATA {
        let header = MavHeader {
            system_id: SOURCE_SYSTEM_ID,
            component_id: SOURCE_COMPONENT_ID,
            sequence: 0,
        };
        let frame = mavlink::serialize_v2(header, msg).unwrap();
        let (_h, decoded) = mavlink::parse_v2(&frame).unwrap();
        match decoded {
            MavMessage::PARAM_SET(d) => d,
            other => panic!("expected PARAM_SET, got {other:?}"),
        }
    }

    /// The 16-byte param_id, trimmed of its null padding, as a string.
    fn param_name(d: &PARAM_SET_DATA) -> String {
        let end = d
            .param_id
            .iter()
            .position(|&b| b == 0)
            .unwrap_or(PARAM_ID_LEN);
        String::from_utf8_lossy(&d.param_id[..end]).to_string()
    }

    // ── the built frame ──────────────────────────────────────────────────────

    #[test]
    fn builds_a_param_set_for_a_known_param_and_value() {
        let msg = build_param_set("WPNAV_SPEED", 750.0);
        let d = round_trip(&msg);
        assert_eq!(param_name(&d), "WPNAV_SPEED");
        assert_eq!(d.param_value, 750.0);
        assert_eq!(d.target_system, 1);
        assert_eq!(d.target_component, 1);
        // The value-only path carries the float type; ArduPilot infers the real type.
        assert_eq!(d.param_type, MavParamType::MAV_PARAM_TYPE_REAL32);
    }

    #[test]
    fn the_frame_header_carries_the_source_identity() {
        let msg = build_param_set("ATC_RAT_RLL_P", 0.135);
        let header = MavHeader {
            system_id: SOURCE_SYSTEM_ID,
            component_id: SOURCE_COMPONENT_ID,
            sequence: 0,
        };
        let frame = mavlink::serialize_v2(header, &msg).unwrap();
        let (h, _msg) = mavlink::parse_v2(&frame).unwrap();
        assert_eq!(h.system_id, 1, "source system is the companion identity");
        assert_eq!(
            h.component_id, 191,
            "source component is the companion identity"
        );
    }

    #[test]
    fn a_long_param_name_is_truncated_to_sixteen_bytes() {
        // The wire param_id is fixed at 16 bytes; a longer name is truncated.
        let msg = build_param_set("THIS_NAME_IS_WAY_TOO_LONG_FOR_THE_FIELD", 1.0);
        let d = round_trip(&msg);
        assert_eq!(param_name(&d), "THIS_NAME_IS_WAY");
    }

    // ── the success body ─────────────────────────────────────────────────────

    #[test]
    fn the_acked_success_body_has_an_empty_message() {
        let body = build_set_response("WPNAV_SPEED", 750.0, true, Some(750.0));
        assert_eq!(
            body,
            json!({
                "name": "WPNAV_SPEED",
                "value": 750.0,
                "ack": true,
                "cached_value": 750.0,
                "message": "",
            })
        );
    }

    #[test]
    fn the_unacked_success_body_carries_the_no_echo_text_and_null_cache() {
        let body = build_set_response("WPNAV_SPEED", 750.0, false, None);
        assert_eq!(
            body,
            json!({
                "name": "WPNAV_SPEED",
                "value": 750.0,
                "ack": false,
                "cached_value": Value::Null,
                "message": "FC did not echo PARAM_VALUE within 2s",
            })
        );
    }

    // ── param_known ──────────────────────────────────────────────────────────

    #[test]
    fn param_known_reads_the_snapshot_params_blob() {
        let snap = json!({ "params": { "WPNAV_SPEED": 500.0 } });
        assert!(param_known(Some(&snap), "WPNAV_SPEED"));
        assert!(!param_known(Some(&snap), "DOES_NOT_EXIST"));
        // Absent snapshot / absent or non-object blob all read as not-known.
        assert!(!param_known(None, "WPNAV_SPEED"));
        assert!(!param_known(Some(&json!({})), "WPNAV_SPEED"));
        assert!(!param_known(
            Some(&json!({ "params": "nope" })),
            "WPNAV_SPEED"
        ));
    }

    #[test]
    fn cached_param_value_reads_a_numeric_param() {
        let snap = json!({ "params": { "WPNAV_SPEED": 750.0 } });
        assert_eq!(cached_param_value(Some(&snap), "WPNAV_SPEED"), Some(750.0));
        // Absent param / absent blob / non-numeric all read as None.
        assert_eq!(cached_param_value(Some(&snap), "OTHER"), None);
        assert_eq!(cached_param_value(None, "WPNAV_SPEED"), None);
        assert_eq!(
            cached_param_value(Some(&json!({ "params": { "X": "nope" } })), "X"),
            None
        );
    }

    // ── the handler: the guard order + the write-path 503 ────────────────────

    /// A non-finite value is a 400 before any snapshot read or send.
    #[tokio::test]
    async fn non_finite_value_is_a_400() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_state(dir.path());
        // A non-finite value is rejected by the first guard (the body model holds an
        // f64; the route checks finiteness, matching the FastAPI math.isfinite gate).
        let resp = set_param(
            Path("WPNAV_SPEED".to_string()),
            State(state),
            Json(ParamSetRequest { value: f64::NAN }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        assert_eq!(body_detail(resp).await, "value must be a finite number");
    }

    /// A param absent from the snapshot's `params` blob is a 404 with the FastAPI
    /// message, before the FC-connected check.
    #[tokio::test]
    async fn unknown_param_is_a_404() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_state(dir.path());
        // A snapshot with an empty params blob → the name is unknown.
        state
            .state
            .set_snapshot_for_test(json!({ "fc_connected": true, "params": {} }));
        let resp = set_param(
            Path("NO_SUCH_PARAM".to_string()),
            State(state),
            Json(ParamSetRequest { value: 1.0 }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            body_detail(resp).await,
            "Parameter 'NO_SUCH_PARAM' not in cache; agent must observe a \
             PARAM_VALUE for it before writes are allowed"
        );
    }

    /// A known param with the FC disconnected is a 503, before any send.
    #[tokio::test]
    async fn known_param_with_fc_disconnected_is_a_503() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_state(dir.path());
        state.state.set_snapshot_for_test(json!({
            "fc_connected": false,
            "params": { "WPNAV_SPEED": 500.0 },
        }));
        let resp = set_param(
            Path("WPNAV_SPEED".to_string()),
            State(state),
            Json(ParamSetRequest { value: 750.0 }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body_detail(resp).await, "FC not connected");
    }

    /// A known param, FC connected, but no MAVLink socket: the send fails, so the
    /// route is a 503 "FC connection unavailable" (the native no-link posture). The
    /// test client points at an absent socket, so the send errors fast.
    #[tokio::test]
    async fn send_failure_with_no_mavlink_socket_is_a_503() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_state(dir.path());
        state.state.set_snapshot_for_test(json!({
            "fc_connected": true,
            "params": { "WPNAV_SPEED": 500.0 },
        }));
        let resp = set_param(
            Path("WPNAV_SPEED".to_string()),
            State(state),
            Json(ParamSetRequest { value: 750.0 }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body_detail(resp).await, "FC connection unavailable");
    }

    /// A known param, FC connected, a live MAVLink socket that accepts the frame,
    /// and the snapshot already carrying the target value: the send succeeds, the
    /// poll sees the echo immediately, and the body reports `ack: true`. This
    /// exercises the full write-path against a mock socket (mirroring the command
    /// route's mock-socket test).
    #[tokio::test]
    async fn full_write_against_a_live_socket_acks_when_the_cache_holds_the_target() {
        use tokio::io::AsyncReadExt;
        use tokio::net::UnixListener;

        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("mavlink.sock");
        let listener = UnixListener::bind(&sock).unwrap();

        // The server reads one framed message and hands the raw frame back.
        let server = tokio::spawn(async move {
            use ados_protocol::frame::{decode_len, HEADER_SIZE, MAVLINK_MAX_FRAME};
            let (mut conn, _addr) = listener.accept().await.unwrap();
            let mut header = [0u8; HEADER_SIZE];
            conn.read_exact(&mut header).await.unwrap();
            let len = decode_len(header, MAVLINK_MAX_FRAME, false).unwrap();
            let mut body = vec![0u8; len];
            conn.read_exact(&mut body).await.unwrap();
            body
        });

        // Build a state whose MAVLink client points at the live socket.
        let pairing = Arc::new(PairingState::with_path(dir.path().join("pairing.json")));
        let stateipc = StateIpcClient::disconnected();
        // The cache already holds the target, so the first poll tick acks.
        stateipc.set_snapshot_for_test(json!({
            "fc_connected": true,
            "params": { "WPNAV_SPEED": 750.0 },
        }));
        let mavlink = MavlinkIpcClient::new(sock.clone());
        let logd = LogdQueryClient::new(dir.path().join("absent-logd.sock"));
        let pairing_paths = PairingPaths {
            config: dir.path().join("config.yaml"),
            pairing_json: dir.path().join("pairing.json"),
            wfb_key_dir: dir.path().join("wfb"),
            bind_state: dir.path().join("bind-state.json"),
        };
        let state = AppState::new(
            pairing,
            stateipc,
            mavlink,
            logd,
            dir.path().join("board.json"),
            pairing_paths,
        );

        let resp = set_param(
            Path("WPNAV_SPEED".to_string()),
            State(state),
            Json(ParamSetRequest { value: 750.0 }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);

        // The server received a PARAM_SET frame for the right param + value.
        let frame = server.await.unwrap();
        let (_h, decoded) = mavlink::parse_v2(&frame).unwrap();
        let d = match decoded {
            MavMessage::PARAM_SET(d) => d,
            other => panic!("expected PARAM_SET on the socket, got {other:?}"),
        };
        assert_eq!(param_name(&d), "WPNAV_SPEED");
        assert_eq!(d.param_value, 750.0);

        // The body acks (the cache already held the target).
        let body = body_json(resp).await;
        assert_eq!(body["name"], json!("WPNAV_SPEED"));
        assert_eq!(body["value"], json!(750.0));
        assert_eq!(body["ack"], json!(true));
        assert_eq!(body["cached_value"], json!(750.0));
        assert_eq!(body["message"], json!(""));
    }

    /// Read the `{"detail"}` string out of a response body.
    async fn body_detail(resp: Response) -> String {
        body_json(resp).await["detail"]
            .as_str()
            .unwrap()
            .to_string()
    }

    /// Read a response body as JSON.
    async fn body_json(resp: Response) -> Value {
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }
}
