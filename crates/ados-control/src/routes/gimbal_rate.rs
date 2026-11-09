//! Gimbal rate control for an operator's hold-to-move pad.
//!
//! `POST /api/v1/gimbal/rate {"pitch_dps", "yaw_dps", "hold_ms"}` sends
//! `MAV_CMD_DO_GIMBAL_MANAGER_PITCHYAW` (1000) with both angles NaN (no angle
//! target) and the two rates, to the flight controller's gimbal manager. A
//! surface resends while the pad is held. A rate that is not renewed within
//! `hold_ms` is stopped here with a zero-rate command, so a surface that
//! disconnects or loses its link mid-move never leaves the gimbal turning.
//!
//! `GET /api/v1/gimbal/rate` answers `{"available", "reason"}` so a surface
//! shows the pad only on a vehicle that reports a gimbal: available needs a
//! connected MAVLink flight controller and a fresh gimbal attitude in the
//! state snapshot. Acceptance by the gimbal manager is not observed on this
//! path (`accepted: null`), the same as the camera-switch route.

use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};

use ados_protocol::mavlink::{self, MavHeader};

use crate::ipc::MavlinkIpcClient;
use crate::routes::detail;
use crate::state::AppState;

/// `MAV_CMD_DO_GIMBAL_MANAGER_PITCHYAW`.
const MAV_CMD_DO_GIMBAL_MANAGER_PITCHYAW: u16 = 1000;
/// The ground-control source identity every agent-built command carries.
const SOURCE_SYSTEM_ID: u8 = 255;
const SOURCE_COMPONENT_ID: u8 = 190;
/// The fastest rate a surface may ask for, degrees per second.
pub const MAX_RATE_DPS: f32 = 180.0;
/// The hold window a request may set, milliseconds.
pub const HOLD_MS_MIN: u64 = 100;
pub const HOLD_MS_MAX: u64 = 2000;
/// The hold window when the request names none.
const HOLD_MS_DEFAULT: u64 = 500;
/// A gimbal attitude older than this does not count as a gimbal present.
const GIMBAL_FRESH_MS: u64 = 1500;

/// The `POST` body.
#[derive(Debug, Deserialize)]
pub struct RateRequest {
    pub pitch_dps: f32,
    pub yaw_dps: f32,
    #[serde(default)]
    pub hold_ms: Option<u64>,
}

/// The seven `COMMAND_LONG` params for a rate: no angle target (NaN pitch and
/// yaw), the two rates, no flags (yaw follows the vehicle), every gimbal
/// device the manager controls.
pub fn rate_params(pitch_dps: f32, yaw_dps: f32) -> [f32; 7] {
    [f32::NAN, f32::NAN, pitch_dps, yaw_dps, 0.0, 0.0, 0.0]
}

/// The addressed `COMMAND_LONG` v2 frame for a rate.
pub fn rate_frame(pitch_dps: f32, yaw_dps: f32, (sys, comp): (u8, u8)) -> Vec<u8> {
    let header = MavHeader {
        system_id: SOURCE_SYSTEM_ID,
        component_id: SOURCE_COMPONENT_ID,
        sequence: 0,
    };
    mavlink::build_command_long_v2(
        header,
        MAV_CMD_DO_GIMBAL_MANAGER_PITCHYAW,
        sys,
        comp,
        rate_params(pitch_dps, yaw_dps),
    )
}

/// Why a request is refused before anything is sent.
pub fn validate(req: &RateRequest) -> Result<Duration, String> {
    for (name, v) in [("pitch_dps", req.pitch_dps), ("yaw_dps", req.yaw_dps)] {
        if !v.is_finite() || v.abs() > MAX_RATE_DPS {
            return Err(format!(
                "{name} must be a finite rate within ±{MAX_RATE_DPS} deg/s"
            ));
        }
    }
    let hold = req.hold_ms.unwrap_or(HOLD_MS_DEFAULT);
    if !(HOLD_MS_MIN..=HOLD_MS_MAX).contains(&hold) {
        return Err(format!(
            "hold_ms must be within {HOLD_MS_MIN}..={HOLD_MS_MAX}"
        ));
    }
    Ok(Duration::from_millis(hold))
}

