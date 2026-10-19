use super::*;
use std::sync::Arc;

use tokio::net::UnixListener;

async fn json_of(r: Response) -> (StatusCode, Value) {
    let status = r.status();
    let bytes = axum::body::to_bytes(r.into_body(), 1 << 16).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap())
}

const SIDECAR: &str = r#"{"version":1,
 "receivers":[{"id":"gnss0","source":"hub","model":"u-blox M10","fix":"3d","sats":14,"hdop":0.8,
   "lat_deg":12.9716,"lon_deg":77.5946,"h_ellipsoid_m":920.4,"h_msl_m":1006.1,"rtk":"none","pps_locked":true}],
 "base":{"mode":"survey","state":"surveying","elapsed_s":120,"min_dur_s":300,"accuracy_m":3.1,"target_accuracy_m":2.0,
   "fixed":null,"rtcm_out":{"msgs_per_s":0.0,"bytes_total":0,"targets":0}},
 "heading":{"available":false,"deg":null,"accuracy_deg":null,"baseline_m":null}}"#;

#[test]
fn the_read_serves_the_sidecar_or_not_present() {
    let dir = tempfile::tempdir().unwrap();
    let p = SensingPaths {
        run_dir: dir.path().to_path_buf(),
        etc_dir: dir.path().to_path_buf(),
    };
    let absent = gnss_body(&p, SystemTime::now());
    assert_eq!(
        absent,
        json!({"present": false, "receivers": [], "base": null, "heading": null})
    );
    std::fs::write(p.sidecar("gnss.json"), SIDECAR).unwrap();
    let body = gnss_body(&p, SystemTime::now());
    assert_eq!(body["present"], true);
    assert!(body.get("version").is_none());
    let mut keys: Vec<&str> = body
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(keys, ["base", "heading", "present", "receivers"]);
    assert_eq!(body["receivers"][0]["fix"], "3d");
    assert_eq!(body["base"]["rtcm_out"]["targets"], 0);
}

/// A one-shot fake of the GNSS command socket: records the request line and
/// answers `reply`.
async fn fake_gnss(
    sock: std::path::PathBuf,
    reply: Value,
) -> Arc<tokio::sync::Mutex<Option<Value>>> {
    let seen = Arc::new(tokio::sync::Mutex::new(None));
    let _ = std::fs::remove_file(&sock);
    let listener = UnixListener::bind(&sock).unwrap();
    let s = seen.clone();
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let (rd, mut wr) = stream.into_split();
        let mut line = String::new();
        BufReader::new(rd).read_line(&mut line).await.unwrap();
        *s.lock().await = Some(serde_json::from_str(&line).unwrap());
        let mut out = serde_json::to_vec(&reply).unwrap();
        out.push(b'\n');
        wr.write_all(&out).await.unwrap();
    });
    seen
}

fn config_doc(path: &Path) -> serde_norway::Value {
    serde_norway::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

#[tokio::test]
async fn the_base_write_forwards_saves_and_returns_the_base_object() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("gnss-cmd.sock");
    let config = dir.path().join("config.yaml");
    std::fs::write(
        &config,
        "agent:\n  profile: visor\ngnss:\n  receivers: []\n",
    )
    .unwrap();
    // The service fills the accuracy the request left out.
    let base = json!({"mode":"survey","state":"surveying","elapsed_s":0,"min_dur_s":120,"accuracy_m":null,
        "target_accuracy_m":2.0,"fixed":null,"rtcm_out":{"msgs_per_s":0.0,"bytes_total":0,"targets":0}});
    let seen = fake_gnss(sock.clone(), json!({"ok": true, "base": base})).await;
    let req = json!({"mode": "survey", "min_dur_s": 120});
    let (status, body) = json_of(put_base_at(&sock, &config, req.clone()).await).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, base);
    assert_eq!(
        seen.lock().await.clone().unwrap(),
        json!({"cmd": "set_base", "base": req})
    );
    let doc = config_doc(&config);
    assert_eq!(doc["agent"]["profile"].as_str(), Some("visor"));
    assert!(doc["gnss"]["receivers"].is_sequence());
    let saved = &doc["gnss"]["base"];
    assert_eq!(saved["mode"].as_str(), Some("survey"));
    assert_eq!(saved["min_dur_s"].as_u64(), Some(120));
    assert_eq!(saved["accuracy_m"].as_f64(), Some(2.0));

    // A fixed position is saved with the mode; the survey settings stay.
    let fixed = json!({"mode":"fixed","lat_deg":12.5,"lon_deg":77.5,"h_ellipsoid_m":900.25});
    fake_gnss(sock.clone(), json!({"ok": true, "base": {"mode": "fixed"}})).await;
    let (status, _) = json_of(put_base_at(&sock, &config, fixed).await).await;
    assert_eq!(status, StatusCode::OK);
    let saved = config_doc(&config)["gnss"]["base"].clone();
    assert_eq!(saved["mode"].as_str(), Some("fixed"));
    assert_eq!(saved["lat_deg"].as_f64(), Some(12.5));
    assert_eq!(saved["h_ellipsoid_m"].as_f64(), Some(900.25));
    assert_eq!(saved["min_dur_s"].as_u64(), Some(120));
}

