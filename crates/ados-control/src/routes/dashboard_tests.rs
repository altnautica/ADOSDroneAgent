//! Dashboard snapshot slices: FC staleness, the video state verdict, and the
//! bitrate measured off mediamtx's `main` path.

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

#[test]
fn video_state_counts_roster_cameras_without_a_v4l2_node() {
    // A node whose only cameras are network sources: no V4L2 node, and `main`
    // not publishing, still has a camera.
    let captured =
        "cameras:\n  - id: front\n    source: rtsp://sim.invalid:8554/front\n    capture: true\n";
    let pulled = "cameras:\n  - id: front\n    source: rtsp://sim.invalid:8554/front\n";
    for text in [captured, pulled] {
        assert!(roster_declares_camera(text), "{text}");
        assert_eq!(
            video_state(false, false, roster_declares_camera(text)),
            "ready"
        );
        assert_eq!(
            video_state(true, false, roster_declares_camera(text)),
            "running"
        );
    }

    // No roster, an empty one, or only disabled entries: nothing to show.
    let disabled = "cameras:\n  - id: front\n    source: rtsp://sim.invalid:8554/front\n    capture: true\n    enabled: false\n";
    for text in ["", "cameras: []\n", disabled] {
        assert!(!roster_declares_camera(text), "{text:?}");
        assert_eq!(
            video_state(false, false, roster_declares_camera(text)),
            "no_camera"
        );
    }

    // A local device alone still counts.
    assert_eq!(video_state(false, true, false), "ready");
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
    let first = video_slice(&camera, "running", true, Some(&live_main(0)), &sample, t0);
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
    let second = video_slice(
        &camera,
        "running",
        true,
        Some(&live_main(500_000)),
        &sample,
        t1,
    );
    assert_eq!(second["bitrate_kbps"], json!(4000));

    // The stream drops: the slice falls back and the sample is forgotten,
    // so the next live reading starts a fresh pair.
    let dropped = video_slice(&camera, "ready", false, None, &sample, t1);
    assert_eq!(dropped["state"], json!("ready"));
    assert_eq!(dropped["codec"], json!("h264"));
    assert_eq!(dropped["bitrate_kbps"], Value::Null);
    let t2 = t1 + Duration::from_secs(1);
    let again = video_slice(
        &camera,
        "running",
        true,
        Some(&live_main(900_000)),
        &sample,
        t2,
    );
    assert_eq!(again["bitrate_kbps"], Value::Null);
}

#[test]
fn a_rewound_counter_is_not_a_rate() {
    let t0 = Instant::now();
    let t1 = t0 + Duration::from_secs(1);
    assert_eq!(measured_kbps((t0, 1_000), (t1, 10)), None);
    assert_eq!(measured_kbps((t0, 0), (t0, 1_000)), None);
}

#[test]
fn rc_rssi_255_is_unknown_and_the_byte_scales_to_percent() {
    assert_eq!(rc_rssi_percent(255.0), None);
    assert_eq!(rc_rssi_percent(254.0), Some(100));
    assert_eq!(rc_rssi_percent(127.0), Some(50));
    assert_eq!(rc_rssi_percent(50.0), Some(50));
    assert_eq!(rc_rssi_percent(0.0), Some(0));
}

#[test]
fn an_unknown_rc_rssi_reads_null() {
    let client = StateIpcClient::disconnected();
    let mut snap = flying();
    snap["rc"]["rssi"] = json!(255);
    client.set_aged_snapshot_for_test(snap, Duration::from_millis(100));
    let fc = fc_slice(client.snapshot().as_ref());
    assert!(fc.as_object().unwrap().contains_key("rc"));
    assert_eq!(fc["rc"], Value::Null);
}
