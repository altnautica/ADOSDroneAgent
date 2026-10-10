use std::cell::RefCell;
use std::path::PathBuf;

use super::*;

async fn body_of(resp: Response) -> (StatusCode, Value) {
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, serde_json::from_slice(&bytes).unwrap())
}

fn update(key: &str, value: Value) -> ConfigUpdate {
    ConfigUpdate {
        key: key.to_string(),
        value,
    }
}

/// A side-effect runner that runs nothing.
fn no_run(_: &str, _: &[&str]) -> bool {
    true
}

fn put(cfg: &Path, key: &str, value: Value) -> Response {
    put_config_at(cfg, &update(key, value), &no_run)
}

#[test]
fn an_absent_config_reads_as_the_full_default_model() {
    let dir = tempfile::tempdir().unwrap();
    let body = read_config_at(&dir.path().join("config.yaml")).unwrap();
    // Top-level blocks and a factory-built sub-model are all present.
    for block in [
        "agent", "mavlink", "video", "network", "server", "security", "ui",
    ] {
        assert!(body.get(block).is_some(), "missing {block}");
    }
    assert_eq!(body["ui"]["theme"], json!("dark"));
    assert_eq!(body["agent"]["board_override"], json!(""));
    assert!(body["mavlink"]["endpoints"].is_array());
}

#[test]
fn the_file_overlays_the_defaults_and_unknown_keys_are_dropped() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("config.yaml");
    std::fs::write(
        &cfg,
        "agent:\n  name: gs-7\n  bogus: 1\nvideo:\n  wfb:\n    channel: 161\nnot_a_block: true\n",
    )
    .unwrap();
    std::fs::write(dir.path().join(BOARD_OVERRIDE_FILE), "rock-5c\n").unwrap();
    let body = read_config_at(&cfg).unwrap();
    assert_eq!(body["agent"]["name"], json!("gs-7"));
    assert_eq!(
        body["agent"]["profile"],
        json!("auto"),
        "unset sibling keeps its default"
    );
    assert!(body["agent"].get("bogus").is_none());
    assert!(body.get("not_a_block").is_none());
    assert_eq!(body["video"]["wfb"]["channel"], json!(161));
    assert_eq!(body["agent"]["board_override"], json!("rock-5c"));
}

#[test]
fn every_set_secret_is_redacted_and_an_unset_one_is_left_empty() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("config.yaml");
    std::fs::write(
        &cfg,
        "security:\n  api:\n    api_key: real-key\n  hmac_secret: s3cret\nnetwork:\n  hotspot:\n    password: hotspotpw\n",
    )
    .unwrap();
    let body = read_config_at(&cfg).unwrap();
    assert_eq!(SECRET_PATHS.len(), 5);
    assert_eq!(body["security"]["api"]["api_key"], json!("***"));
    assert_eq!(body["security"]["hmac_secret"], json!("***"));
    assert_eq!(body["network"]["hotspot"]["password"], json!("***"));
    assert!(!serde_json::to_string(&body).unwrap().contains("s3cret"));
    assert_ne!(body["network"]["wifi_client"]["password"], json!("***"));
}

#[test]
fn an_unparseable_config_is_an_error_not_the_defaults() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("config.yaml");
    std::fs::write(&cfg, "agent: [unclosed\n").unwrap();
    assert!(read_config_at(&cfg).is_err());
}

#[tokio::test]
async fn a_write_coerces_a_string_to_the_field_type_and_persists_it() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("config.yaml");
    std::fs::write(&cfg, "agent:\n  name: gs-7\n").unwrap();
    let (status, body) = body_of(put(&cfg, "video.wfb.channel", json!("149"))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body,
        json!({"status": "ok", "key": "video.wfb.channel", "value": 149, "persisted": true})
    );
    let read = read_config_at(&cfg).unwrap();
    assert_eq!(read["video"]["wfb"]["channel"], json!(149));
    assert_eq!(read["agent"]["name"], json!("gs-7"), "other keys survive");
}

#[tokio::test]
async fn an_unknown_key_is_a_key_not_found_error() {
    let dir = tempfile::tempdir().unwrap();
    let (status, body) = body_of(put(&dir.path().join("c.yaml"), "agent.nope", json!(1))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!({"error": "Key not found: agent.nope"}));
}

#[tokio::test]
async fn a_value_the_field_refuses_is_a_422_and_nothing_is_written() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("config.yaml");
    std::fs::write(&cfg, "agent:\n  name: x\n").unwrap();
    for (key, value) in [
        ("ui.theme", json!("purple")),
        ("agent.name", json!(5)),
        ("mavlink.endpoints", json!("x")),
        // Past the native readers' integer widths: the router and the radio
        // would otherwise drop their whole section to defaults at start.
        ("mavlink.system_id", json!(300)),
        ("video.wfb.fec_n", json!(300)),
        ("video.wfb.tx_power_dbm", json!(200)),
        ("ground_station.mesh.channel", json!(36)),
    ] {
        let (status, body) = body_of(put(&cfg, key, value)).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{key}");
        assert_eq!(body["detail"]["error"]["code"], json!("E_VALIDATION"));
    }
    assert_eq!(
        std::fs::read_to_string(&cfg).unwrap(),
        "agent:\n  name: x\n"
    );
}

