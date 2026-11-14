use super::*;
use crate::routes::nav::LidarStatus;
use std::time::Duration;

fn paths(dir: &std::path::Path) -> SensingPaths {
    SensingPaths {
        run_dir: dir.join("run"),
        etc_dir: dir.join("etc"),
    }
}

fn write(path: std::path::PathBuf, body: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, body).unwrap();
}

fn keys(v: &Value) -> Vec<&str> {
    let mut k: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
    k.sort_unstable();
    k
}

fn pilot_frame() -> Value {
    json!({"owner": "pilot", "sensors": {
        "flow": {"state": "low", "quality": 90, "rate_hz": 19.6, "age_ms": 30},
        "range": {"state": "ok", "height_m": 1.42, "age_ms": 25},
        "lidar": {"state": "ok", "rate_hz": 10.0, "health": "good", "age_ms": 60}}})
}

const MOUNT_CONFIG: &str = "
agent:
  name: x500
pilot:
  fc_sensors:
    flow:
      pos_m: [0.05, 0.0, 0.03]
      orient_yaw_cdeg: 9000
    rangefinder:
      pos_m: [0.06, 0.0, 0.03]
";

#[test]
fn the_health_block_carries_flow_range_lidar_and_cameras() {
    let dir = tempfile::tempdir().unwrap();
    let inputs = HealthInputs {
        pilot: Some((pilot_frame(), 100)),
        lidar: LidarView::default(),
        config_text: MOUNT_CONFIG.into(),
    };
    let h = health_body(&paths(dir.path()), &inputs, SystemTime::now(), 0);
    assert_eq!(keys(&h), ["cameras", "flow", "lidar", "range"]);
    assert_eq!(
        keys(&h["flow"]),
        ["age_ms", "mount", "quality", "rate_hz", "state"]
    );
    assert_eq!(h["flow"]["state"], "low");
    assert_eq!(h["flow"]["quality"], 90);
    assert_eq!(h["flow"]["rate_hz"], 19.6);
    // The pilot's age plus the frame's own age.
    assert_eq!(h["flow"]["age_ms"], 130);
    assert_eq!(keys(&h["range"]), ["age_ms", "height_m", "mount", "state"]);
    assert_eq!(h["range"]["height_m"], 1.42);
    assert_eq!(h["range"]["age_ms"], 125);
    assert_eq!(keys(&h["lidar"]), ["age_ms", "health", "rate_hz", "state"]);
    // The pilot's lidar verdict, as for flow and range.
    assert_eq!(h["lidar"]["state"], "ok");
    assert_eq!(h["lidar"]["health"], "good");
    assert_eq!(h["lidar"]["age_ms"], 160);
    assert_eq!(h["cameras"], json!([]));
}

#[test]
fn the_declared_mount_is_echoed_and_null_when_undeclared() {
    let dir = tempfile::tempdir().unwrap();
    let mut inputs = HealthInputs {
        pilot: Some((pilot_frame(), 0)),
        lidar: LidarView::default(),
        config_text: MOUNT_CONFIG.into(),
    };
    let h = health_body(&paths(dir.path()), &inputs, SystemTime::now(), 0);
    assert_eq!(
        h["flow"]["mount"],
        json!({"yaw_cdeg": 9000, "pos_m": [0.05, 0.0, 0.03]})
    );
    assert_eq!(h["range"]["mount"], json!({"pos_m": [0.06, 0.0, 0.03]}));

    inputs.config_text = "agent:\n  name: x500\n".into();
    let h = health_body(&paths(dir.path()), &inputs, SystemTime::now(), 0);
    assert_eq!(h["flow"]["mount"], Value::Null);
    assert_eq!(h["range"]["mount"], Value::Null);

    // A flow mount without a position is not a declared mount.
    let only_yaw = "pilot:\n  fc_sensors:\n    flow:\n      orient_yaw_cdeg: 0\n";
    assert_eq!(
        FcSensors::from_config(only_yaw).flow,
        Some(FlowMount {
            pos_m: None,
            orient_yaw_cdeg: Some(0)
        })
    );
    inputs.config_text = only_yaw.into();
    let h = health_body(&paths(dir.path()), &inputs, SystemTime::now(), 0);
    assert_eq!(h["flow"]["mount"], Value::Null);
}

