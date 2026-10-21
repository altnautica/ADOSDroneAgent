//! One coherent scenario: a paired ArduPilot copter on the bench, disarmed in
//! STABILIZE, and (for the Ground tab) the ground station it flies with.

use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::app::{App, Slot};
use crate::model::ProfileRoutes;
use crate::poll::{PollResult, ResourceId};
use crate::symbols;
use crate::theme::{Theme, ThemeId, Tier};
use crate::ui::tests::{feed, test_app};

pub(super) const LAN_IP: &str = "192.168.1.50";

/// The node's name for a profile.
fn name(profile: &str) -> &'static str {
    if profile == "ground_station" {
        "my-ground"
    } else {
        "my-drone"
    }
}

/// `/api/v1/setup/status` of a set-up, healthy node.
fn setup(profile: &str, i: usize) -> Value {
    let n = name(profile);
    let drone = profile == "drone";
    json!({
        "version": env!("CARGO_PKG_VERSION"), "device_name": n, "profile": profile, "paired": true,
        "access_urls": [
            {"kind": "setup", "label": "mDNS setup", "url": format!("http://{n}.local:8080/setup"), "primary": true},
            {"kind": "setup", "label": "LAN setup", "url": format!("http://{LAN_IP}:8080/setup")},
            {"kind": "mission_control", "label": "Mission Control", "url": "https://command.altnautica.com"},
            {"kind": "video", "label": "viewer", "url": format!("http://{n}.local:8889/main/")},
            {"kind": "mavlink", "label": "MAVLink WS", "url": format!("ws://{n}.local:8765/")}
        ],
        "mavlink": {"connected": drone, "port": "/dev/ttyACM0", "baud": 115200,
                    "tcp_url": format!("tcp://{n}.local:5760"), "websocket_url": format!("ws://{n}.local:8765/")},
        "video": {"state": "running", "whep_url": format!("http://{n}.local:8889/main/whep")},
        "network": {"hotspot_enabled": false},
        "cloud_choice": {"paired": true, "backend_url": "https://command.altnautica.com", "mode": "cloud"},
        "remote_access": {"status": "disabled"},
        "telemetry": {"mode": "STABILIZE", "armed": false, "battery": {"remaining": 92.0},
                      "gps": {"fix_type": 3, "satellites": 14}, "position": {"alt_rel": wave(i, 0.3, 0.12)}},
        "steps": [
            {"label": "Flight controller", "state": "complete", "detail": "ArduPilot copter on /dev/ttyACM0"},
            {"label": "Profile", "state": "complete", "detail": format!("Confirmed as {profile}")},
            {"label": "Pairing", "state": "complete", "detail": "Paired with Mission Control"},
            {"label": "Video", "state": "complete", "detail": "Main camera streaming"}
        ],
        "hardware_check": {"items": [
            {"id": "fc", "label": "FC", "required": true, "state": "ok"},
            {"id": "cam", "label": "Camera", "required": true, "state": "ok"},
            {"id": "radio", "label": "Radio", "required": true, "state": "ok"},
            {"id": "gps", "label": "GNSS", "required": false, "state": "ok"}
        ]}
    })
}

/// A smooth, deterministic wobble around `mid`.
pub(super) fn wave(i: usize, mid: f64, amp: f64) -> f64 {
    let t = i as f64;
    let v = mid + amp * ((t * 0.31).sin() * 0.7 + (t * 0.83).cos() * 0.3);
    (v * 100.0).round() / 100.0
}

/// The drone's sensor and clock routes (the ground station shows none on the
/// Ground tab).
fn routes(profile: &str) -> ProfileRoutes {
    if profile != "drone" {
        return ProfileRoutes::default();
    }
    let sensor = |id: &str, kind: &str, source: &str, hz: f64| {
        json!({"id": id, "kind": kind, "role": "nav", "source": source, "rate_hz": hz,
               "expected_hz": hz, "last_t_ns": 0, "age_ms": 1.0, "sigma_ns": 40000})
    };
    ProfileRoutes {
        sensors: Some(json!({
            "present": true,
            "hub": {"connected": true, "transport": "serial:/dev/ttyAMA0", "fw": "ados-sensorhub 0.1.0"},
            "fc": {"connected": true, "autopilot": "ardupilot", "highres_imu": true},
            "sensors": [sensor("imu0", "imu", "fc", 400.0), sensor("gnss0", "gnss", "fc", 10.0),
                        sensor("cam0", "camera", "vision", 30.0)],
            "gnss": {"fix": "3d", "sats": 14, "hdop": 0.7, "rtk": "none"}
        })),
        clock: Some(
            json!({"present": true, "timebase": "gps_pps", "locked": true, "sigma_ns": 40000,
                           "t_ns": 0, "utc_ns": null, "sources": []}),
        ),
        ..ProfileRoutes::default()
    }
}

