//! `GET /api/v1/sensors` (the rig's live sensor table) and
//! `PUT /api/v1/sensors/rig` (store a contract `rig.json`).
//!
//! The table joins the stream table `ados-clock` keeps of everything on the
//! sensor bus (`sensors.json`) with the rig document (roles, and rig sensors
//! not yet streaming), the hub link (`sensorhub.json`), the clock sigmas
//! (`clock.json`), the GNSS fix (`gnss.json`) and the flight-controller link.

use std::time::SystemTime;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Map, Value};

use ados_protocol::sidecar::{read_fresh_at, Reading};
use ados_sensorbus::rig::{self, RigSummary};

use super::clock::{clock_body, source_sigma};
use super::{fresh_map, with_present, write_atomic, SensingPaths, FRESH};
use crate::state::AppState;

/// The flight-controller link as the sensors table reports it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct FcView {
    pub connected: bool,
    pub autopilot: Option<String>,
}

pub async fn get_sensors(State(state): State<AppState>) -> Json<Value> {
    let paths = SensingPaths::from_state(&state);
    let profile = super::profile(&state);
    let fc_lane =
        ados_protocol::units::find("ados-mavlink").is_some_and(|u| u.on_profile(&profile));
    let fc = fc_lane.then(|| {
        let snapshot = state.state.snapshot();
        let l = crate::routes::status::fc_liveness_from_snapshot(snapshot.as_ref());
        FcView {
            connected: crate::routes::status::derive_fc_reachable(&l),
            autopilot: l.fc_firmware.as_str().map(str::to_string),
        }
    });
    Json(sensors_body(
        &paths,
        SystemTime::now(),
        ados_sensorbus::time::mono_ns(),
        fc,
    ))
}

pub(crate) fn read_rig(paths: &SensingPaths) -> Option<RigSummary> {
    let text = std::fs::read_to_string(paths.rig()).ok()?;
    let doc: Value = serde_json::from_str(&text).ok()?;
    rig::validate(&doc).ok()
}

pub(crate) fn sensors_body(
    paths: &SensingPaths,
    now: SystemTime,
    now_mono_ns: u64,
    fc: Option<FcView>,
) -> Value {
    let streams = fresh_map(&paths.sidecar("sensors.json"), Some(FRESH), now);
    let present = streams.is_some();
    let clock = clock_body(paths, now);
    let hub_side =
        read_fresh_at::<Map<String, Value>>(&paths.sidecar("sensorhub.json"), Some(FRESH), now);
    let hub_rates = match &hub_side {
        Reading::Fresh(m) => (
            m.get("imu_odr_hz").and_then(Value::as_f64),
            m.get("trigger_hz").and_then(Value::as_f64),
        ),
        _ => (None, None),
    };
    let hub = hub_block(hub_side);
    let rig = read_rig(paths);

    let mut sensors: Vec<Value> = Vec::new();
    let empty = Vec::new();
    let list = streams
        .as_ref()
        .and_then(|m| m.get("streams"))
        .and_then(Value::as_array)
        .unwrap_or(&empty);
    let mut highres = false;
    for s in list {
        let origin = s["origin"].as_str().unwrap_or("host");
        if origin == "host" {
            continue;
        }
        let id = s["src"].as_str().unwrap_or_default();
        let kind = match s["kind"].as_str().unwrap_or_default() {
            "trigger" => "camera",
            k @ ("imu" | "gnss" | "baro" | "mag") => k,
            _ => continue,
        };
        let rate = s["rate_hz"].as_f64().unwrap_or(0.0);
        if origin == "fc" && id == "fc_imu" && rate > 0.0 {
            highres = true;
        }
        let expected = match (origin, kind, id) {
            ("hub", "imu", _) => hub_rates.0,
            ("hub", "camera", _) => hub_rates.1,
            ("fc", "imu", "fc_imu") => Some(200.0),
            ("fc", "imu", "fc_raw_imu") => Some(50.0),
            ("fc", "baro", _) => Some(10.0),
            _ => None,
        };
        let last_t = s["last_t_ns"].as_u64().unwrap_or(0);
        let last_rx = s["last_rx_ns"].as_u64().unwrap_or(0);
        let age_ms =
            (last_rx > 0 && now_mono_ns >= last_rx).then(|| (now_mono_ns - last_rx) as f64 / 1e6);
        sensors.push(json!({
            "id": id,
            "kind": kind,
            "role": role(rig.as_ref(), id, kind),
            "source": origin,
            "rate_hz": rate,
            "expected_hz": expected,
            "last_t_ns": last_t,
            "age_ms": age_ms,
            "sigma_ns": source_sigma(&clock, origin),
        }));
    }
    // Rig sensors that are not streaming yet are listed, honestly idle.
    if let Some(r) = &rig {
        for rs in &r.sensors {
            if sensors.iter().any(|s| s["id"] == rs.id.as_str()) {
                continue;
            }
            let source = match rs.kind.as_str() {
                "camera" | "thermal" => "vision",
                "lidar" => "lidar",
                _ if hub.is_some() => "hub",
                _ => "fc",
            };
            sensors.push(json!({
                "id": rs.id, "kind": rs.kind, "role": rs.role, "source": source,
                "rate_hz": 0.0, "expected_hz": null, "last_t_ns": 0, "age_ms": null, "sigma_ns": -1,
            }));
        }
    }

    let fc = fc.map(
        |f| json!({"connected": f.connected, "autopilot": f.autopilot, "highres_imu": highres}),
    );
    let mut rest = Map::new();
    rest.insert(
        "rig_id".into(),
        json!(rig.as_ref().map(|r| r.rig_id.clone())),
    );
    rest.insert(
        "calib_version".into(),
        json!(rig.as_ref().map(|r| r.calib_version.clone())),
    );
    rest.insert("hub".into(), hub.unwrap_or(Value::Null));
    rest.insert("fc".into(), fc.unwrap_or(Value::Null));
    rest.insert("sensors".into(), Value::Array(sensors));
    rest.insert("gnss".into(), gnss_summary(paths, now));
    with_present(present, rest)
}

