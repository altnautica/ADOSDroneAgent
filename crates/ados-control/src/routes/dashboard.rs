//! `GET /api/v1/dashboard/snapshot`: the agent webapp's one-pager poll (1 Hz).
//!
//! Three flat slices, each read from a source this process already holds:
//!
//! - **`video`** — the configured encoder settings of the stream served at
//!   `main` (the primary `cameras` leg, else `video.camera`, resolved as the
//!   video service resolves them), the readiness of mediamtx's `main` path, and a
//!   bitrate measured off that path's received-bytes counter. `bitrate_kbps` is
//!   null until two readings of a live stream exist, so a configured target never
//!   reads as a measured rate. A camera counts as present when a V4L2 node is
//!   enumerated or the `cameras` roster declares one the node runs without a
//!   local device (a vision-captured entry or a network pull), so an
//!   `rtsp://`-only node that is not publishing reads `ready`, not `no_camera`.
//! - **`fc`** — the vehicle snapshot the router publishes over the state socket,
//!   read through the state client, which drops a snapshot once the hub stops
//!   publishing. A hub that went quiet therefore reads as a disconnected FC with
//!   every heartbeat-derived field null, never as its last values.
//! - **`cloud`** — the server posture from config, the device id, and the live
//!   pairing code (only to a caller on the device's own networks, as
//!   `/api/pairing/info` serves it).
//!
//! A field with no source is null, never a default that reads as a measurement.

use std::time::Instant;

use axum::extract::{Extension, State};
use axum::Json;
use parking_lot::Mutex;
use serde_json::{json, Map, Value};

use ados_protocol::pairing_posture::CallerClass;
use ados_video::config::{CameraConfig, RosterVideoConfig};

use crate::config::PairingConfig;
use crate::mediamtx_probe;
use crate::routes::pairing;
use crate::state::AppState;

/// The last `bytesReceived` reading of the live `main` path and when it was
/// taken, so consecutive polls turn the cumulative counter into a rate. Cleared
/// whenever the stream is not live, so a later reading is never diffed against
/// a publisher that no longer exists.
static BITRATE_SAMPLE: Mutex<Option<(Instant, i64)>> = Mutex::new(None);

/// GPS_RAW_INT carries eph as HDOP x100 with 65535 meaning "unknown"; the router
/// publishes it divided by 100, so the unknown sentinel arrives as 655.35.
const HDOP_UNKNOWN: f64 = 655.35;

/// `GET /api/v1/dashboard/snapshot` → `{video, fc, cloud}`.
pub async fn get_dashboard_snapshot(
    State(state): State<AppState>,
    caller: Option<Extension<CallerClass>>,
) -> Json<Value> {
    let paths = &state.pairing_paths;

    let list = mediamtx_probe::read_paths_list().await;
    let main = list.as_ref().and_then(mediamtx_probe::main_path);
    let ready = main.is_some_and(mediamtx_probe::path_ready);
    let config_text = std::fs::read_to_string(&paths.config).unwrap_or_default();
    let camera = RosterVideoConfig::from_text(&config_text).primary_camera();
    let stream_state = video_state(
        ready,
        video_devices_present(),
        roster_declares_camera(&config_text),
    );
    let video = video_slice(
        &camera,
        stream_state,
        ready,
        main,
        &BITRATE_SAMPLE,
        Instant::now(),
    );

    let fc = fc_slice(state.state.snapshot().as_ref());

    let mode = ados_config::schema::effective(&paths.config)
        .ok()
        .and_then(|c| {
            c.pointer("/server/mode")
                .and_then(Value::as_str)
                .map(str::to_string)
        });
    let pairing_code = pairing::code_for_caller(paths, caller)
        .await
        .unwrap_or_default();
    let cloud = json!({
        "mode": mode,
        "drone_id": PairingConfig::load_from(&paths.config).agent.device_id,
        "pairing_code": pairing_code,
    });

    Json(json!({ "video": video, "fc": fc, "cloud": cloud }))
}

/// Any V4L2 node enumerated: the cheap kernel-side check that tells a node with
/// a local camera that is not yet publishing (`ready`) from one with none.
fn video_devices_present() -> bool {
    std::fs::read_dir("/sys/class/video4linux")
        .map(|mut entries| entries.next().is_some())
        .unwrap_or(false)
}

/// Whether the `cameras` roster in config `text` declares a camera that needs
/// no V4L2 node of its own: an enabled `capture: true` entry (the vision engine
/// opens its device or `rtsp://` URL) or an enabled network-URL entry the video
/// pipeline pulls. The same summary gates those services.
fn roster_declares_camera(text: &str) -> bool {
    let declared = ados_config::cameras::declared_cameras_from(text);
    declared.captured > 0 || declared.network
}

