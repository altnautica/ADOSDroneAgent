//! `GET /api/v1/dashboard/snapshot`: the agent webapp's one-pager poll (1 Hz).
//!
//! Three flat slices, each read from a source this process already holds:
//!
//! - **`video`** — the configured encoder settings of the stream served at
//!   `main` (the primary `video.cameras` leg, else `video.camera`, resolved as the
//!   video service resolves them), the readiness of mediamtx's `main` path, and a
//!   bitrate measured off that path's received-bytes counter. `bitrate_kbps` is
//!   null until two readings of a live stream exist, so a configured target never
//!   reads as a measured rate.
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
use crate::routes::{config_rw, pairing};
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
    let camera = RosterVideoConfig::load_from(&paths.config).primary_camera();
    let video = video_slice(
        &camera,
        ready,
        main,
        video_devices_present(),
        &BITRATE_SAMPLE,
        Instant::now(),
    );

    let fc = fc_slice(state.state.snapshot().as_ref());

    let mode = config_rw::effective_config(&paths.config)
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
/// a camera that is not yet publishing (`ready`) from one with none.
fn video_devices_present() -> bool {
    std::fs::read_dir("/sys/class/video4linux")
        .map(|mut entries| entries.next().is_some())
        .unwrap_or(false)
}

/// The video slice. `main` is mediamtx's `main` path object when listed;
/// `ready` its live-publisher verdict.
fn video_slice(
    camera: &CameraConfig,
    ready: bool,
    main: Option<&Map<String, Value>>,
    devices_present: bool,
    sample: &Mutex<Option<(Instant, i64)>>,
    now: Instant,
) -> Value {
    let state = if ready {
        "running"
    } else if devices_present {
        "ready"
    } else {
        "no_camera"
    };
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
            .map(rc_rssi_percent),
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

/// Normalize a raw MAVLink RC RSSI to 0-100 %. Some firmware sends a percent
/// already (0-100, passed through); ArduPilot fills the 0-254 byte where 254 is
/// full signal (scaled by 100/254). Clamped to 0-100.
fn rc_rssi_percent(raw: f64) -> i64 {
    if raw <= 0.0 {
        return 0;
    }
    if raw <= 100.0 {
        return raw.round_ties_even() as i64;
    }
    ((raw * 100.0 / 254.0).round_ties_even() as i64).clamp(0, 100)
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
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::ipc::state_client::STATE_STALE_AFTER;
    use crate::ipc::StateIpcClient;

    fn flying() -> Value {
        json!({
            "fc_connected": true,
            "fc_port": "/dev/ttyACM0",
            "fc_baud": 115200,
            "autopilot": 3,
            "mav_type": 2,
            "armed": true,
            "mode": "LOITER",
            "last_heartbeat": "2026-09-24T10:00:00Z",
            "gps": {"fix_type": 3, "satellites": 14, "eph": 0.8, "epv": 1.2},
            "battery": {"voltage": 16.2, "remaining": 88},
            "rc": {"rssi": 254},
        })
    }

    #[test]
    fn a_hub_that_stopped_publishing_reads_as_a_disconnected_fc() {
        // The last frame said the vehicle was armed and flying; it is older
        // than the staleness window, so none of it may be reported.
        let client = StateIpcClient::disconnected();
        client.set_aged_snapshot_for_test(flying(), STATE_STALE_AFTER + Duration::from_secs(1));
        let fc = fc_slice(client.snapshot().as_ref());
        assert_eq!(fc["connected"], json!(false));
        for key in [
            "vehicle",
            "firmware",
            "mode",
            "armed",
            "rc",
            "fc_port",
            "fc_baud",
            "last_heartbeat",
        ] {
            assert_eq!(fc[key], Value::Null, "{key}");
        }
        assert_eq!(
            fc["gps"],
            json!({"fix_type": null, "satellites_visible": null, "hdop": null})
        );
        assert_eq!(fc["battery"], json!({"voltage": null, "remaining": null}));

        // The same frame while fresh is reported in full.
        client.set_aged_snapshot_for_test(flying(), Duration::from_millis(100));
        let fc = fc_slice(client.snapshot().as_ref());
        assert_eq!(fc["connected"], json!(true));
        assert_eq!(fc["vehicle"], json!("Quadrotor"));
        assert_eq!(fc["firmware"], json!("ArduPilot"));
        assert_eq!(fc["armed"], json!(true));
        assert_eq!(fc["mode"], json!("LOITER"));
        assert_eq!(
            fc["gps"],
            json!({"fix_type": 3, "satellites_visible": 14, "hdop": 0.8})
        );
        assert_eq!(fc["rc"], json!(100));
    }

    #[test]
    fn heartbeat_fields_stay_unknown_until_a_heartbeat_lands() {
        // The router's zero-initialised snapshot: link up, nothing heard yet.
        let snap = json!({
            "fc_connected": true,
            "armed": false,
            "mode": "",
            "autopilot": 0,
            "mav_type": 0,
            "last_heartbeat": "",
            "gps": {"fix_type": 0, "satellites": 0, "eph": 0.0},
        });
        let fc = fc_slice(Some(&snap));
        assert_eq!(fc["connected"], json!(true));
        assert_eq!(fc["armed"], Value::Null, "not heard is not disarmed");
        assert_eq!(fc["vehicle"], Value::Null);
        assert_eq!(fc["firmware_id"], Value::Null);
        assert_eq!(fc["gps"]["satellites_visible"], Value::Null);
    }

    #[test]
    fn the_unknown_hdop_sentinel_is_not_a_reading() {
        let mut snap = flying();
        snap["gps"]["eph"] = json!(655.35);
        assert_eq!(fc_slice(Some(&snap))["gps"]["hdop"], Value::Null);
    }

    fn live_main(bytes: i64) -> Map<String, Value> {
        json!({"name": "main", "ready": true, "source": {}, "tracks": ["H265"], "bytesReceived": bytes})
            .as_object()
            .unwrap()
            .clone()
    }

    #[test]
    fn bitrate_is_measured_across_two_live_readings_and_reset_when_the_stream_drops() {
        let camera = CameraConfig::default();
        let sample = Mutex::new(None);
        let t0 = Instant::now();
        let first = video_slice(&camera, true, Some(&live_main(0)), true, &sample, t0);
        assert_eq!(first["state"], json!("running"));
        assert_eq!(
            first["codec"],
            json!("H265"),
            "the live codec outranks config"
        );
        assert_eq!(
            first["bitrate_kbps"],
            Value::Null,
            "one reading is not a rate"
        );
        assert_eq!(first["target_bitrate_kbps"], json!(4000));

        let t1 = t0 + Duration::from_secs(1);
        let second = video_slice(&camera, true, Some(&live_main(500_000)), true, &sample, t1);
        assert_eq!(second["bitrate_kbps"], json!(4000));

        // The stream drops: the slice falls back and the sample is forgotten,
        // so the next live reading starts a fresh pair.
        let dropped = video_slice(&camera, false, None, true, &sample, t1);
        assert_eq!(dropped["state"], json!("ready"));
        assert_eq!(dropped["codec"], json!("h264"));
        assert_eq!(dropped["bitrate_kbps"], Value::Null);
        let t2 = t1 + Duration::from_secs(1);
        let again = video_slice(&camera, true, Some(&live_main(900_000)), true, &sample, t2);
        assert_eq!(again["bitrate_kbps"], Value::Null);

        let none = video_slice(&camera, false, None, false, &sample, t2);
        assert_eq!(none["state"], json!("no_camera"));
    }

    #[test]
    fn a_rewound_counter_is_not_a_rate() {
        let t0 = Instant::now();
        let t1 = t0 + Duration::from_secs(1);
        assert_eq!(measured_kbps((t0, 1_000), (t1, 10)), None);
        assert_eq!(measured_kbps((t0, 0), (t0, 1_000)), None);
    }
}
