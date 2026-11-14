//! The rig writes: `PUT /api/v1/sensors/rig` (store a contract `rig.json`)
//! and `POST /api/v1/sensors/rig/lidar` (add the RPLIDAR C1 template sensor,
//! `data/rig-templates/rplidar-c1.json`, compiled in).
//!
//! Both validate against the contract, store atomically and then have the hub
//! service reload the rig. The lidar add creates a minimal rig holding only the
//! lidar when none is stored; a rig that already has a lidar, a sensor already
//! named `lidar0`, or a stored rig that does not validate is refused with 409
//! and left untouched.

use std::future::Future;
use std::sync::LazyLock;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};

use ados_sensorbus::rig;

use super::{write_atomic, SensingPaths};
use crate::state::AppState;

/// The unit command that makes the hub service re-read the rig extrinsics.
const RELOAD_HUB: [&str; 3] = ["--no-block", "reload-or-restart", "ados-sensorhub.service"];

const C1_TEMPLATE: &str = include_str!("../../../../../data/rig-templates/rplidar-c1.json");

/// The C1 template's rig sensor entry.
pub(crate) static C1_SENSOR: LazyLock<Value> = LazyLock::new(|| {
    let t: Value = serde_json::from_str(C1_TEMPLATE).expect("rplidar-c1.json parses");
    t["sensor"].clone()
});

/// The contract id a created rig carries.
const CONTRACT: &str = "ados.capture/1";

/// The calibration version of a rig created from a template alone.
const UNCALIBRATED: &str = "uncalibrated";

/// `PUT /api/v1/sensors/rig`: validate a contract `rig.json` and store it.
pub async fn put_rig(State(state): State<AppState>, body: axum::body::Bytes) -> Response {
    store_then_reload(
        SensingPaths::from_state(&state),
        move |p| put_rig_at(p, &body),
        systemctl,
    )
    .await
}

/// `POST /api/v1/sensors/rig/lidar`: add the C1 template lidar to the rig.
pub async fn post_rig_lidar(State(state): State<AppState>) -> Response {
    let device_id = crate::config::PairingConfig::cached(&state.pairing_paths.config)
        .agent
        .device_id
        .clone();
    let rig_id = if device_id.trim().is_empty() {
        "rig".to_string()
    } else {
        device_id
    };
    store_then_reload(
        SensingPaths::from_state(&state),
        move |p| add_lidar_at(p, &rig_id),
        systemctl,
    )
    .await
}

async fn systemctl(program: &'static str, args: &'static [&'static str]) -> bool {
    crate::probe::status_only(program, args, crate::probe::PROBE_TIMEOUT).await
}

/// Run the write `store` off the reactor, then, when it stored the rig, have
/// the hub service reload it through `run` (`systemctl` in production).
pub(crate) async fn store_then_reload<S, F, Fut>(paths: SensingPaths, store: S, run: F) -> Response
where
    S: FnOnce(&SensingPaths) -> (Response, bool) + Send + 'static,
    F: FnOnce(&'static str, &'static [&'static str]) -> Fut,
    Fut: Future<Output = bool>,
{
    let (resp, stored) = match tokio::task::spawn_blocking(move || store(&paths)).await {
        Ok(r) => r,
        Err(e) => {
            return refuse(
                StatusCode::INTERNAL_SERVER_ERROR,
                "E_RIG_WRITE",
                e.to_string(),
            )
            .0
        }
    };
    if stored && !run("systemctl", &RELOAD_HUB).await {
        tracing::warn!("sensor hub reload after the rig write failed");
    }
    resp
}

fn refuse(status: StatusCode, error: &str, detail: String) -> (Response, bool) {
    let body = Json(json!({"error": error, "detail": detail}));
    ((status, body).into_response(), false)
}

/// Validate `doc` and store it; the flag is true when the rig landed on disk.
/// `extra` joins the success body.
fn validate_and_store(
    paths: &SensingPaths,
    doc: &Value,
    invalid: StatusCode,
    extra: Option<(&str, Value)>,
) -> (Response, bool) {
    let summary = match rig::validate(doc) {
        Ok(s) => s,
        Err(e) => return refuse(invalid, "E_RIG_INVALID", e),
    };
    let pretty = serde_json::to_vec_pretty(doc).unwrap_or_default();
    if let Err(e) = write_atomic(&paths.rig(), &pretty) {
        return refuse(
            StatusCode::INTERNAL_SERVER_ERROR,
            "E_RIG_WRITE",
            e.to_string(),
        );
    }
    let mut ok =
        json!({"ok": true, "rig_id": summary.rig_id, "calib_version": summary.calib_version});
    if let Some((key, value)) = extra {
        ok[key] = value;
    }
    (Json(ok).into_response(), true)
}

/// Validate and store a whole rig body.
pub(crate) fn put_rig_at(paths: &SensingPaths, body: &[u8]) -> (Response, bool) {
    match serde_json::from_slice::<Value>(body) {
        Ok(doc) => validate_and_store(paths, &doc, StatusCode::BAD_REQUEST, None),
        Err(e) => refuse(
            StatusCode::BAD_REQUEST,
            "E_RIG_INVALID",
            format!("body is not JSON: {e}"),
        ),
    }
}

/// Add the C1 template lidar to the rig under `paths`; a new rig is named
/// `rig_id`.
pub(crate) fn add_lidar_at(paths: &SensingPaths, rig_id: &str) -> (Response, bool) {
    let sensor = C1_SENSOR.clone();
    let doc = match std::fs::read_to_string(paths.rig()) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => json!({
            "contract": CONTRACT,
            "rig_id": rig_id,
            "calib_version": UNCALIBRATED,
            "sensors": [sensor],
        }),
        Err(e) => {
            return refuse(
                StatusCode::INTERNAL_SERVER_ERROR,
                "E_RIG_READ",
                e.to_string(),
            )
        }
        Ok(text) => match with_lidar(&text, sensor) {
            Ok(doc) => doc,
            Err((error, detail)) => return refuse(StatusCode::CONFLICT, error, detail),
        },
    };
    validate_and_store(
        paths,
        &doc,
        StatusCode::INTERNAL_SERVER_ERROR,
        Some(("sensor", C1_SENSOR.clone())),
    )
}

/// The stored rig `text` with `sensor` appended, or why it is refused.
fn with_lidar(text: &str, sensor: Value) -> Result<Value, (&'static str, String)> {
    let mut doc: Value = serde_json::from_str(text)
        .map_err(|e| ("E_RIG_INVALID", format!("the stored rig is not JSON: {e}")))?;
    let summary = rig::validate(&doc).map_err(|e| {
        (
            "E_RIG_INVALID",
            format!("the stored rig does not validate: {e}"),
        )
    })?;
    if let Some(l) = summary.sensors.iter().find(|s| s.kind == "lidar") {
        return Err((
            "E_RIG_LIDAR_EXISTS",
            format!("the rig already has lidar {}", l.id),
        ));
    }
    let id = sensor["id"].as_str().unwrap_or_default();
    if summary.sensor(id).is_some() {
        return Err((
            "E_RIG_ID_TAKEN",
            format!("the rig already has a sensor named {id}"),
        ));
    }
    if let Some(list) = doc["sensors"].as_array_mut() {
        list.push(sensor);
    }
    Ok(doc)
}

#[cfg(test)]
#[path = "rig_write_tests.rs"]
mod tests;
