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

#[tokio::test]
async fn the_base_write_forwards_and_returns_the_base_object() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("gnss-cmd.sock");
    let base = json!({"mode":"survey","state":"surveying","elapsed_s":0,"min_dur_s":300,"accuracy_m":null,
        "target_accuracy_m":2.0,"fixed":null,"rtcm_out":{"msgs_per_s":0.0,"bytes_total":0,"targets":0}});
    let seen = fake_gnss(sock.clone(), json!({"ok": true, "base": base})).await;
    let req = json!({"mode": "survey", "min_dur_s": 300, "accuracy_m": 2.0});
    let (status, body) = json_of(put_base_at(&sock, req.clone()).await).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, base);
    assert_eq!(
        seen.lock().await.clone().unwrap(),
        json!({"cmd": "set_base", "base": req})
    );
}

#[tokio::test]
async fn base_write_errors_map_to_status_codes() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("gnss-cmd.sock");
    let (status, body) = json_of(put_base_at(&sock, json!({"mode": "bogus"})).await).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "E_GNSS_BAD_REQUEST");
    let (status, body) = json_of(put_base_at(&sock, json!({"mode": "off"})).await).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body["error"], "E_GNSS_UNAVAILABLE");
    fake_gnss(
        sock.clone(),
        json!({"ok": false, "error": "E_GNSS_UNSUPPORTED", "detail": "receiver cannot survey"}),
    )
    .await;
    let (status, body) = json_of(put_base_at(&sock, json!({"mode": "survey"})).await).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["error"], "E_GNSS_UNSUPPORTED");
}

#[tokio::test]
async fn rtcm_is_published_on_the_bus_and_fragments_are_counted() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("sensors.sock");
    // Down bus: refused, never a fake ok.
    let (status, body) = json_of(post_rtcm_at(&sock, &[0xD3, 0, 1, 7, 0, 0, 0]).await).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body["error"], "E_SENSOR_BUS_DOWN");

    let listener = UnixListener::bind(&sock).unwrap();
    let (tap, mut taps) = tokio::sync::mpsc::channel(4);
    tokio::spawn(ados_sensorbus::broker::serve(
        listener,
        Some(tap),
        Arc::new(Default::default()),
    ));
    // One 20-byte payload frame and one 300-byte one: 1 + 2 fragments.
    let mut bytes = vec![0xD3, 0, 20];
    bytes.extend([1u8; 23]);
    bytes.extend([0xD3, 0x01, 0x2C]);
    bytes.extend([2u8; 303]);
    let (status, body) = json_of(post_rtcm_at(&sock, &bytes).await).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!({"ok": true, "fragments": 3}));
    let frame = taps.recv().await.unwrap();
    assert_eq!(frame.kind, Kind::Rtcm);
    assert_eq!(frame.origin, Origin::Host);
    assert_eq!(frame.payload, bytes);

    let (status, _) = json_of(post_rtcm_at(&sock, &[]).await).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, body) = json_of(post_rtcm_at(&sock, &[0u8; 900]).await).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "E_RTCM_TOO_LONG");
}