#[test]
fn base_keys_per_mode() {
    let base = json!({"min_dur_s": 300, "target_accuracy_m": 1.5});
    assert_eq!(
        Value::Object(base_keys(&json!({"mode": "off"}), &base)),
        json!({"mode": "off"})
    );
    assert_eq!(
        Value::Object(base_keys(&json!({"mode": "survey"}), &base)),
        json!({"mode": "survey", "min_dur_s": 300, "accuracy_m": 1.5})
    );
}

#[tokio::test]
async fn base_write_errors_map_to_status_codes_and_save_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("gnss-cmd.sock");
    let config = dir.path().join("config.yaml");
    let (status, body) = json_of(put_base_at(&sock, &config, json!({"mode": "bogus"})).await).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "E_GNSS_BAD_REQUEST");
    let (status, body) = json_of(put_base_at(&sock, &config, json!({"mode": "off"})).await).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body["error"], "E_GNSS_UNAVAILABLE");
    fake_gnss(
        sock.clone(),
        json!({"ok": false, "error": "E_GNSS_UNSUPPORTED", "detail": "receiver cannot survey"}),
    )
    .await;
    let (status, body) =
        json_of(put_base_at(&sock, &config, json!({"mode": "survey"})).await).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["error"], "E_GNSS_UNSUPPORTED");
    assert!(!config.exists(), "a refused write saves nothing");
}

#[tokio::test]
async fn rtcm_is_published_on_the_bus_and_fragments_are_counted() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("sensors.sock");
    let one = rtcm::encode_rtcm3(&[7]);
    // Down bus: refused, never a fake ok.
    let (status, body) = json_of(post_rtcm_at(&sock, &one).await).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body["error"], "E_SENSOR_BUS_DOWN");

    let listener = ados_protocol::ipc::bind_command_socket(&sock, 0o660).unwrap();
    let (tap, mut taps) = tokio::sync::mpsc::channel(4);
    tokio::spawn(ados_sensorbus::broker::serve(
        listener,
        Some(tap),
        Arc::new(Default::default()),
    ));
    // One 20-byte payload frame and one 300-byte one: 1 + 2 fragments.
    let mut bytes = rtcm::encode_rtcm3(&[1u8; 20]);
    bytes.extend(rtcm::encode_rtcm3(&[2u8; 300]));
    let (status, body) = json_of(post_rtcm_at(&sock, &bytes).await).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!({"ok": true, "fragments": 3}));
    let frame = taps.recv().await.unwrap();
    assert_eq!(frame.kind, Kind::Rtcm);
    assert_eq!(frame.origin, Origin::Host);
    assert_eq!(frame.payload, bytes);

    let (status, _) = json_of(post_rtcm_at(&sock, &[]).await).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    // No CRC-valid frame: refused and nothing published.
    let mut corrupt = rtcm::encode_rtcm3(&[3u8; 20]);
    corrupt[10] ^= 0xFF;
    for bad in [corrupt, vec![0u8; 900]] {
        let (status, body) = json_of(post_rtcm_at(&sock, &bad).await).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"], "E_RTCM_INVALID");
    }
    // A valid frame longer than four fragments carry.
    let long = rtcm::encode_rtcm3(&[4u8; 800]);
    let (status, body) = json_of(post_rtcm_at(&sock, &long).await).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "E_RTCM_TOO_LONG");
    assert!(taps.try_recv().is_err(), "refused bodies are not published");
}
