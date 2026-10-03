//! Flight-controller parameter write route.
//!
//! `POST /api/params/{name}` writes a single FC parameter. The body is
//! `{"value": <number>}`; the route turns it into a MAVLink `PARAM_SET` frame
//! and writes it to `/run/ados/mavlink.sock`, which the router forwards to the
//! FC. ArduPilot saves the value to EEPROM on receipt and echoes a `PARAM_VALUE`
//! back; the route then polls the router's on-disk parameter cache for up to two
//! seconds to confirm the new value landed and reports that as the `ack`.
//!
//! ## Why the cache FILE and not the state snapshot
//!
//! The parameter map used to ride the 10 Hz vehicle-state snapshot in full — ~24 KB
//! against ArduPilot's ~700 parameters, which alone made a relayed read need ~21
//! aux-lane fragments — and both the known-param guard and the ack poll read it
//! from there. The map now lives only in the file the router persists atomically
//! ([`crate::param_store`]). The router writes that file the instant a value
//! actually changes outside a `PARAM_REQUEST_LIST` sweep precisely so this ack poll
//! still sees the echo inside its window; during a sweep the write is debounced,
//! which is the only period a two-second ack could miss and is also the one period
//! no operator write is in flight.
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
//! ## Guard order
//!
//! 1. The value must be a finite number → 400 `"value must be a finite number"`.
//! 2. The parameter must not be the vehicle's own MAVLink identity → 409.
//! 3. The parameter must be one the agent has already observed (present in the
//!    router's parameter cache) → 404 when it is not. This guards against typos
//!    pushing garbage params into the FC.
//! 4. The cache must record the parameter's `MAV_PARAM_TYPE` → 409
//!    `E_PARAM_TYPE_UNKNOWN` when it does not. The type decides the encoding,
//!    and a guessed type corrupts integer parameters on PX4.
//! 5. The FC must be connected → 503 `"FC not connected"`.
//! 6. The value must fit the parameter's type → 400 (an INT8 cannot hold 300,
//!    and wrapping it would write a different number than the operator typed).
//! 7. The frame send must succeed → 503 `"FC connection unavailable"` when the
//!    MAVLink socket cannot be reached; the command is never silently dropped.
//!
//! The armed interlock in front of every route refuses this write while the
//! vehicle is armed unless the caller forces it.
//!
//! ## The `PARAM_SET` frame
//!
//! The frame carries the type the FC itself declared in its last `PARAM_VALUE`
//! for the name, read from the router's cache. The 4-byte value field is
//! encoded for the firmware named by the vehicle's HEARTBEAT: PX4 takes an
//! integer parameter as its integer bytes, ArduPilot takes every value as a
//! float (see [`ados_protocol::param_codec`]). The ack compares the echo with
//! the value the vehicle will actually hold (the rounded integer, or the
//! nearest 32-bit float), at 32-bit float precision.
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
use ados_protocol::param_codec;

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

/// Parameters that change the vehicle's own MAVLink identity.
///
/// Writing one of these through this surface is irreversible: [`TARGET_SYSTEM`]
/// is fixed, so the vehicle stops answering the very route that would restore
/// it. Matched case-insensitively because operators type parameter names.
fn is_identity_param(name: &str) -> bool {
    matches!(
        name.trim().to_ascii_uppercase().as_str(),
        "SYSID_THISMAV" | "MAV_SYS_ID"
    )
}

/// The width of a MAVLink `param_id` field: a 16-byte, null-padded ASCII name.
const PARAM_ID_LEN: usize = 16;

/// How long to poll the cached param blob for the FC's `PARAM_VALUE` echo before
/// reporting `ack: false`, matching the FastAPI route's 2-second deadline.
const ACK_POLL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// One poll interval between cache reads while waiting for the echo, matching the
/// FastAPI route's `await asyncio.sleep(0.1)`.
const ACK_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(100);

/// The message the route reports when the FC did not echo a `PARAM_VALUE` within
/// the poll window, byte-identical to the FastAPI route's text.
const NO_ACK_MESSAGE: &str = "FC did not echo PARAM_VALUE within 2s";

