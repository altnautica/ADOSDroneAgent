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
          {"kind":"baro","src":"fc_baro","origin":"fc","rate_hz":10.0,"last_t_ns":800,"last_rx_ns":1500000,"count":3},
          {"kind":"rtcm","src":"rest","origin":"host","rate_hz":1.0,"last_t_ns":0,"last_rx_ns":1,"count":1}]}"#,
    );
    write(
        p.sidecar("sensorhub.json"),
        r#"{"version":1,"connected":true,"transport":"tcp:192.168.1.50:7610","fw":"ados-sensorhub 0.1.0",
          "board":"matek_h743_slim_v3","proto_ver":1,"uptime_s":321,"crc_errors":0,"dropped":2,
          "imu_odr_hz":1000.0,"trigger_hz":10.0,"trigger_hz_configured":20.0}"#,
    );
    write(
        p.registered("clock"),
        r#"{"version":1,"timebase":"hub_freerun","locked":true,"sigma_ns":420000,"t_ns":1,"utc_ns":null,
          "sources":[{"id":"fc","kind":"fc","locked":true,"offset_ns":0,"skew_ppm":0,"sigma_ns":800000,"last_update_ns":1},
                     {"id":"hub","kind":"hub","locked":true,"offset_ns":0,"skew_ppm":0,"sigma_ns":420000,"last_update_ns":1}]}"#,
    );
    write(
        p.sidecar("mavlink-router.json"),
        r#"{"version":1,"routes":[],"sensor_rates_hz":{"fc_imu":200.0}}"#,
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
    let body = sensors_body(&p, "drone", SystemTime::now(), 3_000_000, Some(fc));
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
    // 4 streams (the host rtcm stream is not a sensor) + gnss0 from the rig.
    assert_eq!(sensors.len(), 5);
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
    // The configured trigger rate, not the hub's measured 10 Hz.
    assert_eq!(sensors[1]["expected_hz"], 20.0);
    assert_eq!(sensors[1]["rate_hz"], 20.0);
    assert_eq!(sensors[2]["sigma_ns"], 800000);
    // FC rates come from what the router requested; an unrequested stream
    // has no expectation.
    assert_eq!(sensors[2]["expected_hz"], 200.0);
    assert_eq!(sensors[3]["id"], "fc_baro");
    assert_eq!(sensors[3]["expected_hz"], Value::Null);
    let gnss = &sensors[4];
    assert_eq!(gnss["id"], "gnss0");
    assert_eq!(gnss["last_t_ns"], 0);
    assert_eq!(gnss["age_ms"], Value::Null);
    assert_eq!(gnss["sigma_ns"], -1);
    assert_eq!(body["gnss"], Value::Null);
}

#[test]
fn nothing_running_is_not_present_with_null_blocks() {
    let dir = tempfile::tempdir().unwrap();
    let body = sensors_body(&paths(dir.path()), "drone", SystemTime::now(), 0, None);
    assert_eq!(body["present"], false);
    assert_eq!(body["hub"], Value::Null);
    assert_eq!(body["fc"], Value::Null);
    assert_eq!(body["rig_id"], Value::Null);
    assert_eq!(body["sensors"], json!([]));
}

#[test]
fn a_stale_stream_table_empties_every_block() {
    // The sidecars exist, but ados-clock stopped rewriting them: nothing is
    // reported from them, not the rig, the hub, the FC nor GNSS.
    let dir = tempfile::tempdir().unwrap();
    let p = paths(dir.path());
    seed(&p);
    write(
        p.sidecar("gnss.json"),
        r#"{"version":1,"receivers":[{"fix":"3d","sats":9,"hdop":0.9,"rtk":"none"}]}"#,
    );
    let later = SystemTime::now() + Duration::from_secs(30);
    let fc = FcView {
        connected: true,
        autopilot: None,
    };
    let body = sensors_body(&p, "drone", later, 0, Some(fc));
    assert_eq!(body["present"], false);
    for key in ["hub", "fc", "gnss", "rig_id", "calib_version"] {
        assert_eq!(body[key], Value::Null, "{key}");
    }
    assert_eq!(body["sensors"], json!([]));
}

#[test]
fn a_stale_hub_reads_as_configured_but_disconnected() {
    let dir = tempfile::tempdir().unwrap();
    let p = paths(dir.path());
    seed(&p);
    let hub = std::fs::File::options()
        .write(true)
        .open(p.sidecar("sensorhub.json"))
        .unwrap();
    hub.set_modified(SystemTime::now() - Duration::from_secs(30))
        .unwrap();
    let body = sensors_body(&p, "drone", SystemTime::now(), 0, None);
    assert_eq!(body["present"], true);
    assert_eq!(body["hub"]["connected"], false);
    // No live hub rate: hub streams carry no expectation.
    assert_eq!(body["sensors"][0]["expected_hz"], Value::Null);
}

