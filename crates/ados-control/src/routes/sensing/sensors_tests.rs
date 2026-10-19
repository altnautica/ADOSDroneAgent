use super::*;
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

const RIG: &str = r#"{"contract":"ados.capture/1","rig_id":"visor-pro-01","calib_version":"2026-10-19a",
  "sensors":[
   {"id":"imu0","kind":"imu","role":"nav","T_body_sensor":[1,0,0,0,0,1,0,0,0,0,1,0,0,0,0,1],"time_offset_ns":0},
   {"id":"cam0","kind":"camera","role":"front_left","T_body_sensor":[1,0,0,0,0,1,0,0,0,0,1,0,0,0,0,1],"time_offset_ns":0},
   {"id":"gnss0","kind":"gnss","role":"nav","T_body_sensor":[1,0,0,0,0,1,0,0,0,0,1,0,0,0,0,1],"time_offset_ns":0}]}"#;

fn seed(p: &SensingPaths) {
    write(
        p.sidecar("sensors.json"),
        r#"{"version":1,"updated_ns":5,"streams":[
          {"kind":"imu","src":"imu0","origin":"hub","rate_hz":1000.0,"last_t_ns":1000,"last_rx_ns":2000000,"count":9},
          {"kind":"trigger","src":"cam0","origin":"hub","rate_hz":20.0,"last_t_ns":900,"last_rx_ns":1000000,"count":3},
          {"kind":"imu","src":"fc_imu","origin":"fc","rate_hz":200.0,"last_t_ns":800,"last_rx_ns":1500000,"count":3},
          {"kind":"rtcm","src":"rest","origin":"host","rate_hz":1.0,"last_t_ns":0,"last_rx_ns":1,"count":1}]}"#,
    );
    write(
        p.sidecar("sensorhub.json"),
        r#"{"version":1,"connected":true,"transport":"tcp:192.168.1.50:7610","fw":"ados-sensorhub 0.1.0",
          "board":"matek_h743_slim_v3","proto_ver":1,"uptime_s":321,"crc_errors":0,"dropped":2,
          "imu_odr_hz":1000.0,"trigger_hz":20.0}"#,
    );
    write(
        p.sidecar("clock.json"),
        r#"{"version":1,"timebase":"hub_freerun","locked":true,"sigma_ns":420000,"t_ns":1,"utc_ns":null,
          "sources":[{"id":"fc","kind":"fc","locked":true,"offset_ns":0,"skew_ppm":0,"sigma_ns":800000,"last_update_ns":1},
                     {"id":"hub","kind":"hub","locked":true,"offset_ns":0,"skew_ppm":0,"sigma_ns":420000,"last_update_ns":1}]}"#,
    );
    write(p.rig(), RIG);
}

#[test]
fn the_table_joins_streams_rig_hub_clock_and_fc() {
    let dir = tempfile::tempdir().unwrap();
    let p = paths(dir.path());
    seed(&p);
    let fc = FcView {
        connected: true,
        autopilot: Some("ardupilot".into()),
    };
    let body = sensors_body(&p, SystemTime::now(), 3_000_000, Some(fc));
    assert_eq!(body["present"], true);
    assert_eq!(body["rig_id"], "visor-pro-01");
    assert_eq!(body["calib_version"], "2026-10-19a");
    let mut hub_keys: Vec<&str> = body["hub"]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    hub_keys.sort_unstable();
    assert_eq!(
        hub_keys,
        [
            "board",
            "connected",
            "crc_errors",
            "dropped",
            "fw",
            "proto_ver",
            "transport",
            "uptime_s"
        ]
    );
    assert_eq!(body["hub"]["connected"], true);
    assert_eq!(
        body["fc"],
        json!({"connected": true, "autopilot": "ardupilot", "highres_imu": true})
    );
    let sensors = body["sensors"].as_array().unwrap();
    // 3 streams (the host rtcm stream is not a sensor) + gnss0 from the rig.
    assert_eq!(sensors.len(), 4);
    let imu = &sensors[0];
    let mut keys: Vec<&str> = imu
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        [
            "age_ms",
            "expected_hz",
            "id",
            "kind",
            "last_t_ns",
            "rate_hz",
            "role",
            "sigma_ns",
            "source"
        ]
    );
    assert_eq!(imu["source"], "hub");
    assert_eq!(imu["expected_hz"], 1000.0);
    assert_eq!(imu["age_ms"], 1.0);
    assert_eq!(imu["sigma_ns"], 420000);
    assert_eq!(sensors[1]["kind"], "camera");
    assert_eq!(sensors[1]["role"], "front_left");
    assert_eq!(sensors[1]["expected_hz"], 20.0);
    assert_eq!(sensors[2]["sigma_ns"], 800000);
    let gnss = &sensors[3];
    assert_eq!(gnss["id"], "gnss0");
    assert_eq!(gnss["last_t_ns"], 0);
    assert_eq!(gnss["age_ms"], Value::Null);
    assert_eq!(gnss["sigma_ns"], -1);
    assert_eq!(body["gnss"], Value::Null);
}

#[test]
fn nothing_running_is_not_present_with_null_blocks() {
    let dir = tempfile::tempdir().unwrap();
    let body = sensors_body(&paths(dir.path()), SystemTime::now(), 0, None);
    assert_eq!(body["present"], false);
    assert_eq!(body["hub"], Value::Null);
    assert_eq!(body["fc"], Value::Null);
    assert_eq!(body["rig_id"], Value::Null);
    assert_eq!(body["sensors"], json!([]));
}

#[test]
fn a_stale_hub_reads_as_configured_but_disconnected() {
    let dir = tempfile::tempdir().unwrap();
    let p = paths(dir.path());
    seed(&p);
    let later = SystemTime::now() + Duration::from_secs(30);
    let body = sensors_body(&p, later, 0, None);
    assert_eq!(body["present"], false);
    assert_eq!(body["hub"]["connected"], false);
}

#[tokio::test]
async fn the_rig_write_validates_then_stores() {
    let dir = tempfile::tempdir().unwrap();
    let p = paths(dir.path());
    let ok = put_rig_at(&p, RIG.as_bytes());
    assert_eq!(ok.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(ok.into_body(), 1 << 16).await.unwrap();
    let v: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        v,
        json!({"ok": true, "rig_id": "visor-pro-01", "calib_version": "2026-10-19a"})
    );
    assert_eq!(read_rig(&p).unwrap().sensors.len(), 3);

    let bad = put_rig_at(&p, br#"{"contract":"ados.capture/1","rig_id":"x"}"#);
    assert_eq!(bad.status(), StatusCode::BAD_REQUEST);
    let bytes = axum::body::to_bytes(bad.into_body(), 1 << 16)
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(v["error"], "E_RIG_INVALID");
    assert!(v["detail"].as_str().is_some());
    assert_eq!(
        put_rig_at(&p, b"not json").status(),
        StatusCode::BAD_REQUEST
    );
    // The stored rig is the earlier valid one.
    assert_eq!(read_rig(&p).unwrap().rig_id, "visor-pro-01");
}