/// The dashboard `video.state`: `running` while `main` has a live publisher,
/// `ready` while a camera is present (a V4L2 node, or one the roster declares)
/// but `main` is not publishing, and `no_camera` only when neither exists.
fn video_state(ready: bool, devices_present: bool, roster_camera: bool) -> &'static str {
    if ready {
        "running"
    } else if devices_present || roster_camera {
        "ready"
    } else {
        "no_camera"
    }
}

/// The video slice. `state` is [`video_state`]'s verdict; `main` is mediamtx's
/// `main` path object when listed; `ready` its live-publisher verdict.
fn video_slice(
    camera: &CameraConfig,
    state: &'static str,
    ready: bool,
    main: Option<&Map<String, Value>>,
    sample: &Mutex<Option<(Instant, i64)>>,
    now: Instant,
) -> Value {
    let mut codec = camera.codec.clone();
    let mut bitrate_kbps = None;
    match main.filter(|_| ready) {
        Some(path) => {
            // The codec the publisher actually sent outranks the configured one.
            if let Some(live) = path
                .get("tracks")
                .and_then(Value::as_array)
                .and_then(|tracks| {
                    tracks
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::trim)
                        .find(|t| !t.is_empty())
                })
            {
                codec = live.to_string();
            }
            bitrate_kbps = path
                .get("bytesReceived")
                .and_then(Value::as_i64)
                .filter(|b| *b >= 0)
                .and_then(|bytes| {
                    let last = sample.lock().replace((now, bytes));
                    last.and_then(|last| measured_kbps(last, (now, bytes)))
                });
        }
        None => {
            sample.lock().take();
        }
    }
    json!({
        "codec": codec,
        "width": camera.width,
        "height": camera.height,
        "fps": camera.fps,
        "target_bitrate_kbps": Some(camera.bitrate_kbps).filter(|k| *k > 0),
        "state": state,
        "bitrate_kbps": bitrate_kbps,
        "glass_to_glass_ms": Value::Null,
    })
}

/// The rate between two `(when, bytesReceived)` readings in kbps, or `None` when
/// no time passed or the counter went backwards (the publisher restarted, so the
/// next pair of readings gives the rate).
fn measured_kbps(last: (Instant, i64), current: (Instant, i64)) -> Option<i64> {
    let elapsed = current.0.saturating_duration_since(last.0).as_secs_f64();
    if elapsed <= 0.0 || current.1 < last.1 {
        return None;
    }
    let kbps = ((current.1 - last.1) as f64 * 8.0 / 1000.0 / elapsed).round_ties_even();
    Some(kbps.max(0.0) as i64)
}

/// The FC slice from the state snapshot (`None` when the hub has not published
/// recently). The router's vehicle snapshot is zero-initialised: `armed` is
/// false and `mode` empty until the first HEARTBEAT, and the GPS block reads 0
/// until the first GPS_RAW_INT. Those defaults are not readings, so every
/// heartbeat-derived field is null until `last_heartbeat` is set, and the GPS
/// fields are null until a fix is reported.
fn fc_slice(snapshot: Option<&Value>) -> Value {
    let vehicle = snapshot.and_then(Value::as_object);
    let field = |key: &str| vehicle.and_then(|m| m.get(key));
    let block = |key: &str| field(key).and_then(Value::as_object);
    let gps = block("gps");
    let battery = block("battery");
    let gps_field = |key: &str| gps.and_then(|m| m.get(key)).cloned();
    let battery_field = |key: &str| {
        battery
            .and_then(|m| m.get(key))
            .cloned()
            .unwrap_or(Value::Null)
    };

    let last_heartbeat = field("last_heartbeat")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty());
    let heard = last_heartbeat.is_some();
    let heard_field = |key: &str| field(key).filter(|_| heard);
    let autopilot = heard_field("autopilot");
    let mav_type = heard_field("mav_type");

    let fix_type = gps_field("fix_type")
        .and_then(|v| v.as_i64())
        .filter(|f| *f > 0);
    let has_gps = fix_type.is_some();

    let (connected, fc_port, fc_baud) = pairing::fc_from_snapshot(snapshot);

    json!({
        "vehicle": enum_label(mav_type, mav_type_name, "vehicle"),
        "vehicle_id": mav_type.and_then(enum_id),
        "firmware": enum_label(autopilot, autopilot_name, "autopilot"),
        "firmware_id": autopilot.and_then(enum_id),
        "mode": heard_field("mode").and_then(Value::as_str).filter(|m| !m.is_empty()),
        // Only what a heartbeat said: an FC that has not reported is not
        // "disarmed", it is unknown.
        "armed": heard_field("armed").and_then(Value::as_bool),
        "gps": {
            "fix_type": fix_type,
            "satellites_visible": gps_field("satellites").filter(|_| has_gps),
            "hdop": gps_field("eph").filter(|_| has_gps).and_then(|e| hdop(&e)),
        },
        "battery": {
            "voltage": battery_field("voltage"),
            "remaining": battery_field("remaining"),
        },
        "rc": block("rc")
            .and_then(|m| m.get("rssi"))
            .and_then(Value::as_f64)
            .and_then(rc_rssi_percent),
        "fc_port": fc_port,
        "fc_baud": fc_baud,
        "connected": connected,
        "last_heartbeat": last_heartbeat,
    })
}