fn role(rig: Option<&RigSummary>, id: &str, kind: &str) -> String {
    if let Some(s) = rig.and_then(|r| r.sensor(id)) {
        return s.role.clone();
    }
    match kind {
        "camera" => "primary",
        _ => "nav",
    }
    .to_string()
}

/// The hub link: `None` when no hub service has run (not configured); a
/// stale sidecar is a hub that is configured but not connected.
fn hub_block(side: Reading<Map<String, Value>>) -> Option<Value> {
    let (m, live) = match side {
        Reading::Fresh(m) => (m, true),
        Reading::Stale(m) => (m, false),
        _ => return None,
    };
    let g = |k: &str| m.get(k).cloned().unwrap_or(Value::Null);
    Some(json!({
        "connected": live && m.get("connected").and_then(Value::as_bool).unwrap_or(false),
        "transport": g("transport"),
        "fw": g("fw"),
        "board": g("board"),
        "proto_ver": g("proto_ver"),
        "uptime_s": g("uptime_s"),
        "crc_errors": m.get("crc_errors").cloned().unwrap_or(json!(0)),
        "dropped": m.get("dropped").cloned().unwrap_or(json!(0)),
    }))
}

/// The first receiver's fix summary, or null with no live GNSS.
fn gnss_summary(paths: &SensingPaths, now: SystemTime) -> Value {
    let Some(g) = fresh_map(&paths.sidecar("gnss.json"), Some(FRESH), now) else {
        return Value::Null;
    };
    match g
        .get("receivers")
        .and_then(Value::as_array)
        .and_then(|r| r.first())
    {
        Some(r) => json!({"fix": r["fix"], "sats": r["sats"], "hdop": r["hdop"], "rtk": r["rtk"]}),
        None => Value::Null,
    }
}

/// `PUT /api/v1/sensors/rig`: validate a contract `rig.json` and store it.
pub async fn put_rig(State(state): State<AppState>, body: axum::body::Bytes) -> Response {
    put_rig_at(&SensingPaths::from_state(&state), &body)
}

pub(crate) fn put_rig_at(paths: &SensingPaths, body: &[u8]) -> Response {
    let invalid = |detail: String| {
        (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "E_RIG_INVALID", "detail": detail})),
        )
            .into_response()
    };
    let doc: Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(e) => return invalid(format!("body is not JSON: {e}")),
    };
    let summary = match rig::validate(&doc) {
        Ok(s) => s,
        Err(e) => return invalid(e),
    };
    let pretty = serde_json::to_vec_pretty(&doc).unwrap_or_default();
    if let Err(e) = write_atomic(&paths.rig(), &pretty) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": "E_RIG_WRITE", "detail": e.to_string()})),
        )
            .into_response();
    }
    Json(json!({"ok": true, "rig_id": summary.rig_id, "calib_version": summary.calib_version}))
        .into_response()
}

#[cfg(test)]
#[path = "sensors_tests.rs"]
mod tests;
