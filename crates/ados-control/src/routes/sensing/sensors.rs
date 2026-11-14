//! `GET /api/v1/sensors`: the rig's live sensor table and the navigation
//! sensor health.
//!
//! The table joins the stream table `ados-clock` keeps of everything on the
//! sensor bus (`sensors.json`) with the rig document (roles, and rig sensors
//! not yet streaming), the hub link (`sensorhub.json`), the clock sigmas
//! (`clock.json`), the GNSS fix (`gnss.json`), the flight-controller link
//! (with the stream rates the router requested, `mavlink-router.json`) and the
//! scans the nav hub sees arrive (a rig lidar's measured rate). `health` is
//! [`super::health`].

use std::time::SystemTime;

use axum::extract::State;
use axum::Json;
use serde_json::{json, Map, Value};

use ados_protocol::sidecar::{read_fresh_at, Reading};
use ados_sensorbus::rig::{self, RigSummary};

use super::clock::{clock_body, source_sigma};
use super::health::{health_body, HealthInputs};
use super::{fresh_map, with_present, SensingPaths, FRESH};
use crate::routes::nav::{LidarView, NavKind};
use crate::state::AppState;

/// The flight-controller link as the sensors table reports it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct FcView {
    pub connected: bool,
    pub autopilot: Option<String>,
}

/// Whether `unit` runs on `profile` (underscore form).
pub(crate) fn unit_on(unit: &str, profile: &str) -> bool {
    ados_protocol::units::find(unit).is_some_and(|u| u.on_profile(profile))
}

pub async fn get_sensors(State(state): State<AppState>) -> Json<Value> {
    let paths = SensingPaths::from_state(&state);
    let profile = super::profile(&state);
    let fc = unit_on("ados-mavlink", &profile).then(|| {
        let snapshot = state.state.snapshot();
        let l = crate::routes::status::fc_liveness_from_snapshot(snapshot.as_ref());
        FcView {
            connected: crate::routes::status::derive_fc_reachable(&l),
            autopilot: l.fc_firmware.as_str().map(str::to_string),
        }
    });
    let lidar = state.nav.lidar_view();
    let pilot = state.nav.latest(NavKind::Pilot);
    let config = state.pairing_paths.config.clone();
    let body = tokio::task::spawn_blocking(move || {
        let now = SystemTime::now();
        let now_mono_ns = ados_sensorbus::time::mono_ns();
        let mut body = sensors_body(&paths, &profile, now, now_mono_ns, fc, &lidar);
        let inputs = HealthInputs {
            pilot,
            lidar,
            config_text: std::fs::read_to_string(config).unwrap_or_default(),
        };
        body["health"] = health_body(&paths, &inputs, now, now_mono_ns);
        body
    })
    .await
    .unwrap_or_else(|_| absent_body());
    Json(body)
}

/// The body with no sensor service running: every block empty.
fn absent_body() -> Value {
    let mut rest = Map::new();
    for key in ["rig_id", "calib_version", "hub", "fc"] {
        rest.insert(key.into(), Value::Null);
    }
    rest.insert("sensors".into(), json!([]));
    rest.insert("gnss".into(), Value::Null);
    with_present(false, rest)
}

/// The stored rig document and its summary, when it validates.
pub(crate) fn read_rig_doc(paths: &SensingPaths) -> Option<(Value, RigSummary)> {
    let text = std::fs::read_to_string(paths.rig()).ok()?;
    let doc: Value = serde_json::from_str(&text).ok()?;
    let summary = rig::validate(&doc).ok()?;
    Some((doc, summary))
}

/// The scan rate a rig sensor's lidar model declares.
fn declared_scan_hz(doc: &Value, id: &str) -> Option<f64> {
    doc["sensors"]
        .as_array()?
        .iter()
        .find(|s| s["id"] == id)?
        .get("lidar")?
        .get("rate_hz")?
        .as_f64()
}

/// The table for a node running `profile` (underscore form). `lidar` is what
/// the nav hub measured of the scans.
pub(crate) fn sensors_body(
    paths: &SensingPaths,
    profile: &str,
    now: SystemTime,
    now_mono_ns: u64,
    fc: Option<FcView>,
    lidar: &LidarView,
) -> Value {
    let Some(streams) = fresh_map(&paths.sidecar("sensors.json"), Some(FRESH), now) else {
        return absent_body();
    };
    let clock = clock_body(paths, now);
    let hub_side =
        read_fresh_at::<Map<String, Value>>(&paths.sidecar("sensorhub.json"), Some(FRESH), now);
    // The rates the hub was configured with, never the measured ones: the
    // measured rate is the stream's own `rate_hz`.
    let hub_rates = match &hub_side {
        Reading::Fresh(m) => (
            m.get("imu_odr_hz").and_then(Value::as_f64),
            m.get("trigger_hz_configured").and_then(Value::as_f64),
        ),
        _ => (None, None),
    };
    let hub = hub_block(hub_side);
    let (rig_doc, rig) = match read_rig_doc(paths) {
        Some((doc, summary)) => (doc, Some(summary)),
        None => (Value::Null, None),
    };
    // The per-stream rates the router asked the flight controller for, only
    // while its sidecar is inside the registry's freshness window.
    let fc_rates = fresh_map(
        &paths.sidecar("mavlink-router.json"),
        ados_protocol::paths::max_age("mavlink-router"),
        now,
    )
    .and_then(|mut m| m.remove("sensor_rates_hz"))
    .and_then(|v| match v {
        Value::Object(m) => Some(m),
        _ => None,
    });

    let mut sensors: Vec<Value> = Vec::new();
    let empty = Vec::new();
    let list = streams
        .get("streams")
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
        // The hub's per-IMU ODR applies to each hub IMU stream.
        let expected = match (origin, kind) {
            ("hub", "imu") => hub_rates.0,
            ("hub", "camera") => hub_rates.1,
            ("fc", _) => fc_rates
                .as_ref()
                .and_then(|r| r.get(id))
                .and_then(Value::as_f64),
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
    // Rig sensors that are not streaming on the bus are listed, honestly idle,
    // except the lidar: its scans reach the nav hub, so once one has arrived it
    // lists the measured rate and the last scan's age. GNSS without a hub
    // comes from `ados-gnss` where that service runs.
    let gnss_lane = unit_on("ados-gnss", profile);
    if let Some(r) = &rig {
        for rs in &r.sensors {
            if sensors.iter().any(|s| s["id"] == rs.id.as_str()) {
                continue;
            }
            let source = match rs.kind.as_str() {
                "camera" | "thermal" => "vision",
                "lidar" => "lidar",
                _ if hub.is_some() => "hub",
                "gnss" if gnss_lane => "gnss",
                _ => "fc",
            };
            let (rate, expected, age_ms) = if rs.kind == "lidar" {
                (
                    lidar.rate_hz.unwrap_or(0.0),
                    declared_scan_hz(&rig_doc, &rs.id),
                    lidar.age_ms,
                )
            } else {
                (0.0, None, None)
            };
            sensors.push(json!({
                "id": rs.id, "kind": rs.kind, "role": rs.role, "source": source,
                "rate_hz": rate, "expected_hz": expected, "last_t_ns": 0, "age_ms": age_ms,
                "sigma_ns": -1,
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
    with_present(true, rest)
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

#[cfg(test)]
#[path = "sensors_tests.rs"]
mod tests;
