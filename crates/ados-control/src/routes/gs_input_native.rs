//! The ground station's native controller.
//!
//! - **`GET /api/v1/ground-station/input`** — whether the station flies the
//!   sticks and the safety buttons natively (`native_sticks`; the kiosk cockpit
//!   then ignores them in the browser), the pilot relay and streaming state,
//!   the control mode the station follows, the attached `controller`
//!   (`{id, name, connection, driver, battery_pct}`), and the effective
//!   binding document. Read from the `ados-input` runtime sidecar; a sidecar
//!   older than its staleness window reads as not native, no controller.
//! - **`PUT /api/v1/ground-station/input/bindings`** — the Settings › Controls
//!   document. A document missing a locked binding is refused `422` with the
//!   missing ones; a valid one is forwarded to the `ados-input` daemon, which
//!   applies it to the running loop and persists it in
//!   `ground-station-input.json` next to the primary pad.

use std::path::Path;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};

use ados_hid::pilot_input::bindings::{ControlsDoc, DocError};
use ados_hid::pilot_input::status::{read_control_mode, wall_ms, GsInputState};

use crate::routes::gs_gamepad_write::{is_ground_station, profile_mismatch};
use crate::state::AppState;

/// The response body from the two files (pure; tested without the daemon).
pub(crate) fn input_body(state_path: &Path, gs_input: &Path, now_ms: u64) -> Value {
    let bindings = ados_hid::sidecar::GroundStationInput::load(gs_input)
        .and_then(|g| g.bindings)
        .unwrap_or_else(ControlsDoc::ados_default);
    match GsInputState::load_fresh(state_path, now_ms) {
        Some(s) => json!({
            "native_sticks": s.native_sticks,
            "pilot_relay": s.pilot_relay,
            "streaming": s.streaming,
            "control_mode": s.control_mode,
            "control_mode_source": s.control_mode_source,
            "controller": s.controller,
            "bindings": bindings,
        }),
        None => json!({
            "native_sticks": false,
            "pilot_relay": false,
            "streaming": false,
            "control_mode": read_control_mode(state_path),
            "control_mode_source": "default",
            "controller": Value::Null,
            "bindings": bindings,
        }),
    }
}

/// `GET /api/v1/ground-station/input`.
pub async fn get_input(State(state): State<AppState>) -> Response {
    if !is_ground_station(&state) {
        return profile_mismatch();
    }
    let side = state
        .run_dir
        .join(ados_hid::pilot_input::status::GS_INPUT_STATE);
    let gs_input = crate::routes::gs_input_read::gs_input_json_path(&state);
    Json(input_body(&side, &gs_input, wall_ms())).into_response()
}

/// The `422` body for a refused document.
pub(crate) fn refused(e: &DocError) -> Response {
    let detail = match e {
        DocError::MissingLocked(missing) => json!({"error": {
            "code": "E_LOCKED_BINDINGS_MISSING",
            "message": "locked bindings can be moved but not removed",
            "missing": missing,
        }}),
        DocError::Invalid(m) => json!({"error": {"code": "E_INVALID_BINDINGS", "message": m}}),
    };
    (
        StatusCode::UNPROCESSABLE_ENTITY,
        Json(json!({"detail": detail})),
    )
        .into_response()
}

/// `PUT /api/v1/ground-station/input/bindings`.
pub async fn put_bindings(State(state): State<AppState>, Json(doc): Json<ControlsDoc>) -> Response {
    if !is_ground_station(&state) {
        return profile_mismatch();
    }
    if let Err(e) = doc.validate() {
        return refused(&e);
    }
    let sock = state.run_dir.join(ados_protocol::paths::HID_CMD_SOCK);
    let req = json!({"op": "set_bindings", "doc": doc});
    match crate::ipc::cmd::roundtrip_object(&sock, &req, crate::ipc::cmd::QUICK).await {
        Ok(obj) if obj.get("ok") == Some(&Value::Bool(true)) => {
            Json(json!({"ok": true, "bindings": doc})).into_response()
        }
        Ok(obj) => (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({"detail": {"error": Value::Object(obj)}})),
        )
            .into_response(),
        Err(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"detail": {"error": {
                "code": "E_BINDINGS_FAILED",
                "message": "input daemon unreachable",
            }}})),
        )
            .into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ados_hid::pilot_input::status::{ControlMode, Controller, ModeSource};

    #[test]
    fn fresh_sidecar_reports_native_sticks_and_the_controller() {
        let dir = tempfile::tempdir().unwrap();
        let side = dir.path().join("gs-input-state.json");
        let gs = dir.path().join("ground-station-input.json");
        GsInputState {
            v: 1,
            ts_ms: 50_000,
            native_sticks: true,
            pilot_relay: true,
            streaming: true,
            control_mode: ControlMode::Ados,
            control_mode_source: ModeSource::Drone,
            controller: Some(Controller {
                id: "usb:054c:09cc:event5".into(),
                name: "Wireless Controller".into(),
                connection: "bluetooth".into(),
                driver: "hid-playstation".into(),
                battery_pct: Some(70),
            }),
        }
        .save(&side)
        .unwrap();
        let b = input_body(&side, &gs, 51_000);
        assert_eq!(b["native_sticks"], true);
        assert_eq!(b["controller"]["name"], "Wireless Controller");
        assert_eq!(b["controller"]["battery_pct"], 70);
        assert_eq!(b["control_mode"], "ados");
        assert_eq!(b["bindings"]["kind"], "ados-controls");

        // Stale: not native, no controller, the last mode kept.
        let b = input_body(&side, &gs, 60_000);
        assert_eq!(b["native_sticks"], false);
        assert!(b["controller"].is_null());
        assert_eq!(b["control_mode"], "ados");
    }

    #[test]
    fn a_missing_locked_binding_is_a_422_listing_it() {
        let mut doc = ControlsDoc::ados_default();
        doc.bindings.retain(|b| b.action != "arm");
        let r = refused(&doc.validate().unwrap_err());
        assert_eq!(r.status(), StatusCode::UNPROCESSABLE_ENTITY);
    }
}