#[test]
fn each_hub_imu_expects_the_per_imu_odr() {
    let dir = tempfile::tempdir().unwrap();
    let p = paths(dir.path());
    seed(&p);
    write(
        p.sidecar("sensors.json"),
        r#"{"version":1,"updated_ns":5,"streams":[
          {"kind":"imu","src":"imu0","origin":"hub","rate_hz":1000.0,"last_t_ns":1,"last_rx_ns":1,"count":9},
          {"kind":"imu","src":"imu1","origin":"hub","rate_hz":1000.0,"last_t_ns":1,"last_rx_ns":1,"count":9}]}"#,
    );
    let body = sensors_body(&p, "drone", SystemTime::now(), 0, None);
    assert_eq!(body["sensors"][0]["expected_hz"], 1000.0);
    assert_eq!(body["sensors"][1]["expected_hz"], 1000.0);
}

#[test]
fn a_hub_without_a_configured_trigger_rate_gives_no_camera_expectation() {
    let dir = tempfile::tempdir().unwrap();
    let p = paths(dir.path());
    seed(&p);
    write(
        p.sidecar("sensorhub.json"),
        r#"{"version":1,"connected":true,"transport":"auto","imu_odr_hz":1000.0,"trigger_hz":20.0}"#,
    );
    let body = sensors_body(&p, "drone", SystemTime::now(), 0, None);
    assert_eq!(body["sensors"][1]["id"], "cam0");
    assert_eq!(body["sensors"][1]["expected_hz"], Value::Null);
}

#[test]
fn idle_rig_gnss_without_a_hub_names_the_service_that_owns_it() {
    let dir = tempfile::tempdir().unwrap();
    let p = paths(dir.path());
    seed(&p);
    std::fs::remove_file(p.sidecar("sensorhub.json")).unwrap();
    let source = |profile: &str| {
        let body = sensors_body(&p, profile, SystemTime::now(), 0, None);
        let sensors = body["sensors"].as_array().unwrap().clone();
        let gnss = sensors.iter().find(|s| s["id"] == "gnss0").unwrap();
        gnss["source"].clone()
    };
    // Ground stations and visors run ados-gnss; a drone reads GNSS off the FC.
    assert_eq!(source("ground_station"), "gnss");
    assert_eq!(source("visor"), "gnss");
    assert_eq!(source("drone"), "fc");
}

#[test]
fn a_stale_router_sidecar_gives_no_fc_expectation() {
    let window = ados_protocol::paths::max_age("mavlink-router")
        .expect("the registry declares a freshness window for mavlink-router.json");
    assert!(window < Duration::from_secs(30));
    let dir = tempfile::tempdir().unwrap();
    let p = paths(dir.path());
    seed(&p);
    let router = std::fs::File::options()
        .write(true)
        .open(p.sidecar("mavlink-router.json"))
        .unwrap();
    router
        .set_modified(SystemTime::now() - Duration::from_secs(30))
        .unwrap();
    let body = sensors_body(&p, "drone", SystemTime::now(), 0, None);
    let fc_imu = body["sensors"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["id"] == "fc_imu")
        .unwrap();
    assert_eq!(fc_imu["expected_hz"], Value::Null);
    assert_eq!(fc_imu["rate_hz"], 200.0);
}

async fn json_of(resp: Response) -> Value {
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 16)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test]
async fn the_rig_write_validates_stores_then_reloads_the_hub() {
    use std::sync::{Arc, Mutex};
    let dir = tempfile::tempdir().unwrap();
    let p = paths(dir.path());
    let calls: Arc<Mutex<Vec<String>>> = Arc::default();
    let put = |body: &'static [u8]| {
        let calls = calls.clone();
        put_rig_with(
            p.clone(),
            axum::body::Bytes::from_static(body),
            move |program, args| {
                calls
                    .lock()
                    .unwrap()
                    .push(format!("{program} {}", args.join(" ")));
                async { true }
            },
        )
    };

    let ok = put(RIG.as_bytes()).await;
    assert_eq!(ok.status(), StatusCode::OK);
    assert_eq!(
        json_of(ok).await,
        json!({"ok": true, "rig_id": "visor-pro-01", "calib_version": "2026-10-19a"})
    );
    assert_eq!(read_rig(&p).unwrap().sensors.len(), 3);
    assert_eq!(
        *calls.lock().unwrap(),
        ["systemctl --no-block reload-or-restart ados-sensorhub.service"]
    );

    let bad = put(br#"{"contract":"ados.capture/1","rig_id":"x"}"#).await;
    assert_eq!(bad.status(), StatusCode::BAD_REQUEST);
    let v = json_of(bad).await;
    assert_eq!(v["error"], "E_RIG_INVALID");
    assert!(v["detail"].as_str().is_some());
    assert_eq!(put(b"not json").await.status(), StatusCode::BAD_REQUEST);
    // A refused write reloads nothing, and the stored rig is the earlier one.
    assert_eq!(calls.lock().unwrap().len(), 1);
    assert_eq!(read_rig(&p).unwrap().rig_id, "visor-pro-01");
}