fn poll(profile: &str, i: usize) -> PollResult {
    let services: Vec<Value> = (0..11).map(|_| json!({"state": "active"})).collect();
    PollResult {
        setup: Ok(setup(profile, i)),
        fc_status: (profile == "drone").then(
            || json!({"transportOpen": true, "mavlinkAlive": true, "fcFirmware": "ardupilot"}),
        ),
        status_full: Some(json!({ "services": services })),
        profile: routes(profile),
    }
}

/// A fresh app on `profile`, drawn in `theme` with Unicode symbols, after a
/// minute of healthy polls (so the overview trends have history).
pub(super) fn base(profile: &str, theme: ThemeId) -> App {
    let mut app = test_app();
    feed(&mut app, setup(profile, 0), routes(profile), Duration::ZERO);
    for i in 1..60 {
        app.core
            .view
            .apply(poll(profile, i), Instant::now(), "12:00:00".into());
    }
    *app.core.latest.lock().unwrap() = Some(env!("CARGO_PKG_VERSION").into());
    app.core.now = Instant::now();
    app.core.tier = Tier::Truecolor;
    app.core.theme = Theme::new(Tier::Truecolor, theme, false);
    app.core.sym = &symbols::UNICODE;
    app
}

/// Store `value` as the latest read of `id`.
pub(super) fn put(app: &mut App, id: ResourceId, value: Value) {
    let slot = Slot {
        value: Some(value),
        error: None,
        code: None,
        error_body: None,
        at: Instant::now(),
    };
    app.core.res.insert(id, slot);
}

/// Store `value` and hand it to the active tab, as a poll would.
pub(super) fn land(app: &mut App, id: ResourceId, value: Value) {
    put(app, id.clone(), value);
    let profile = app.core.profile().to_string();
    app.tabs
        .get_mut(app.core.tab, &profile)
        .on_resource(&mut app.core, &id);
}

pub(super) fn snapshot() -> Value {
    json!({"fc": {"vehicle": "Quadrotor", "firmware": "ArduPilot", "connected": true, "gps": {"hdop": 0.7}}})
}

pub(super) fn telemetry(i: usize) -> Value {
    json!({
        "armed": false, "mode": "STABILIZE",
        "attitude": {"roll": wave(i, 0.01, 0.01), "pitch": wave(i + 7, -0.01, 0.01), "yaw": 1.62},
        "position": {"lat": 47.397742, "lon": 8.545594, "alt_rel": wave(i, 0.3, 0.12), "alt_msl": 488.3, "heading": 93.0},
        "position_age_ms": 110,
        "velocity": {"groundspeed": wave(i, 0.05, 0.04), "climb": wave(i + 3, 0.0, 0.05)},
        "battery": {"voltage": wave(i, 16.62, 0.03), "current": wave(i + 5, 0.45, 0.08), "remaining": 92,
                    "temperature": null, "cell_voltages": [4.16, 4.15, 4.16, 4.15]},
        "gps": {"fix_type": 3, "satellites": 14, "eph": 0.7}
    })
}

pub(super) fn battery() -> Value {
    json!({
        "enabled": true, "stale": false, "thresholds": {"reserve_percent": 25},
        "packs": [{
            "id": 0, "source": "mavlink", "stale": false, "cells_plausible": true,
            "cell_voltages_v": [4.16, 4.15, 4.16, 4.15], "weakest_cell_index": 1,
            "divergence_mv": 10.0, "voltage_v": 16.62, "current_a": 0.45,
            "remaining_pct": 92, "prediction": {"state": "normal", "eta_s": 1260}, "anomalies": []
        }]
    })
}

pub(super) fn wfb(i: usize) -> Value {
    json!({
        "state": "connected", "interface": "wlan1", "channel": 149, "actual_channel": 149,
        "frequency_mhz": 5745, "bandwidth_mhz": 20, "adapter": {"driver": "", "chipset": ""},
        "adapter_chipset": "RTL8812AU", "rssi_dbm": wave(i, -52.0, 4.0), "snr_db": wave(i + 4, 27.0, 3.0),
        "noise_dbm": -80.0, "packets_received": 1800 + i * 180, "loss_percent": wave(i + 9, 0.6, 0.5),
        "fec_recovered": i * 3, "fec_failed": 0, "bitrate_kbps": wave(i, 7800.0, 900.0) as u64,
        "restart_count": 0, "tx_power_dbm": 10, "tx_power_max_dbm": 20, "mcs_index": 1,
        "regulatory_domain": "US", "link_diag": "healthy", "profile": "drone"
    })
}