#[test]
fn flow_and_range_are_null_without_a_fresh_pilot_verdict() {
    let dir = tempfile::tempdir().unwrap();
    let p = paths(dir.path());
    let body = |pilot: Option<(Value, u64)>| {
        let inputs = HealthInputs {
            pilot,
            ..HealthInputs::default()
        };
        health_body(&p, &inputs, SystemTime::now(), 0)
    };
    for pilot in [
        None,
        // A pilot without an FC link publishes `sensors: null`.
        Some((json!({"owner": "pilot", "sensors": null}), 0)),
        // A stale frame no longer speaks for the sensors.
        Some((pilot_frame(), PILOT_FRESH_MS + 1)),
        // An unknown state word is not a verdict.
        Some((
            json!({"sensors": {"flow": {"state": "great"}, "range": {"state": 3}}}),
            0,
        )),
    ] {
        let h = body(pilot);
        assert_eq!(h["flow"], Value::Null);
        assert_eq!(h["range"], Value::Null);
    }
}

#[test]
fn the_lidar_verdict_follows_the_spec_thresholds() {
    let t = &*THRESHOLDS;
    assert_eq!(
        (t.lidar_min_hz, t.lidar_max_hz, t.lidar_lost_ms),
        (8.0, 12.0, 500)
    );
    let status = |connected: bool, health: Option<&str>| {
        Some(LidarStatus {
            device: Some("/dev/ttyUSB0".into()),
            connected,
            health: health.map(str::to_owned),
        })
    };
    let v = |rate: Option<f64>, age: Option<u64>, s: Option<LidarStatus>| LidarView {
        rate_hz: rate,
        age_ms: age,
        status: s,
    };
    let cases = [
        (v(None, None, None), "absent"),
        (v(None, None, status(false, None)), "lost"),
        // Connected with no scan yet: the pilot's `absent`.
        (v(None, None, status(true, None)), "absent"),
        // Disconnected with no device named: the pilot's `lost`.
        (
            v(
                None,
                None,
                Some(LidarStatus {
                    device: None,
                    connected: false,
                    health: None,
                }),
            ),
            "lost",
        ),
        (v(Some(10.0), Some(80), None), "ok"),
        (v(Some(10.0), Some(80), status(true, Some("good"))), "ok"),
        (
            v(Some(10.0), Some(80), status(true, Some("warning"))),
            "degraded",
        ),
        (v(Some(6.5), Some(80), None), "degraded"),
        (v(Some(13.0), Some(80), None), "degraded"),
        // Scans arriving, too few to measure a rate: the pilot's `ok`.
        (v(None, Some(80), None), "ok"),
        (v(Some(10.0), Some(500), None), "lost"),
        (v(Some(10.0), Some(501), None), "lost"),
        (v(Some(10.0), Some(80), status(false, None)), "lost"),
    ];
    for (view, want) in cases {
        assert_eq!(lidar_state(&view, t), want, "{view:?}");
    }
    let h = lidar_block(&v(Some(9.9), Some(80), status(true, Some("good"))), t);
    assert_eq!(
        h,
        json!({"state": "ok", "rate_hz": 9.9, "health": "good", "age_ms": 80})
    );
}

#[test]
fn the_lidar_reads_the_pilot_verdict_whenever_a_fresh_pilot_frame_carries_one() {
    let dir = tempfile::tempdir().unwrap();
    let p = paths(dir.path());
    let connected_no_scan = LidarView {
        rate_hz: None,
        age_ms: None,
        status: Some(LidarStatus {
            device: Some("/dev/ttyUSB0".into()),
            connected: true,
            health: Some("good".into()),
        }),
    };
    let frame = json!({"owner": "pilot", "sensors": {
        "lidar": {"state": "lost", "rate_hz": null, "health": "error", "age_ms": 900}}});
    let inputs = HealthInputs {
        pilot: Some((frame.clone(), 40)),
        lidar: connected_no_scan.clone(),
        config_text: String::new(),
    };
    let h = health_body(&p, &inputs, SystemTime::now(), 0);
    assert_eq!(
        h["lidar"],
        json!({"state": "lost", "rate_hz": null, "health": "error", "age_ms": 940})
    );
    // A stale pilot frame no longer speaks: the local fallback, pilot rules.
    let inputs = HealthInputs {
        pilot: Some((frame, PILOT_FRESH_MS + 1)),
        lidar: connected_no_scan,
        config_text: String::new(),
    };
    let h = health_body(&p, &inputs, SystemTime::now(), 0);
    assert_eq!(h["lidar"]["state"], "absent");
    assert_eq!(h["lidar"]["health"], "good");
}

