//! `gnss-cmd.sock` round trips over a temp socket.

use std::path::Path;
use std::time::Duration;

use ados_gnss::config::{GnssConfig, ReceiverCfg, Source};
use ados_gnss::service::{Service, Shared};
use ados_gnss_stream::base::BaseMode;
use ados_gnss_stream::valset::RxPort;
use ados_protocol::shutdown::Shutdown;
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

fn shared(source: &str) -> Shared {
    let cfg = GnssConfig {
        receivers: vec![ReceiverCfg {
            id: "gnss0".into(),
            source: Source::parse(source).unwrap(),
            model: Some("u-blox ZED-F9P".into()),
            port: RxPort::Uart1,
        }],
        pps_device: None,
        base: BaseMode::Off,
        survey_min_dur_s: 300,
        survey_accuracy_m: 2.0,
        heading_enabled: false,
    };
    Service::new(&cfg, 0).shared()
}

async fn ask(sock: &Path, req: Value) -> Value {
    let mut s = UnixStream::connect(sock).await.unwrap();
    s.write_all(format!("{req}\n").as_bytes()).await.unwrap();
    let mut out = String::new();
    tokio::time::timeout(Duration::from_secs(5), s.read_to_string(&mut out))
        .await
        .unwrap()
        .unwrap();
    assert!(out.ends_with('\n'));
    serde_json::from_str(out.trim()).unwrap()
}

async fn start(source: &str) -> (tempfile::TempDir, std::path::PathBuf, Shutdown) {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("gnss-cmd.sock");
    let peers = dir.path().join("linked-peers.json");
    // One fresh peer and one stale one: only the fresh one is a target.
    let now = ados_sensorbus::time::realtime_ns() as f64 / 1e9;
    let doc = json!({"version": 1, "peers": [
        {"device_id": "d1", "last_seen_unix": now - 1.0},
        {"device_id": "d2", "last_seen_unix": now - 600.0},
    ]});
    std::fs::write(&peers, doc.to_string()).unwrap();
    let shutdown = Shutdown::new();
    tokio::spawn(ados_gnss::cmdsock::serve(
        shared(source),
        sock.clone(),
        peers,
        shutdown.clone(),
    ));
    for _ in 0..100 {
        if sock.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    (dir, sock, shutdown)
}

#[tokio::test]
async fn set_base_round_trip() {
    let (_dir, sock, shutdown) = start("udp:7800").await;

    let r = ask(
        &sock,
        json!({"cmd":"set_base","base":{"mode":"survey","min_dur_s":120,"accuracy_m":1.5}}),
    )
    .await;
    assert_eq!(r["ok"], true, "{r}");
    let b = &r["base"];
    assert_eq!(b["mode"], "survey");
    assert_eq!(b["state"], "surveying");
    assert_eq!(b["min_dur_s"], 120);
    assert_eq!(b["target_accuracy_m"], 1.5);
    assert_eq!(b["elapsed_s"], 0);
    assert_eq!(b["accuracy_m"], Value::Null);
    assert_eq!(b["fixed"], Value::Null);
    assert_eq!(b["rtcm_out"]["targets"], 1);

    let r = ask(&sock, json!({"cmd":"set_base","base":{"mode":"off"}})).await;
    assert_eq!(
        (r["ok"].clone(), r["base"]["state"].clone()),
        (json!(true), json!("off"))
    );

    for bad in [
        json!({"mode":"survey","min_dur_s":0,"accuracy_m":1.0}),
        json!({"mode":"survey","min_dur_s":10,"accuracy_m":0.0}),
        json!({"mode":"fixed","lat_deg":91.0,"lon_deg":0.0,"h_ellipsoid_m":1.0}),
        json!({"mode":"fixed","lat_deg":0.0,"lon_deg":-181.0,"h_ellipsoid_m":1.0}),
        json!({"mode":"fixed","lat_deg":0.0,"lon_deg":0.0}),
        json!({"mode":"moving"}),
    ] {
        let r = ask(&sock, json!({"cmd":"set_base","base":bad})).await;
        assert_eq!(r["ok"], false, "{bad}");
        assert_eq!(r["error"], "E_GNSS_BAD_REQUEST", "{bad}");
        assert!(r["detail"].as_str().is_some_and(|d| !d.is_empty()));
    }
    let r = ask(&sock, json!({"cmd":"reboot"})).await;
    assert_eq!(r["error"], "E_GNSS_BAD_REQUEST");

    let r = ask(
        &sock,
        json!({"cmd":"set_base","base":{"mode":"fixed","lat_deg":12.5,"lon_deg":77.5,"h_ellipsoid_m":900.0}}),
    )
    .await;
    assert_eq!(r["ok"], true);
    assert_eq!(r["base"]["mode"], "fixed");
    shutdown.trigger();
}

#[tokio::test]
async fn hub_receiver_reports_unsupported() {
    let (_dir, sock, shutdown) = start("hub").await;
    let r = ask(&sock, json!({"cmd":"set_base","base":{"mode":"survey"}})).await;
    assert_eq!(r["ok"], false);
    assert_eq!(r["error"], "E_GNSS_UNSUPPORTED");
    let r = ask(&sock, json!({"cmd":"set_base","base":{"mode":"off"}})).await;
    assert_eq!(r["ok"], true);
    shutdown.trigger();
}