/// The LoRa status after `n` beacon intervals (the counters move between reads).
pub(super) fn lora(n: u64) -> Value {
    json!({
        "present": true,
        "region": {"code": "EU", "name": "Europe", "band_mhz": [863.0, 870.0]},
        "channel": {"freq_mhz": 868.1, "bw_khz": 125, "sf": 9, "erp_mw": 25.0, "max_erp_mw": 25.0},
        "tx_allowed": true, "blocked_reason": null,
        "governor": {"rules": [{"kind": "duty_cycle", "limit_pct": 1.0, "used_pct": 0.18, "sub_band": "868.0-868.6"}]},
        "tx": {"frames": 120 + n * 2, "refused": 0}, "rx": {"frames": 342 + n * 5, "crc_errors": 2}
    })
}

/// Wall-clock milliseconds, for "heard N s ago" style fields.
pub(super) fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

pub(super) fn lora_beacons() -> Value {
    let now = now_ms();
    json!({"beacons": [
        {"node_id": 7, "mode": "LOITER", "battery_pct": 81, "rssi_dbm": -95, "snr_db": 7.5, "received_at_ms": now - 2_000},
        {"node_id": 12, "mode": "AUTO", "battery_pct": 64, "rssi_dbm": -103, "snr_db": 3.0, "received_at_ms": now - 9_000}
    ]})
}

pub(super) fn video() -> [(ResourceId, Value); 3] {
    [
        (
            ResourceId::VideoConfig,
            json!({
                "radio": {"channel": 149, "band": "u-nii-3", "mcs_index": 1, "fec_k": 8, "fec_n": 12, "tx_power_dbm": 10, "preset": "balanced"},
                "encoder": {"bitrate_kbps": 6000, "width": 1280, "height": 720, "fps": 30, "codec": "h264"},
                "adaptive": {"available": true}
            }),
        ),
        (
            ResourceId::VideoCameras,
            json!({"cameras": [
                {"device_path": "/dev/video0", "type": "csi", "label": "Main camera", "width": 1920, "height": 1080},
                {"device_path": "/dev/video2", "type": "usb", "label": "Thermal", "width": 640, "height": 512}
            ], "assignments": {}}),
        ),
        (
            ResourceId::SetupStatus,
            json!({"video": {"state": "running", "whep_url": "http://my-drone.local:8889/main/whep",
                             "hls_url": "http://my-drone.local:8888/main/index.m3u8"},
                   "network": {"mdns_host": "my-drone.local", "api_port": 8080}}),
        ),
    ]
}

pub(super) fn latency(i: usize) -> Value {
    json!({"latency_ms": wave(i + 2, 92.0, 9.0), "ewma_ms": wave(i, 88.0, 6.0), "samples": 40 + i, "source": "sei"})
}

/// A copter's parameter set, as `GET /api/params` reports a finished sweep.
pub(super) const PARAMS: [(&str, f64); 30] = [
    ("ATC_ACCEL_P_MAX", 110000.0),
    ("ATC_ACCEL_R_MAX", 110000.0),
    ("ATC_ACCEL_Y_MAX", 27000.0),
    ("ATC_ANG_PIT_P", 4.5),
    ("ATC_ANG_RLL_P", 4.5),
    ("ATC_ANG_YAW_P", 4.5),
    ("ATC_RAT_PIT_D", 0.0036),
    ("ATC_RAT_PIT_I", 0.135),
    ("ATC_RAT_PIT_P", 0.135),
    ("ATC_RAT_RLL_D", 0.0036),
    ("ATC_RAT_RLL_I", 0.135),
    ("ATC_RAT_RLL_P", 0.135),
    ("ATC_RAT_YAW_I", 0.018),
    ("ATC_RAT_YAW_P", 0.18),
    ("ATC_THR_MIX_MAN", 0.1),
    ("BATT_CAPACITY", 5200.0),
    ("BATT_CRT_VOLT", 13.6),
    ("BATT_LOW_VOLT", 14.0),
    ("BATT_MONITOR", 4.0),
    ("COMPASS_USE", 1.0),
    ("FENCE_ALT_MAX", 120.0),
    ("FENCE_ENABLE", 1.0),
    ("FENCE_RADIUS", 300.0),
    ("GPS_TYPE", 1.0),
    ("INS_GYRO_FILTER", 20.0),
    ("LOIT_SPEED", 1250.0),
    ("PILOT_SPEED_UP", 250.0),
    ("RTL_ALT", 1500.0),
    ("WPNAV_SPEED", 500.0),
    ("WPNAV_SPEED_UP", 250.0),
];

pub(super) fn params() -> Value {
    let map: serde_json::Map<String, Value> = PARAMS
        .iter()
        .map(|(n, v)| (n.to_string(), json!(v)))
        .collect();
    let n = PARAMS.len();
    json!({"params": map, "count": n, "cached": n, "priming": false, "priming_timeout": false,
           "priming_send_failed": false, "progress": {"got": n, "expected": n}})
}