/// A MAVLink enum value as its integer id, when it is numeric.
fn enum_id(value: &Value) -> Option<i64> {
    value
        .as_i64()
        .or_else(|| value.as_f64().map(|f| f.trunc() as i64))
}

/// A MAVLink enum value as its operator-facing label: the table name, else
/// `"<noun> <id>"` for an id the table does not know, else the trimmed text of a
/// non-numeric value. `None` when nothing was reported.
fn enum_label(
    value: Option<&Value>,
    names: fn(i64) -> Option<&'static str>,
    noun: &str,
) -> Option<String> {
    let value = value?;
    let id = enum_id(value).or_else(|| value.as_str().and_then(|s| s.trim().parse().ok()));
    match id {
        Some(id) => Some(names(id).map_or_else(|| format!("{noun} {id}"), str::to_string)),
        None => value
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string),
    }
}

/// HDOP from the router's `gps.eph`, or `None` when the FC did not know it.
fn hdop(eph: &Value) -> Option<f64> {
    eph.as_f64().filter(|e| *e > 0.0 && *e < HDOP_UNKNOWN)
}

/// Normalize a raw MAVLink RC RSSI to 0-100 %, `None` when unknown (255, the
/// MAVLink "invalid/unknown" value). Some firmware sends a percent already
/// (0-100, passed through); ArduPilot fills the 0-254 byte where 254 is full
/// signal (scaled by 100/254). Clamped to 0-100.
fn rc_rssi_percent(raw: f64) -> Option<i64> {
    if raw >= 255.0 {
        return None;
    }
    if raw <= 0.0 {
        return Some(0);
    }
    if raw <= 100.0 {
        return Some(raw.round_ties_even() as i64);
    }
    Some(((raw * 100.0 / 254.0).round_ties_even() as i64).clamp(0, 100))
}

/// MAVLink `MAV_AUTOPILOT` → firmware label.
fn autopilot_name(id: i64) -> Option<&'static str> {
    Some(match id {
        0 => "Generic",
        3 => "ArduPilot",
        4 => "OpenPilot",
        5 => "Generic Waypoints Only",
        6 => "Generic Waypoints + Simple Nav",
        7 => "Generic Full Mission",
        8 => "Invalid",
        9 => "PPZ",
        10 => "UDB",
        11 => "FP",
        12 => "PX4",
        13 => "SMACCM",
        14 => "AutoQuad",
        15 => "Armazila",
        16 => "Aerob",
        17 => "ASLUAV",
        18 => "SmartAP",
        19 => "AirRails",
        20 => "ReflectronUDP",
        _ => return None,
    })
}

/// MAVLink `MAV_TYPE` → vehicle label. The agent SPA keys its firmware/metadata
/// catalog selection off these exact strings.
fn mav_type_name(id: i64) -> Option<&'static str> {
    Some(match id {
        0 => "Generic",
        1 => "Fixed Wing",
        2 => "Quadrotor",
        3 => "Coaxial",
        4 => "Helicopter",
        5 => "Antenna Tracker",
        6 => "GCS",
        7 => "Airship",
        8 => "Free Balloon",
        9 => "Rocket",
        10 => "Ground Rover",
        11 => "Surface Boat",
        12 => "Submarine",
        13 => "Hexarotor",
        14 => "Octorotor",
        15 => "Tricopter",
        16 => "Flapping Wing",
        17 => "Kite",
        18 => "Onboard Controller",
        19 => "VTOL Tailsitter Duo",
        20 => "VTOL Quadrotor",
        21 => "VTOL Tiltrotor",
        22..=26 | 29 => "VTOL Reserved",
        27 => "VTOL Tailsitter",
        28 => "VTOL Tiltwing",
        30 => "Gimbal",
        31 => "ADSB",
        32 => "Parafoil",
        33 => "Dodecarotor",
        34 => "Camera",
        35 => "Charging Station",
        36 => "FLARM",
        37 => "Servo",
        38 => "ODID",
        39 => "Decarotor",
        40 => "Battery",
        41 => "Parachute",
        _ => return None,
    })
}

#[cfg(test)]
#[path = "dashboard_tests.rs"]
mod tests;