const CAMERA_CONFIG: &str = "
cameras:
  - id: front
    source: /dev/video0
    role: primary
    capture: true
  - id: belly
    source: /dev/video2
    role: down
    capture: true
  - id: spare
    source: /dev/video4
    enabled: false
";

fn camera(rows: &Value, id: &str) -> Value {
    rows.as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"] == id)
        .unwrap_or_else(|| panic!("{id} listed"))
        .clone()
}

#[test]
fn cameras_read_live_stale_and_lost_from_their_frame_age() {
    let dir = tempfile::tempdir().unwrap();
    let p = paths(dir.path());
    write(
        p.sidecar("vision-status.json"),
        r#"{"version":1,"frames_consumed":10,"live_inputs":2,
           "source_errors":[{"camera":"rear","error":"delivered no frame for 10 s"}],
           "cameras":{"front":{"fps":29.7,"last_frame_age_ms":40},
                      "belly":{"fps":4.0,"last_frame_age_ms":1800},
                      "rear":{"fps":null,"last_frame_age_ms":null}}}"#,
    );
    let rows = super::super::health_cameras::camera_rows(
        &p,
        CAMERA_CONFIG,
        SystemTime::now(),
        0,
        &THRESHOLDS,
    );
    let ids: Vec<&str> = rows
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["id"].as_str().unwrap())
        .collect();
    // Configured (the disabled spare left out), then the engine's others.
    assert_eq!(ids, ["front", "belly", "rear"]);
    let front = camera(&rows, "front");
    assert_eq!(
        keys(&front),
        ["age_ms", "fps", "id", "leg", "role", "state"]
    );
    assert_eq!(front["state"], "live");
    assert_eq!(front["fps"], 29.7);
    assert_eq!(front["leg"], "main");
    assert_eq!(front["role"], "primary");
    let belly = camera(&rows, "belly");
    assert_eq!(belly["state"], "stale");
    assert_eq!(belly["leg"], "belly");
    assert_eq!(belly["role"], "down");
    let rear = camera(&rows, "rear");
    assert_eq!(rear["state"], "lost");
    assert_eq!(rear["leg"], Value::Null);
    assert_eq!(rear["role"], Value::Null);

    // The sidecar's own age counts: written 6 s ago, every frame is old.
    let later = SystemTime::now() + Duration::from_secs(6);
    let rows = super::super::health_cameras::camera_rows(&p, CAMERA_CONFIG, later, 0, &THRESHOLDS);
    assert_eq!(camera(&rows, "front")["state"], "lost");
    assert!(camera(&rows, "front")["age_ms"].as_u64().unwrap() >= 6_040);
}

#[test]
fn a_camera_without_engine_frames_falls_back_to_its_trigger_stream_or_the_pipeline() {
    let dir = tempfile::tempdir().unwrap();
    let p = paths(dir.path());
    write(
        p.sidecar("sensors.json"),
        r#"{"version":1,"updated_ns":5,"streams":[
          {"kind":"trigger","src":"belly","origin":"hub","rate_hz":5.0,"last_t_ns":1,"last_rx_ns":1000000000,"count":3},
          {"kind":"trigger","src":"cam1","origin":"hub","rate_hz":20.0,"last_t_ns":1,"last_rx_ns":1000000000,"count":3}]}"#,
    );
    let now_unix = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_secs_f64();
    write(
        p.sidecar("camera-state.json"),
        &json!({"version": 2, "state": "ready", "pipeline_state": "streaming",
                "updated_at_unix": now_unix})
        .to_string(),
    );
    let rows = super::super::health_cameras::camera_rows(
        &p,
        TRIGGER_CONFIG,
        SystemTime::now(),
        1_100_000_000,
        &THRESHOLDS,
    );
    let belly = camera(&rows, "belly");
    assert_eq!(belly["state"], "live");
    assert_eq!(belly["fps"], 20.0);
    assert_eq!(belly["age_ms"], 100);
    // The primary publishes `main`: live, with no rate or age to report.
    let front = camera(&rows, "front");
    assert_eq!(front["state"], "live");
    assert_eq!(front["fps"], Value::Null);
    assert_eq!(front["age_ms"], Value::Null);

    // Without the pipeline's word the primary has no reading: lost.
    std::fs::remove_file(p.sidecar("camera-state.json")).unwrap();
    let rows = super::super::health_cameras::camera_rows(
        &p,
        TRIGGER_CONFIG,
        SystemTime::now(),
        1_100_000_000,
        &THRESHOLDS,
    );
    assert_eq!(camera(&rows, "front")["state"], "lost");
}