/// The stop deadline: each request supersedes the one before it, and only the
/// latest request's window, unrenewed, sends the stop.
#[derive(Debug, Default, Clone)]
pub struct HoldTimer {
    generation: Arc<AtomicU64>,
}

impl HoldTimer {
    /// Record a new request. A moving rate arms a stop after `hold` that fires
    /// only if no later request arrived; a zero rate is itself the stop and
    /// cancels any pending one.
    pub fn renew<F, Fut>(&self, moving: bool, hold: Duration, stop: F)
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send,
    {
        let mine = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        if !moving {
            return;
        }
        let generation = self.generation.clone();
        tokio::spawn(async move {
            tokio::time::sleep(hold).await;
            if generation.load(Ordering::SeqCst) == mine {
                stop().await;
            }
        });
    }
}

/// The node's one hold timer (one gimbal manager per flight controller).
static HOLD_TIMER: LazyLock<HoldTimer> = LazyLock::new(HoldTimer::default);

/// Whether a surface should offer the pad, and why not when it should not.
pub fn availability(state: &AppState) -> (bool, Option<&'static str>) {
    if !state.fc_connected() {
        return (false, Some("flight controller not connected"));
    }
    if state.fc_speaks_msp() {
        return (false, Some("this flight controller speaks MSP"));
    }
    let fresh = state.state.snapshot().is_some_and(|s| {
        s.get("gimbal")
            .and_then(|g| g.get("age_ms"))
            .and_then(Value::as_u64)
            .is_some_and(|age| age <= GIMBAL_FRESH_MS)
    });
    if !fresh {
        return (false, Some("no gimbal reported by the flight controller"));
    }
    (true, None)
}

/// `GET /api/v1/gimbal/rate` → `{"available": bool, "reason": string|null}`.
pub async fn get_gimbal_rate(State(state): State<AppState>) -> Json<Value> {
    let (available, reason) = availability(&state);
    Json(json!({ "available": available, "reason": reason }))
}

/// Send one rate frame; `Err` is the socket error text.
async fn send_rate(
    client: &MavlinkIpcClient,
    pitch: f32,
    yaw: f32,
    target: (u8, u8),
) -> Result<(), String> {
    client
        .send(&rate_frame(pitch, yaw, target))
        .await
        .map_err(|e| e.to_string())
}

/// `POST /api/v1/gimbal/rate`: 200 `{"sent": true, "accepted": null,
/// "hold_ms"}`; 400 on a non-finite or out-of-range rate or hold; 501 on an
/// MSP flight controller; 503 when the flight controller or the MAVLink socket
/// is not there.
pub async fn post_gimbal_rate(
    State(state): State<AppState>,
    Json(req): Json<RateRequest>,
) -> Response {
    let hold = match validate(&req) {
        Ok(h) => h,
        Err(why) => return detail(StatusCode::BAD_REQUEST, why),
    };
    if !state.fc_connected() {
        return detail(StatusCode::SERVICE_UNAVAILABLE, "FC not connected");
    }
    if state.fc_speaks_msp() {
        return detail(
            StatusCode::NOT_IMPLEMENTED,
            "this flight controller speaks MSP (Betaflight/iNav); gimbal rate control needs MAVLink",
        );
    }
    let target = crate::fc_target::fc_target(&state);
    if let Err(e) = send_rate(&state.mavlink, req.pitch_dps, req.yaw_dps, target).await {
        tracing::warn!(error = %e, "gimbal rate send failed");
        return detail(StatusCode::SERVICE_UNAVAILABLE, "No MAVLink connection");
    }
    let moving = req.pitch_dps != 0.0 || req.yaw_dps != 0.0;
    let client = state.mavlink.clone();
    HOLD_TIMER.renew(moving, hold, move || async move {
        if let Err(e) = send_rate(&client, 0.0, 0.0, target).await {
            tracing::warn!(error = %e, "gimbal rate hold-expiry stop failed");
        } else {
            tracing::info!("gimbal_rate_hold_expired: stopped");
        }
    });
    (
        StatusCode::OK,
        Json(json!({
            "sent": true,
            "accepted": Value::Null,
            "hold_ms": hold.as_millis() as u64,
        })),
    )
        .into_response()
}

#[cfg(test)]
#[path = "gimbal_rate_tests.rs"]
mod tests;