#[tokio::test]
async fn put_config_profile_bogus_422() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("config.yaml");
    let (status, body) = body_of(put(&cfg, "agent.profile", json!("bogus"))).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body["detail"]["error"]["code"], json!("E_VALIDATION"));
    assert!(!cfg.exists(), "nothing was written");
    let (status, _) = body_of(put(&cfg, "agent.profile", json!("ground_station"))).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn vision_backend_tflite_422() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("config.yaml");
    let (status, _) = body_of(put(&cfg, "vision.backend", json!("tflite"))).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    let (status, _) = body_of(put(&cfg, "vision.backend", json!("onnx"))).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn battery_critical_ge_low_422() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("config.yaml");
    std::fs::write(&cfg, "battery:\n  low_cell_mv: 3500\n").unwrap();
    let (status, body) = body_of(put(&cfg, "battery.critical_cell_mv", json!(3500))).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        body,
        json!({
            "error": "E_INVALID",
            "field": "battery.critical_cell_mv",
            "message": "battery.critical_cell_mv must be less than battery.low_cell_mv",
        })
    );
    assert_eq!(
        std::fs::read_to_string(&cfg).unwrap(),
        "battery:\n  low_cell_mv: 3500\n"
    );
    let (status, _) = body_of(put(&cfg, "battery.critical_cell_mv", json!(3400))).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn injector_arbitration_on_drone_422() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("config.yaml");
    std::fs::write(&cfg, "agent:\n  profile: drone\n").unwrap();
    let (status, body) = body_of(put(&cfg, "mavlink.injector_arbitration", json!(true))).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body["error"], json!("E_INVALID"));
    assert_eq!(body["field"], json!("mavlink.injector_arbitration"));

    // A ground station may turn it on.
    let gs = dir.path().join("gs.yaml");
    std::fs::write(&gs, "agent:\n  profile: ground_station\n").unwrap();
    let (status, _) = body_of(put(&gs, "mavlink.injector_arbitration", json!(true))).await;
    assert_eq!(status, StatusCode::OK);

    // A file that already says so stays writable: only a write that
    // introduces the combination is refused.
    let legacy = dir.path().join("legacy.yaml");
    std::fs::write(
        &legacy,
        "agent:\n  profile: drone\nmavlink:\n  injector_arbitration: true\n",
    )
    .unwrap();
    let (status, _) = body_of(put(&legacy, "agent.name", json!("still-writable"))).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn crsf_enable_creates_marker() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("config.yaml");
    let calls = RefCell::new(Vec::new());
    let run = |p: &str, a: &[&str]| {
        calls.borrow_mut().push(format!("{p} {}", a.join(" ")));
        true
    };
    let resp = put_config_at(&cfg, &update("radio.crsf.enabled", json!(true)), &run);
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(dir.path().join("crsf-enabled").exists());
    assert_eq!(
        *calls.borrow(),
        vec!["systemctl --no-block reload-or-restart ados-crsf.service"]
    );

    // The same value again writes nothing and kicks nothing.
    let resp = put_config_at(&cfg, &update("radio.crsf.enabled", json!(true)), &run);
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(calls.borrow().len(), 1);
}

#[tokio::test]
async fn the_redaction_sentinel_is_never_written_to_a_secret() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("config.yaml");
    std::fs::write(&cfg, "security:\n  hmac_secret: real\n").unwrap();
    let (status, body) = body_of(put(&cfg, "security.hmac_secret", json!("***"))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        body["detail"]["error"]["code"],
        json!("E_REDACTED_SENTINEL")
    );
    assert!(std::fs::read_to_string(&cfg).unwrap().contains("real"));
}

#[tokio::test]
async fn a_string_that_does_not_coerce_is_an_invalid_value_error() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("c.yaml");
    let (status, body) = body_of(put(&cfg, "video.wfb.channel", json!("abc"))).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body["error"]
        .as_str()
        .unwrap()
        .starts_with("Invalid value:"));
}

/// Every document under `tests/fixtures/config-invalid/`. The Python parity
/// suite asserts each fixture the model refuses is named here.
const INVALID_FIXTURES: &[&str] = &[
    "battery-critical-ge-low.yaml",
    "mesh-channel-36.yaml",
    "profile-bogus.yaml",
    "swarm-hard-ge-radius.yaml",
    "vision-backend-tflite.yaml",
];

fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/config-invalid")
}

/// Whether any scalar leaf of `doc` is refused by its field's schema.
fn a_leaf_is_refused(doc: &Value, path: &mut Vec<String>) -> bool {
    match doc {
        Value::Object(map) => map.iter().any(|(key, value)| {
            path.push(key.clone());
            let refused = a_leaf_is_refused(value, path);
            path.pop();
            refused
        }),
        Value::Array(_) => false,
        scalar => schema::field(path.iter().map(String::as_str))
            .is_some_and(|node| validate(node, scalar).is_err()),
    }
}

#[test]
fn the_native_validator_refuses_every_invalid_fixture() {
    let mut names: Vec<String> = std::fs::read_dir(fixtures_dir())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(".yaml"))
        .collect();
    names.sort();
    assert_eq!(names, INVALID_FIXTURES, "the fixture list drifted");
    for name in INVALID_FIXTURES {
        let path = fixtures_dir().join(name);
        let doc = schema::read_document(&path).unwrap().unwrap();
        let effective = schema::effective(&path).unwrap();
        let refused = a_leaf_is_refused(&doc, &mut Vec::new())
            || CROSS_FIELD.iter().any(|rule| (rule.violated)(&effective));
        assert!(
            refused,
            "{name} is accepted natively but refused by the model"
        );
    }
}