/// The belly camera's frames are triggered by hub output 1 (its index).
const TRIGGER_CONFIG: &str = "
cameras:
  - id: front
    source: /dev/video0
    role: primary
    capture: true
  - id: belly
    source: /dev/video2
    role: down
    capture: true
    trigger: hub
";

/// A belly camera on its own encoded leg, not captured by the vision engine.
const LEG_CONFIG: &str = "
cameras:
  - id: front
    source: /dev/video0
    role: primary
  - id: belly
    source: /dev/video2
    role: down
";

fn unix_now(at: SystemTime) -> f64 {
    at.duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_secs_f64()
}

#[test]
fn a_secondary_camera_streaming_on_its_own_leg_reads_live() {
    let dir = tempfile::tempdir().unwrap();
    let p = paths(dir.path());
    let now = SystemTime::now();
    let rows = |p: &SensingPaths| {
        super::super::health_cameras::camera_rows(p, LEG_CONFIG, now, 0, &THRESHOLDS)
    };
    // No leg report: no reading, lost as before.
    assert_eq!(camera(&rows(&p), "belly")["state"], "lost");

    write(
        p.sidecar("video-streams.json"),
        &json!({"updated_at_unix": unix_now(now), "streams": [
            {"id": "main", "live": true}, {"id": "belly", "live": true}]})
        .to_string(),
    );
    let belly = camera(&rows(&p), "belly");
    assert_eq!(belly["leg"], "belly");
    assert_eq!(belly["state"], "live");
    assert_eq!(belly["fps"], Value::Null);

    // A leg the orchestrator reports not live stays lost.
    write(
        p.sidecar("video-streams.json"),
        &json!({"updated_at_unix": unix_now(now), "streams": [
            {"id": "belly", "live": false}]})
        .to_string(),
    );
    assert_eq!(camera(&rows(&p), "belly")["state"], "lost");

    // A stale leg report is no word at all.
    write(
        p.sidecar("video-streams.json"),
        &json!({"updated_at_unix": unix_now(now) - 60.0, "streams": [
            {"id": "belly", "live": true}]})
        .to_string(),
    );
    assert_eq!(camera(&rows(&p), "belly")["state"], "lost");
}

#[test]
fn the_tapped_main_reading_belongs_to_the_configured_primary() {
    let dir = tempfile::tempdir().unwrap();
    let p = paths(dir.path());
    write(
        p.sidecar("vision-status.json"),
        r#"{"version":1,"cameras":{"main":{"fps":29.5,"last_frame_age_ms":30}}}"#,
    );
    let rows = super::super::health_cameras::camera_rows(
        &p,
        LEG_CONFIG,
        SystemTime::now(),
        0,
        &THRESHOLDS,
    );
    let ids: Vec<&str> = rows
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["front", "belly"]);
    let front = camera(&rows, "front");
    assert_eq!(front["leg"], "main");
    assert_eq!(front["state"], "live");
    assert_eq!(front["fps"], 29.5);
}

#[test]
fn a_stale_vision_sidecar_is_ignored() {
    let dir = tempfile::tempdir().unwrap();
    let p = paths(dir.path());
    write(
        p.sidecar("vision-status.json"),
        r#"{"version":1,"source_errors":[{"camera":"front","error":"no frame"}],
           "cameras":{"front":{"fps":29.5,"last_frame_age_ms":30}}}"#,
    );
    // The engine stopped long ago; the video still streams `main`.
    let later = SystemTime::now()
        + ados_video::camera_presence::VISION_STATUS_FRESH
        + Duration::from_secs(1);
    write(
        p.sidecar("camera-state.json"),
        &json!({"version": 2, "state": "ready", "pipeline_state": "streaming",
                "updated_at_unix": unix_now(later)})
        .to_string(),
    );
    let rows = super::super::health_cameras::camera_rows(&p, TRIGGER_CONFIG, later, 0, &THRESHOLDS);
    let front = camera(&rows, "front");
    assert_eq!(front["state"], "live");
    assert_eq!(front["fps"], Value::Null);
    assert_eq!(front["age_ms"], Value::Null);
}
