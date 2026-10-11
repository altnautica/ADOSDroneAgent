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
//! the flight controller's own id as resolved by [`crate::fc_target::fc_target`]:
//! the system id latched from its HEARTBEAT, once per request.

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

/// Parameters that change the vehicle's own MAVLink identity.
///
/// Refused because a REST write would re-address the vehicle under an
/// operator's feet; the fleet reconciler is the only writer. Matched
/// case-insensitively because operators type parameter names.
fn is_identity_param(name: &str) -> bool {
    matches!(
        name.trim().to_ascii_uppercase().as_str(),
        // ArduPilot before 4.7, ArduPilot 4.7+, PX4.
        "SYSID_THISMAV" | "MAV_SYSID" | "MAV_SYS_ID"
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

    // 2. Refuse a write to the vehicle's own MAVLink identity. A REST write
    //    would re-address the vehicle under an operator's feet; the fleet
    //    reconciler is the only writer.
    if is_identity_param(&name) {
        return ParamError {
            status: StatusCode::CONFLICT,
            detail: format!(
                "Refusing to write '{name}': the vehicle's MAVLink identity is \
                 owned by the fleet identity reconciler, and a write here would \
                 re-address the vehicle under every other operator's commands."
            ),
        }
        .into_response();
    }

    match send_param_set(&state, &name, target).await {
        Ok((ack, cached_value)) => {
            Json(build_set_response(&name, target, ack, cached_value)).into_response()
        }
        Err(resp) => resp,
    }
}

/// The shared PARAM_SET path: guards 3-7, the send, and the ack poll.
///
/// Returns `(ack, cached_value)` on a sent frame, or the 4xx/5xx response a
/// guard raised. The REST route calls it after its own value and identity
/// guards; the fleet identity reconciler calls it directly, as the one writer
/// of the identity parameter.
// The error is the ready-to-send HTTP response; boxing it would only add an
// allocation on a cold refusal path.
#[allow(clippy::result_large_err)]
pub(crate) async fn send_param_set(
    state: &AppState,
    name: &str,
    target: f64,
) -> Result<(bool, Option<f64>), Response> {
    // 3. The parameter must be one the agent has already observed. The native
    //    front's only param source is the router's on-disk cache; a name absent
    //    from it (or an unreadable cache) is refused with the FastAPI 404.
    let Some(cached) = crate::param_store::read_param(&state.params_path, name)
        .ok()
        .flatten()
    else {
        return Err(ParamError {
            status: StatusCode::NOT_FOUND,
            detail: format!(
                "Parameter '{name}' not in cache; agent must observe a \
                 PARAM_VALUE for it before writes are allowed"
            ),
        }
        .into_response());
    };

    // 4. The FC's declared type decides the encoding; without it the write
    //    would be a guess, and a wrong guess corrupts an integer on PX4.
    let Some(param_type) = cached.param_type else {
        return Err(type_unknown(name));
    };

    // 5. The FC must be connected (the FastAPI `fc.connected` guard) → 503.
    let snapshot = state.state.snapshot();
    let connected = snapshot
        .as_ref()
        .and_then(|s| s.get("fc_connected"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if !connected {
        return Err(ParamError {
            status: StatusCode::SERVICE_UNAVAILABLE,
            detail: "FC not connected".to_string(),
        }
        .into_response());
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
                return Err(ParamError {
                    status: StatusCode::BAD_REQUEST,
                    detail: why,
                }
                .into_response())
            }
        };

    // Build the PARAM_SET frame for the FC this request resolved, and
    // serialize it with the source identity.
    let msg = build_param_set(
        name,
        encoded.wire,
        param_type,
        crate::fc_target::fc_target(state),
    );
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
            return Err(detail(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to send PARAM_SET: {e}"),
            ));
        }
    };

    // 7. Send the frame. An absent or broken MAVLink socket means no live FC link
    //    from this surface's view → 503 (the native equivalent of the FastAPI
    //    `conn is None` / send-raise paths); the write is never silently dropped.
    if let Err(e) = state.mavlink.send(&frame).await {
        tracing::warn!(error = %e, param = %name, "param_set send to the mavlink socket failed");
        return Err(ParamError {
            status: StatusCode::SERVICE_UNAVAILABLE,
            detail: "FC connection unavailable".to_string(),
        }
        .into_response());
    }

    // Poll the cached params for up to two seconds for the FC's PARAM_VALUE echo.
    // The router rewrites its cache file as PARAM_VALUE frames land; this re-reads
    // it each tick, the native equivalent of the FastAPI route polling its
    // in-process cache.
    let (ack, cached_value) = poll_for_ack(state, name, encoded.stored).await;

    tracing::info!(param = %name, value = target, param_type, ack, "param_set");
    Ok((ack, cached_value))
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

/// Build the `PARAM_SET` message for a known param, its encoded value field, its
/// declared type, and the FC `(system, component)` it is addressed to.
///
/// The `param_id` is the name as 16-byte null-padded ASCII (a name longer than 16
/// bytes is truncated, as the wire field is fixed-width).
fn build_param_set(
    name: &str,
    wire_value: f32,
    param_type: u8,
    (target_system, target_component): (u8, u8),
) -> MavMessage {
    let mut param_id = [0u8; PARAM_ID_LEN];
    let bytes = name.as_bytes();
    let copy = bytes.len().min(PARAM_ID_LEN);
    param_id[..copy].copy_from_slice(&bytes[..copy]);

    MavMessage::PARAM_SET(PARAM_SET_DATA {
        param_value: wire_value,
        target_system,
        target_component,
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
mod tests;