/// The `POST /api/params/{name}` request body. A single required numeric `value` to
/// write to the FC.
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

    // 2. Refuse a write that would put the vehicle beyond this surface's reach.
    //
    //    `TARGET_SYSTEM` below is fixed at 1, so changing the vehicle's own
    //    MAVLink system id makes every subsequent PARAM_SET from this agent
    //    addressed to a system that no longer exists — including the write that
    //    would put it back. ArduPilot drops them, and `arm`/`disarm`/`mode`/
    //    `rtl` stop reaching the aircraft too, because the command path carries
    //    the same constant. There is no reboot route on this surface either, so
    //    an operator cannot even cycle out of it.
    //
    //    One request, irreversible, and nothing about it looks unusual at the
    //    time. Refused with the reason named rather than accepted. Lifting the
    //    limit properly means discovering the vehicle's system id from its
    //    HEARTBEAT and carrying it through both paths per vehicle; merely
    //    widening the constant would replace an obvious failure with a subtle
    //    one, where a command silently reaches the wrong airframe.
    if is_identity_param(&name) {
        return ParamError {
            status: StatusCode::CONFLICT,
            detail: format!(
                "Refusing to write '{name}': this agent addresses vehicle \
                 system id 1 only, so changing the vehicle's MAVLink identity \
                 would make it unreachable by this surface — including the \
                 write that would undo it, and arm/disarm/mode/rtl. Use a \
                 direct USB parameter tool if you need a non-default system id."
            ),
        }
        .into_response();
    }

    // 3. The parameter must be one the agent has already observed. The native
    //    front's only param source is the router's on-disk cache; a name absent
    //    from it (or an unreadable cache) is refused with the FastAPI 404.
    let Some(cached) = crate::param_store::read_param(&state.params_path, &name)
        .ok()
        .flatten()
    else {
        return ParamError {
            status: StatusCode::NOT_FOUND,
            detail: format!(
                "Parameter '{name}' not in cache; agent must observe a \
                 PARAM_VALUE for it before writes are allowed"
            ),
        }
        .into_response();
    };

    // 4. The FC's declared type decides the encoding; without it the write
    //    would be a guess, and a wrong guess corrupts an integer on PX4.
    let Some(param_type) = cached.param_type else {
        return type_unknown(&name);
    };

    // 5. The FC must be connected (the FastAPI `fc.connected` guard) → 503.
    let snapshot = state.state.snapshot();
    let connected = snapshot
        .as_ref()
        .and_then(|s| s.get("fc_connected"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if !connected {
        return ParamError {
            status: StatusCode::SERVICE_UNAVAILABLE,
            detail: "FC not connected".to_string(),
        }
        .into_response();
    }

    // 6. Encode for the firmware the HEARTBEAT named; a value the type cannot
    //    hold is refused rather than wrapped.
    let autopilot = snapshot
        .as_ref()
        .and_then(|s| s.get("autopilot"))
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let encoded =
        match param_codec::encode(target, param_type, param_codec::uses_bytewise(autopilot)) {
            Ok(e) => e,
            Err(why) => {
                return ParamError {
                    status: StatusCode::BAD_REQUEST,
                    detail: why,
                }
                .into_response()
            }
        };

    // Build the PARAM_SET frame and serialize it with the source identity.
    let msg = build_param_set(&name, encoded.wire, param_type);
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

    // 7. Send the frame. An absent or broken MAVLink socket means no live FC link
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

    // Poll the cached params for up to two seconds for the FC's PARAM_VALUE echo.
    // The router rewrites its cache file as PARAM_VALUE frames land; this re-reads
    // it each tick, the native equivalent of the FastAPI route polling its
    // in-process cache.
    let (ack, cached_value) = poll_for_ack(&state, &name, encoded.stored).await;

    tracing::info!(param = %name, value = target, param_type, ack, "param_set");
    Json(build_set_response(&name, target, ack, cached_value)).into_response()
}

/// The 409 for a cached parameter whose `MAV_PARAM_TYPE` the cache does not
/// record. A fresh parameter download from the FC records it.
fn type_unknown(name: &str) -> Response {
    (
        StatusCode::CONFLICT,
        Json(json!({
            "error": "E_PARAM_TYPE_UNKNOWN",
            "detail": format!(
                "The type of '{name}' is not known yet; refresh the parameter list \
                 from the flight controller before writing it"
            ),
        })),
    )
        .into_response()
}

/// The dialect enum member for a `MAV_PARAM_TYPE` value the cache validated as
/// defined (1 through 10).
fn mav_param_type(param_type: u8) -> MavParamType {
    match param_type {
        param_codec::MAV_PARAM_TYPE_UINT8 => MavParamType::MAV_PARAM_TYPE_UINT8,
        param_codec::MAV_PARAM_TYPE_INT8 => MavParamType::MAV_PARAM_TYPE_INT8,
        param_codec::MAV_PARAM_TYPE_UINT16 => MavParamType::MAV_PARAM_TYPE_UINT16,
        param_codec::MAV_PARAM_TYPE_INT16 => MavParamType::MAV_PARAM_TYPE_INT16,
        param_codec::MAV_PARAM_TYPE_UINT32 => MavParamType::MAV_PARAM_TYPE_UINT32,
        param_codec::MAV_PARAM_TYPE_INT32 => MavParamType::MAV_PARAM_TYPE_INT32,
        param_codec::MAV_PARAM_TYPE_UINT64 => MavParamType::MAV_PARAM_TYPE_UINT64,
        param_codec::MAV_PARAM_TYPE_INT64 => MavParamType::MAV_PARAM_TYPE_INT64,
        param_codec::MAV_PARAM_TYPE_REAL64 => MavParamType::MAV_PARAM_TYPE_REAL64,
        _ => MavParamType::MAV_PARAM_TYPE_REAL32,
    }
}

/// Build the `PARAM_SET` message for a known param, its encoded value field and
/// its declared type.
///
/// The `param_id` is the name as 16-byte null-padded ASCII (a name longer than 16
/// bytes is truncated, as the wire field is fixed-width).
fn build_param_set(name: &str, wire_value: f32, param_type: u8) -> MavMessage {
    let mut param_id = [0u8; PARAM_ID_LEN];
    let bytes = name.as_bytes();
    let copy = bytes.len().min(PARAM_ID_LEN);
    param_id[..copy].copy_from_slice(&bytes[..copy]);

    MavMessage::PARAM_SET(PARAM_SET_DATA {
        param_value: wire_value,
        target_system: TARGET_SYSTEM,
        target_component: TARGET_COMPONENT,
        param_id: param_id.into(),
        param_type: mav_param_type(param_type),
    })
}

/// Poll the cached param for the FC's `PARAM_VALUE` echo for up to two seconds,
/// returning `(ack, cached_value)`.
///
/// Each tick re-reads the router's cache file; the echo counts as an `ack` once the
/// cached value matches `stored`, the value the vehicle holds after applying the
/// write, at 32-bit float precision. The cached value is reported even when the
/// ack times out (so the caller sees the last value seen), and the loop sleeps
/// [`ACK_POLL_INTERVAL`] between reads.
async fn poll_for_ack(state: &AppState, name: &str, stored: f64) -> (bool, Option<f64>) {
    let deadline = tokio::time::Instant::now() + ACK_POLL_TIMEOUT;
    let mut cached_value: Option<f64> = None;
    loop {
        cached_value = cached_param_value(state, name).or(cached_value);
        if let Some(v) = cached_value {
            if param_codec::echo_matches(v, stored) {
                return (true, Some(v));
            }
        }
        if tokio::time::Instant::now() >= deadline {
            return (false, cached_value);
        }
        tokio::time::sleep(ACK_POLL_INTERVAL).await;
    }
}

/// Read the cached value of `name` as a number, or `None` when the cache is
/// missing / unreadable or the param is absent or non-numeric.
fn cached_param_value(state: &AppState, name: &str) -> Option<f64> {
    crate::param_store::read_param(&state.params_path, name)
        .ok()
        .flatten()
        .map(|p| p.value)
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
    #[test]
    fn identity_params_are_recognised_however_they_are_typed() {
        // Operators type parameter names, and MAVLink names are conventionally
        // upper case but arrive however they arrive.
        for name in [
            "SYSID_THISMAV",
            "sysid_thismav",
            " SysId_ThisMav ",
            "MAV_SYS_ID",
        ] {
            assert!(
                is_identity_param(name),
                "{name} changes the vehicle identity"
            );
        }
    }

    #[test]
    fn ordinary_params_are_not_caught_by_the_identity_guard() {
        // The guard must be narrow. Refusing a benign parameter because its
        // name merely resembles one of these would be its own bug.
        for name in [
            "SYSID_MYGCS", // which GCS may command us — NOT our own identity
            "SYSID_ENFORCE",
            "NTF_LED_BRIGHT",
            "ACRO_RP_RATE",
            "AHRS_EKF_TYPE",
        ] {
            assert!(!is_identity_param(name), "{name} must remain writable");
        }
    }

    #[tokio::test]
    async fn writing_the_vehicle_identity_is_refused_rather_than_stranding_it() {
        // TARGET_SYSTEM is fixed at 1, so a successful write here would make the
        // aircraft stop answering the very route that would undo it — and
        // arm/disarm/mode/rtl with it. One request, irreversible, and nothing
        // about it looks unusual at the time.
        let dir = tempfile::tempdir().unwrap();
        let state = test_state(dir.path());
        // Even with the parameter present in the cache (so the 404 guard would
        // pass), the write is refused.
        write_cache(dir.path(), &[("SYSID_THISMAV", 1.0)]);
        let resp = set_param(
            Path("SYSID_THISMAV".to_string()),
            State(state),
            Json(ParamSetRequest { value: 2.0 }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::CONFLICT);
    }

    use super::*;
    use crate::auth::PairingState;
    use crate::ipc::{LogdQueryClient, MavlinkIpcClient, StateIpcClient};
    use crate::state::PairingPaths;
    use std::sync::Arc;

    /// Build an `AppState` for a handler test: a disconnected state client (the
    /// test primes its snapshot directly), a MAVLink client pointed at an absent
    /// socket (so a send fails → the 503 path), and inert paths for the rest.
    fn test_state(dir: &std::path::Path) -> AppState {
        test_state_with_socket(dir, dir.join("absent-mavlink.sock"))
    }

    /// [`test_state`] with the MAVLink client pointed at `mavlink_sock`.
    fn test_state_with_socket(dir: &std::path::Path, mavlink_sock: std::path::PathBuf) -> AppState {
        let pairing = Arc::new(PairingState::with_path(dir.join("pairing.json")));
        let state = StateIpcClient::disconnected();
        let mavlink = MavlinkIpcClient::new(mavlink_sock);
        let logd = LogdQueryClient::new(dir.join("absent-logd.sock"));
        let pairing_paths = PairingPaths {
            config: dir.join("config.yaml"),
            pairing_json: dir.join("pairing.json"),
            wfb_key_dir: dir.join("wfb"),
            bind_state: dir.join("bind-state.json"),
            profile_conf: dir.join("profile.conf"),
            mesh_role: dir.join("mesh-role"),
            relay_secret: dir.join("relay-peer-secret"),
        };
        AppState::new(
            pairing,
            state,
            mavlink,
            logd,
            dir.join("board.json"),
            pairing_paths,
            std::sync::Arc::new(crate::dashboard_pin::DashboardPin::with_path(
                dir.join("dashboard-pin.json"),
            )),
            std::sync::Arc::new(crate::mcp::McpTokenStore::with_path(
                dir.join("mcp-token.json"),
            )),
        )
        .with_params_path(dir.join("params.json"))
    }

    /// Write a router-shaped parameter cache at the path `test_state` reads — the
    /// `{name: {value, param_type, last_updated}}` document the MAVLink router
    /// persists. Every entry is REAL32.
    fn write_cache(dir: &std::path::Path, entries: &[(&str, f64)]) {
        let typed: Vec<(&str, f64, Value)> =
            entries.iter().map(|(n, v)| (*n, *v, json!(9))).collect();
        write_typed_cache(dir, &typed);
    }

    /// [`write_cache`] with an explicit `param_type` per entry (`null` for none).
    fn write_typed_cache(dir: &std::path::Path, entries: &[(&str, f64, Value)]) {
        let doc: serde_json::Map<String, Value> = entries
            .iter()
            .map(|(name, value, param_type)| {
                (
                    (*name).to_string(),
                    json!({ "value": value, "param_type": param_type, "last_updated": 1.0 }),
                )
            })
            .collect();
        std::fs::write(
            dir.join("params.json"),
            serde_json::to_vec(&Value::Object(doc)).unwrap(),
        )
        .unwrap();
    }

    /// A Unix socket that reads one framed MAVLink message and hands it back.
    fn one_frame_socket(sock: &std::path::Path) -> tokio::task::JoinHandle<Vec<u8>> {
        use tokio::io::AsyncReadExt;
        let listener = tokio::net::UnixListener::bind(sock).unwrap();
        tokio::spawn(async move {
            use ados_protocol::frame::{decode_len, HEADER_SIZE, MAVLINK_MAX_FRAME};
            let (mut conn, _addr) = listener.accept().await.unwrap();
            let mut header = [0u8; HEADER_SIZE];
            conn.read_exact(&mut header).await.unwrap();
            let len = decode_len(header, MAVLINK_MAX_FRAME, false).unwrap();
            let mut body = vec![0u8; len];
            conn.read_exact(&mut body).await.unwrap();
            body
        })
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
    fn builds_a_param_set_carrying_the_declared_type() {
        let msg = build_param_set("WPNAV_SPEED", 750.0, param_codec::MAV_PARAM_TYPE_REAL32);
        let d = round_trip(&msg);
        assert_eq!(param_name(&d), "WPNAV_SPEED");
        assert_eq!(d.param_value, 750.0);
        assert_eq!(d.target_system, 1);
        assert_eq!(d.target_component, 1);
        assert_eq!(d.param_type, MavParamType::MAV_PARAM_TYPE_REAL32);
        let d = round_trip(&build_param_set(
            "FRAME_CLASS",
            1.0,
            param_codec::MAV_PARAM_TYPE_INT8,
        ));
        assert_eq!(d.param_type, MavParamType::MAV_PARAM_TYPE_INT8);
    }

    #[test]
    fn the_frame_header_carries_the_source_identity() {
        let msg = build_param_set("ATC_RAT_RLL_P", 0.135, param_codec::MAV_PARAM_TYPE_REAL32);
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
        let msg = build_param_set(
            "THIS_NAME_IS_WAY_TOO_LONG_FOR_THE_FIELD",
            1.0,
            param_codec::MAV_PARAM_TYPE_REAL32,
        );
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

    // ── cached_param_value + the cache-backed guards ─────────────────────────

    /// A truncated cache must never be read as "this param exists": that would
    /// let a PARAM_SET through for a name the agent has never actually seen.
    #[tokio::test]
    async fn a_corrupt_cache_is_a_404_not_a_write() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_state(dir.path());
        std::fs::write(dir.path().join("params.json"), b"{ truncated").unwrap();
        state
            .state
            .set_snapshot_for_test(json!({ "fc_connected": true }));
        let resp = set_param(
            Path("WPNAV_SPEED".to_string()),
            State(state),
            Json(ParamSetRequest { value: 1.0 }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    /// A cached name with no recorded type is refused rather than guessed: a
    /// guessed float would corrupt an integer parameter on PX4.
    #[tokio::test]
    async fn a_param_without_a_cached_type_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_state(dir.path());
        write_typed_cache(dir.path(), &[("COM_FLTMODE1", 5.0, Value::Null)]);
        state
            .state
            .set_snapshot_for_test(json!({ "fc_connected": true, "autopilot": 12 }));
        let resp = set_param(
            Path("COM_FLTMODE1".to_string()),
            State(state),
            Json(ParamSetRequest { value: 7.0 }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::CONFLICT);
        assert_eq!(
            body_json(resp).await["error"],
            json!("E_PARAM_TYPE_UNKNOWN")
        );
    }

    /// A value its type cannot hold is a 400, never a wrapped write.
    #[tokio::test]
    async fn a_value_outside_the_params_type_is_a_400() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_state(dir.path());
        write_typed_cache(dir.path(), &[("FRAME_CLASS", 1.0, json!(2))]);
        state
            .state
            .set_snapshot_for_test(json!({ "fc_connected": true, "autopilot": 3 }));
        let resp = set_param(
            Path("FRAME_CLASS".to_string()),
            State(state),
            Json(ParamSetRequest { value: 300.0 }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    /// PX4 takes an integer parameter as its integer bytes. Sent as the float
    /// 5.0, the vehicle would store 1084227584.
    #[tokio::test]
    async fn a_px4_integer_write_sends_the_integer_bytes_and_type() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("mavlink.sock");
        let server = one_frame_socket(&sock);
        let state = test_state_with_socket(dir.path(), sock);
        // The cache already holds the decoded target, so the first poll acks.
        write_typed_cache(dir.path(), &[("COM_FLTMODE1", 5.0, json!(6))]);
        state
            .state
            .set_snapshot_for_test(json!({ "fc_connected": true, "autopilot": 12 }));
        let resp = set_param(
            Path("COM_FLTMODE1".to_string()),
            State(state),
            Json(ParamSetRequest { value: 5.0 }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let (_h, decoded) = mavlink::parse_v2(&server.await.unwrap()).unwrap();
        let MavMessage::PARAM_SET(d) = decoded else {
            panic!("expected PARAM_SET on the socket");
        };
        assert_eq!(d.param_value.to_le_bytes(), 5i32.to_le_bytes());
        assert_eq!(d.param_type, MavParamType::MAV_PARAM_TYPE_INT32);
        assert_eq!(body_json(resp).await["ack"], json!(true));
    }

    /// ArduPilot takes the same integer as a float, with its declared type.
    #[tokio::test]
    async fn an_ardupilot_integer_write_sends_the_float_and_type() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("mavlink.sock");
        let server = one_frame_socket(&sock);
        let state = test_state_with_socket(dir.path(), sock);
        write_typed_cache(dir.path(), &[("FLTMODE1", 5.0, json!(2))]);
        state
            .state
            .set_snapshot_for_test(json!({ "fc_connected": true, "autopilot": 3 }));
        let resp = set_param(
            Path("FLTMODE1".to_string()),
            State(state),
            Json(ParamSetRequest { value: 5.0 }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let (_h, decoded) = mavlink::parse_v2(&server.await.unwrap()).unwrap();
        let MavMessage::PARAM_SET(d) = decoded else {
            panic!("expected PARAM_SET on the socket");
        };
        assert_eq!(d.param_value, 5.0);
        assert_eq!(d.param_type, MavParamType::MAV_PARAM_TYPE_INT8);
    }

    /// A float write whose f32 echo differs from the f64 request in the seventh
    /// digit still acks: the vehicle holds exactly what it was sent.
    #[tokio::test]
    async fn a_landed_float_write_acks_at_f32_precision() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("mavlink.sock");
        let server = one_frame_socket(&sock);
        let state = test_state_with_socket(dir.path(), sock);
        write_cache(dir.path(), &[("WPNAV_SPEED", f64::from(100.1f32))]);
        state
            .state
            .set_snapshot_for_test(json!({ "fc_connected": true, "autopilot": 3 }));
        let resp = set_param(
            Path("WPNAV_SPEED".to_string()),
            State(state),
            Json(ParamSetRequest { value: 100.1 }),
        )
        .await;
        server.await.unwrap();
        assert_eq!(body_json(resp).await["ack"], json!(true));
    }

    #[test]
    fn cached_param_value_reads_a_numeric_param() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_state(dir.path());
        // Absent cache reads as None.
        assert_eq!(cached_param_value(&state, "WPNAV_SPEED"), None);

        write_cache(dir.path(), &[("WPNAV_SPEED", 750.0)]);
        assert_eq!(cached_param_value(&state, "WPNAV_SPEED"), Some(750.0));
        // A name absent from the cache reads as None.
        assert_eq!(cached_param_value(&state, "OTHER"), None);
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
        // No cache file at all → the name is unknown.
        state
            .state
            .set_snapshot_for_test(json!({ "fc_connected": true }));
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
        write_cache(dir.path(), &[("WPNAV_SPEED", 500.0)]);
        state
            .state
            .set_snapshot_for_test(json!({ "fc_connected": false }));
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
        write_cache(dir.path(), &[("WPNAV_SPEED", 500.0)]);
        state
            .state
            .set_snapshot_for_test(json!({ "fc_connected": true }));
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
        write_cache(dir.path(), &[("WPNAV_SPEED", 750.0)]);
        stateipc.set_snapshot_for_test(json!({ "fc_connected": true }));
        let mavlink = MavlinkIpcClient::new(sock.clone());
        let logd = LogdQueryClient::new(dir.path().join("absent-logd.sock"));
        let pairing_paths = PairingPaths {
            config: dir.path().join("config.yaml"),
            pairing_json: dir.path().join("pairing.json"),
            wfb_key_dir: dir.path().join("wfb"),
            bind_state: dir.path().join("bind-state.json"),
            profile_conf: dir.path().join("profile.conf"),
            mesh_role: dir.path().join("mesh-role"),
            relay_secret: dir.path().join("relay-peer-secret"),
        };
        let state = AppState::new(
            pairing,
            stateipc,
            mavlink,
            logd,
            dir.path().join("board.json"),
            pairing_paths,
            std::sync::Arc::new(crate::dashboard_pin::DashboardPin::with_path(
                dir.path().join("dashboard-pin.json"),
            )),
            std::sync::Arc::new(crate::mcp::McpTokenStore::with_path(
                dir.path().join("mcp-token.json"),
            )),
        )
        .with_params_path(dir.path().join("params.json"));

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
