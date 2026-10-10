//! Upload-route admission: one archive in flight, and the streamed-digest
//! check a digest-signed upload must pass.

use std::io::Write;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tower::util::ServiceExt;

use ados_plugin_host::backend::RecordingBackend;
use ados_plugin_host::supervisor::{Paths, PluginSupervisor};

use super::try_archive_slot;
use crate::routes::plugins_lifecycle::PluginLifecycle;
use crate::state::AppState;

const MANIFEST: &str = "id: com.example.web\nversion: 1.0.0\nname: Web\nlicense: MIT\n\
compatibility:\n  ados_version: \">=0.1.0\"\n\
agent:\n  entrypoint: agent/py/x.py\n  permissions:\n    - event.publish\n    - mavlink.read\n\
gcs:\n  entrypoint: gcs/index.js\n  contributes:\n    panels:\n      - id: main\n";

fn state_in(root: &std::path::Path) -> AppState {
    let paths = Paths {
        install_dir: root.join("plugins"),
        unit_dir: root.join("units"),
        state_path: root.join("state").join("plugin-state.json"),
        log_dir: root.join("log"),
        control_dir: root.join("host"),
        loopback_guard_state: root.join("guard.json"),
        socket_dir: root.join("sockets"),
        token_secret: root.join("token-secret"),
        runner: root.join("runner"),
        run_dir: root.join("run"),
        data_root: root.join("plugin-data"),
        device_id_file: root.join("device-id"),
    };
    let backend = Arc::new(RecordingBackend::default());
    let factory_paths = paths.clone();
    let lifecycle = PluginLifecycle::new(
        move || {
            PluginSupervisor::new(factory_paths.clone(), false, None, "1.0.0")
                .with_backend(backend.clone())
                .with_profile("drone")
        },
        None,
        paths.run_dir.clone(),
        paths.run_dir.clone(),
    );
    let config = root.join("config.yaml");
    let config_body = "agent:\n  device_id: node-7\n  profile: drone\n";
    std::fs::write(&config, config_body).unwrap();
    let pairing_json = root.join("pairing.json");
    std::fs::write(&pairing_json, r#"{"paired": false}"#).unwrap();
    let state = AppState::new(
        Arc::new(crate::auth::PairingState::with_path(pairing_json.clone())),
        crate::ipc::StateIpcClient::disconnected(),
        crate::ipc::MavlinkIpcClient::new(root.join("mavlink.sock")),
        crate::ipc::LogdQueryClient::new(root.join("logd-query.sock")),
        root.join("board.json"),
        crate::state::PairingPaths {
            config,
            pairing_json,
            wfb_key_dir: root.join("wfb"),
            bind_state: root.join("bind-state.json"),
            profile_conf: root.join("profile.conf"),
            mesh_role: root.join("mesh-role"),
            relay_secret: root.join("relay-peer-secret"),
        },
        Arc::new(crate::dashboard_pin::DashboardPin::with_path(
            root.join("dashboard-pin.json"),
        )),
        Arc::new(crate::mcp::McpTokenStore::with_path(
            root.join("mcp-token.json"),
        )),
    )
    .with_plugins(lifecycle)
    .with_run_dir(root.join("run"));
    state.state.set_snapshot_for_test(json!({"armed": false}));
    state
}

/// A stored `.adosplug` carrying `padding` extra bytes in a GCS asset.
fn archive(padding: usize) -> Vec<u8> {
    let mut buf = Vec::new();
    {
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
        let stored = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        zip.start_file("manifest.yaml", stored).unwrap();
        zip.write_all(MANIFEST.as_bytes()).unwrap();
        zip.start_file("gcs/index.js", stored).unwrap();
        zip.write_all(b"export default 1;").unwrap();
        if padding > 0 {
            zip.start_file("gcs/asset.bin", stored).unwrap();
            zip.write_all(&vec![0u8; padding]).unwrap();
        }
        zip.finish().unwrap();
    }
    buf
}

async fn upload(state: &AppState, bytes: &[u8], digest: Option<&str>) -> (StatusCode, Value) {
    let boundary = "ados-test-boundary";
    let mut body = format!(
        "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"web.adosplug\"\r\n\
         Content-Type: application/octet-stream\r\n\r\n"
    )
    .into_bytes();
    body.extend_from_slice(bytes);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    let mut request = Request::builder()
        .method("POST")
        .uri("/api/plugins/parse")
        .header(
            "content-type",
            format!("multipart/form-data; boundary={boundary}"),
        );
    if let Some(d) = digest {
        request = request.header("X-Content-SHA256", d);
    }
    let router = crate::routes::build_router(state.clone(), false);
    let resp = router
        .oneshot(request.body(Body::from(body)).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, serde_json::from_slice(&bytes).unwrap())
}

#[tokio::test]
async fn second_parse_gets_429() {
    let dir = tempfile::tempdir().unwrap();
    let state = state_in(dir.path());
    // An install already holds the slot.
    let held = try_archive_slot().expect("the slot is free");
    let (status, body) = upload(&state, &archive(0), None).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(
        body,
        json!({"error": "E_BUSY", "message": "another install is running"})
    );
    drop(held);
    let (status, body) = upload(&state, &archive(0), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

#[tokio::test]
async fn digest_mismatch_401() {
    let dir = tempfile::tempdir().unwrap();
    let state = state_in(dir.path());
    let wrong = hex::encode(Sha256::digest(b"some other archive"));
    let (status, body) = upload(&state, &archive(0), Some(&wrong)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body, json!({"error": "E_DIGEST_MISMATCH"}));
    // The slot is released after a refused upload.
    assert!(try_archive_slot().is_some());
}

#[tokio::test]
async fn digest_match_accepts_2mib() {
    let dir = tempfile::tempdir().unwrap();
    let state = state_in(dir.path());
    let bytes = archive(2 * 1024 * 1024);
    assert!(bytes.len() > 2 * 1024 * 1024);
    // Upper-case hex is the same digest.
    let digest = hex::encode(Sha256::digest(&bytes)).to_ascii_uppercase();
    let (status, body) = upload(&state, &bytes, Some(&digest)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["plugin_id"], json!("com.example.web"));
}
