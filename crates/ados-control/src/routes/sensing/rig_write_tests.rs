use super::*;
use crate::routes::sensing::sensors::read_rig_doc;
use ados_sensorbus::rig::RigSummary;
use std::sync::{Arc, Mutex};

fn read_rig(p: &SensingPaths) -> Option<RigSummary> {
    read_rig_doc(p).map(|(_, s)| s)
}

fn paths(dir: &std::path::Path) -> SensingPaths {
    SensingPaths {
        run_dir: dir.join("run"),
        etc_dir: dir.join("etc"),
    }
}

const RIG: &str = r#"{"contract":"ados.capture/1","rig_id":"visor-pro-01","calib_version":"2026-10-19a",
  "sensors":[
   {"id":"imu0","kind":"imu","role":"nav","T_body_sensor":[1,0,0,0,0,1,0,0,0,0,1,0,0,0,0,1],"time_offset_ns":0},
   {"id":"cam0","kind":"camera","role":"front_left","T_body_sensor":[1,0,0,0,0,1,0,0,0,0,1,0,0,0,0,1],"time_offset_ns":0},
   {"id":"gnss0","kind":"gnss","role":"nav","T_body_sensor":[1,0,0,0,0,1,0,0,0,0,1,0,0,0,0,1],"time_offset_ns":0}]}"#;

async fn json_of(resp: Response) -> Value {
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 16)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

/// A recording stand-in for `systemctl`.
type Calls = Arc<Mutex<Vec<String>>>;

fn recorder(
    calls: &Calls,
) -> impl FnOnce(&'static str, &'static [&'static str]) -> std::future::Ready<bool> {
    let calls = calls.clone();
    move |program, args| {
        calls
            .lock()
            .unwrap()
            .push(format!("{program} {}", args.join(" ")));
        std::future::ready(true)
    }
}

const RELOAD: &str = "systemctl --no-block reload-or-restart ados-sensorhub.service";

#[tokio::test]
async fn the_rig_write_validates_stores_then_reloads_the_hub() {
    let dir = tempfile::tempdir().unwrap();
    let p = paths(dir.path());
    let calls = Calls::default();
    let put = |body: &'static [u8]| {
        store_then_reload(p.clone(), move |p| put_rig_at(p, body), recorder(&calls))
    };

    let ok = put(RIG.as_bytes()).await;
    assert_eq!(ok.status(), StatusCode::OK);
    assert_eq!(
        json_of(ok).await,
        json!({"ok": true, "rig_id": "visor-pro-01", "calib_version": "2026-10-19a"})
    );
    assert_eq!(read_rig(&p).unwrap().sensors.len(), 3);
    assert_eq!(*calls.lock().unwrap(), [RELOAD]);

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

#[test]
fn the_c1_template_is_a_contract_lidar_entry() {
    let s = &*C1_SENSOR;
    assert_eq!(s["id"], "lidar0");
    assert_eq!(s["kind"], "lidar");
    assert_eq!(s["role"], "nav");
    assert_eq!(s["time_offset_ns"], 0);
    // Identity rotation, the mast 0.12 m above the CG (FLU).
    assert_eq!(
        s["T_body_sensor"],
        json!([1, 0, 0, 0, 0, 1, 0, 0, 0, 0, 1, 0.12, 0, 0, 0, 1])
    );
    let l = &s["lidar"];
    assert_eq!(l["model"], "rplidar-c1");
    assert_eq!(l["pattern"], "planar");
    assert_eq!(l["min_range_m"], 0.05);
    assert_eq!(l["max_range_m"], 12.0);
    assert_eq!(l["rate_hz"], 10.0);
    // 0.72 degree resolution at 10 Hz: 500 samples a turn.
    let per_turn = l["points_per_s"].as_f64().unwrap() / l["rate_hz"].as_f64().unwrap();
    assert!((360.0 / per_turn - 0.72).abs() < 1e-9);
    assert_eq!(l["blocked_azimuth_deg"], json!([]));
    // Valid against the contract schema as the only sensor of a rig.
    let rig =
        json!({"contract": "ados.capture/1", "rig_id": "r", "calib_version": "1", "sensors": [s]});
    rig::validate(&rig).expect("the template validates");
}

#[tokio::test]
async fn the_lidar_add_creates_a_rig_then_refuses_a_second_lidar() {
    let dir = tempfile::tempdir().unwrap();
    let p = paths(dir.path());
    let calls = Calls::default();
    let add = || store_then_reload(p.clone(), |p| add_lidar_at(p, "drone-7"), recorder(&calls));

    let ok = add().await;
    assert_eq!(ok.status(), StatusCode::OK);
    let v = json_of(ok).await;
    assert_eq!(v["rig_id"], "drone-7");
    assert_eq!(v["calib_version"], "uncalibrated");
    assert_eq!(v["sensor"]["id"], "lidar0");
    let rig = read_rig(&p).expect("the created rig validates");
    assert_eq!(rig.sensors.len(), 1);
    assert_eq!(rig.sensors[0].kind, "lidar");
    assert_eq!(*calls.lock().unwrap(), [RELOAD]);

    let again = add().await;
    assert_eq!(again.status(), StatusCode::CONFLICT);
    assert_eq!(json_of(again).await["error"], "E_RIG_LIDAR_EXISTS");
    assert_eq!(calls.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn the_lidar_add_extends_a_stored_rig_and_refuses_a_bad_one() {
    let dir = tempfile::tempdir().unwrap();
    let p = paths(dir.path());
    std::fs::create_dir_all(&p.etc_dir).unwrap();
    std::fs::write(p.rig(), RIG).unwrap();
    let calls = Calls::default();

    let ok = store_then_reload(p.clone(), |p| add_lidar_at(p, "unused"), recorder(&calls)).await;
    assert_eq!(ok.status(), StatusCode::OK);
    let rig = read_rig(&p).unwrap();
    assert_eq!(rig.rig_id, "visor-pro-01");
    assert_eq!(rig.calib_version, "2026-10-19a");
    let ids: Vec<&str> = rig.sensors.iter().map(|s| s.id.as_str()).collect();
    assert_eq!(ids, ["imu0", "cam0", "gnss0", "lidar0"]);

    // A sensor already named lidar0 that is not a lidar is refused.
    std::fs::write(p.rig(), RIG.replace("gnss0", "lidar0")).unwrap();
    let (resp, stored) = add_lidar_at(&p, "unused");
    assert!(!stored);
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    assert_eq!(json_of(resp).await["error"], "E_RIG_ID_TAKEN");

    // A stored rig that does not validate is left alone.
    std::fs::write(p.rig(), r#"{"rig_id":"x"}"#).unwrap();
    let (resp, stored) = add_lidar_at(&p, "unused");
    assert!(!stored);
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    assert_eq!(json_of(resp).await["error"], "E_RIG_INVALID");
    assert_eq!(
        std::fs::read_to_string(p.rig()).unwrap(),
        r#"{"rig_id":"x"}"#
    );
    assert_eq!(calls.lock().unwrap().len(), 1);
}
