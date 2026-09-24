//! The native plugin lifecycle through the real router, against a tempdir
//! plugin layout and a recording service backend.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tower::util::ServiceExt;

use ados_plugin_host::backend::RecordingBackend;
use ados_plugin_host::download::{
    DownloadBody, DownloadError, DownloadSource, StaticDownloadSource, DOWNLOAD_MAX_BYTES,
};
use ados_plugin_host::manifest::PluginManifest;
use ados_plugin_host::supervisor::{Paths, PluginSupervisor};

use super::install::BUILTIN_MANIFESTS;
use super::PluginLifecycle;
use crate::state::AppState;

const WEB_ID: &str = "com.example.web";

const WEB_MANIFEST: &str = "id: com.example.web\nversion: 1.0.0\nname: Web\nlicense: MIT\n\
compatibility:\n  ados_version: \">=0.1.0\"\n\
agent:\n  entrypoint: agent/py/x.py\n  permissions:\n    - event.publish\n    - mavlink.read\n\
gcs:\n  entrypoint: gcs/index.js\n  contributes:\n    panels:\n      - id: main\n";

struct Fixture {
    dir: tempfile::TempDir,
    state: AppState,
    paths: Paths,
}

fn plugin_paths(root: &Path) -> Paths {
    Paths {
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
    }
}

/// A node of `profile` with the given `pairing.json` body.
fn fixture_with(
    profile: &'static str,
    pairing: &str,
    download: Option<Arc<dyn DownloadSource>>,
) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let paths = plugin_paths(root);
    let backend = Arc::new(RecordingBackend::default());
    let factory_paths = paths.clone();
    let lifecycle = PluginLifecycle::new(
        move || {
            PluginSupervisor::new(factory_paths.clone(), false, None, "1.0.0")
                .with_backend(backend.clone())
                .with_profile(profile)
        },
        download,
        paths.run_dir.clone(),
        paths.run_dir.clone(),
    )
    .with_model_delivery_socket(root.join("residual.sock"));

    let config = root.join("config.yaml");
    std::fs::write(
        &config,
        format!("agent:\n  device_id: node-7\n  profile: {profile}\n"),
    )
    .unwrap();
    let pairing_json = root.join("pairing.json");
    std::fs::write(&pairing_json, pairing).unwrap();
    let pairing_paths = crate::state::PairingPaths {
        config,
        pairing_json: pairing_json.clone(),
        wfb_key_dir: root.join("wfb"),
        bind_state: root.join("bind-state.json"),
        profile_conf: root.join("profile.conf"),
        mesh_role: root.join("mesh-role"),
        relay_secret: root.join("relay-peer-secret"),
    };
    let state = AppState::new(
        Arc::new(crate::auth::PairingState::with_path(pairing_json)),
        crate::ipc::StateIpcClient::disconnected(),
        crate::ipc::MavlinkIpcClient::new(root.join("mavlink.sock")),
        crate::ipc::LogdQueryClient::new(root.join("logd-query.sock")),
        root.join("board.json"),
        pairing_paths,
        Arc::new(crate::dashboard_pin::DashboardPin::with_path(
            root.join("dashboard-pin.json"),
        )),
        Arc::new(crate::mcp::McpTokenStore::with_path(
            root.join("mcp-token.json"),
        )),
    )
    .with_plugins(lifecycle);
    Fixture { dir, state, paths }
}

fn fixture() -> Fixture {
    fixture_with("drone", r#"{"paired": false}"#, None)
}

/// A stored (uncompressed) `.adosplug` with the given entries.
fn archive(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut buf = Vec::new();
    {
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
        let stored = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        for (name, bytes) in entries {
            zip.start_file(*name, stored).unwrap();
            zip.write_all(bytes).unwrap();
        }
        zip.finish().unwrap();
    }
    buf
}

fn web_archive() -> Vec<u8> {
    archive(&[
        ("manifest.yaml", WEB_MANIFEST.as_bytes()),
        ("gcs/index.js", b"export default 1;"),
    ])
}

fn multipart(file_name: &str, bytes: &[u8]) -> (String, Vec<u8>) {
    let boundary = "ados-test-boundary";
    let mut body = format!(
        "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{file_name}\"\r\n\
         Content-Type: application/octet-stream\r\n\r\n"
    )
    .into_bytes();
    body.extend_from_slice(bytes);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    (format!("multipart/form-data; boundary={boundary}"), body)
}

async fn send(state: &AppState, request: Request<Body>) -> (StatusCode, Vec<u8>) {
    let router = crate::routes::build_router(state.clone(), false);
    let resp = router.oneshot(request).await.unwrap();
    let status = resp.status();
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap()
        .to_vec();
    (status, body)
}

async fn get(state: &AppState, uri: &str) -> (StatusCode, Vec<u8>) {
    send(
        state,
        Request::builder().uri(uri).body(Body::empty()).unwrap(),
    )
    .await
}

async fn post_json(state: &AppState, uri: &str, body: Value) -> (StatusCode, Value) {
    let (status, bytes) = send(
        state,
        Request::builder()
            .method("POST")
            .uri(uri)
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap(),
    )
    .await;
    (status, serde_json::from_slice(&bytes).unwrap())
}

async fn upload(state: &AppState, uri: &str, bytes: &[u8]) -> (StatusCode, Value) {
    let (content_type, body) = multipart("web.adosplug", bytes);
    let (status, bytes) = send(
        state,
        Request::builder()
            .method("POST")
            .uri(uri)
            .header("content-type", content_type)
            .body(Body::from(body))
            .unwrap(),
    )
    .await;
    (status, serde_json::from_slice(&bytes).unwrap())
}

fn envelope(body: &Value) -> (u64, &str) {
    assert_eq!(body["ok"], json!(false), "{body}");
    (
        body["code"].as_u64().unwrap(),
        body["kind"].as_str().unwrap(),
    )
}

async fn install_web(f: &Fixture) -> Value {
    let (status, body) = upload(
        &f.state,
        "/api/plugins/install?requested_permissions=event.publish,mavlink.read",
        &web_archive(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    body
}

fn read_job(f: &Fixture, job: &str) -> Value {
    let path = f.paths.run_dir.join(format!("plugin_install_{job}.json"));
    serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
}

#[tokio::test]
async fn not_found_is_the_python_envelope_byte_for_byte() {
    let f = fixture();
    let (status, body) = get(&f.state, "/api/plugins/com.example.none").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(
        String::from_utf8(body).unwrap(),
        r#"{"ok":false,"code":14,"kind":"not_found","detail":"plugin com.example.none not installed"}"#
    );
    // A lifecycle write on a missing plugin is the same refusal, not a 500.
    let (status, body) =
        post_json(&f.state, "/api/plugins/com.example.none/enable", json!({})).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(envelope(&body), (14, "not_found"));
}

#[tokio::test]
async fn a_malformed_signature_is_code_10() {
    let f = fixture();
    let bad = archive(&[
        ("manifest.yaml", WEB_MANIFEST.as_bytes()),
        ("gcs/index.js", b"x"),
        ("SIGNATURE", b"only-one-line\n"),
    ]);
    for route in ["/api/plugins/parse", "/api/plugins/install"] {
        let (status, body) = upload(&f.state, route, &bad).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{route}");
        assert_eq!(envelope(&body), (10, "signature_invalid"), "{route}");
    }
}

#[tokio::test]
async fn install_grants_the_requested_permissions_and_refuses_an_undeclared_one() {
    let f = fixture();
    let body = install_web(&f).await;
    assert_eq!(body["plugin_id"], json!(WEB_ID));
    assert_eq!(body["granted"], json!(["event.publish", "mavlink.read"]));
    assert_eq!(body["job_id"], Value::Null);

    let (status, body) = post_json(
        &f.state,
        "/api/plugins/com.example.web/grant",
        json!({"permission_id": "mavlink.write"}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(envelope(&body), (11, "permission_deny"));

    // The detail read carries the grant set and the GCS block the GCS mounts from.
    let (status, detail) = get(&f.state, "/api/plugins/com.example.web").await;
    assert_eq!(status, StatusCode::OK);
    let detail: Value = serde_json::from_slice(&detail).unwrap();
    assert_eq!(
        detail["granted_capabilities"],
        json!(["event.publish", "mavlink.read"])
    );
    assert_eq!(detail["manifest"]["gcs"]["isolation"], json!("iframe"));
    assert_eq!(detail["manifest"]["license"], json!("MIT"));
    assert_eq!(detail["manifest"]["halves"], json!(["agent", "gcs"]));
    assert_eq!(
        detail["manifest"]["permissions"][0]["label"],
        json!("Publish events on the plugin event bus")
    );

    // Revoke answers what is still granted.
    let (status, bytes) = send(
        &f.state,
        Request::builder()
            .method("DELETE")
            .uri("/api/plugins/com.example.web/perms/mavlink.read")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let revoked: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(revoked["granted"], json!(["event.publish"]));
}

/// A download body the source reports as larger than the archive cap.
struct Oversized;

impl DownloadSource for Oversized {
    fn open(&self, _url: &str) -> Result<DownloadBody, DownloadError> {
        Ok(DownloadBody {
            reader: Box::new(std::io::empty()),
            content_length: Some(DOWNLOAD_MAX_BYTES + 1),
        })
    }
}

#[tokio::test]
async fn an_oversized_download_is_archive_too_large() {
    let f = fixture_with("drone", r#"{"paired": false}"#, Some(Arc::new(Oversized)));
    let url = "https://github.com/example/web/releases/download/v1/web.adosplug";
    let (status, body) = post_json(
        &f.state,
        "/api/plugins/parse_from_url",
        json!({ "url": url }),
    )
    .await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(envelope(&body), (13, "archive_too_large"));
    // A host off the allowlist never reaches the transport.
    let (status, body) = post_json(
        &f.state,
        "/api/plugins/parse_from_url",
        json!({ "url": "https://example.com/web.adosplug" }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(envelope(&body), (2, "url_invalid"));
}

#[tokio::test]
async fn a_target_profile_refusal_is_code_18_and_fails_the_job() {
    let f = fixture();
    let manifest = "id: com.example.ws\nversion: 1.0.0\nname: Ws\ncompatibility:\n  \
                    ados_version: \">=0.1.0\"\nagent:\n  entrypoint: agent/py/x.py\n  \
                    target_profiles: [workstation]\n";
    let (status, body) = upload(
        &f.state,
        "/api/plugins/install?job_id=job-1",
        &archive(&[("manifest.yaml", manifest.as_bytes())]),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(envelope(&body), (18, "incompatible"));
    assert_eq!(
        body["detail"],
        json!("incompatible: target_profiles excludes drone")
    );
    let job = read_job(&f, "job-1");
    assert_eq!(job["stage"], json!("failed"));
    assert_eq!(job["kind"], json!("incompatible"));
    assert_eq!(job["jobId"], json!("job-1"));
}

#[tokio::test]
async fn url_install_walks_the_job_and_reports_the_archive_digest() {
    let url = "https://github.com/example/web/releases/download/v1/web.adosplug";
    let bytes = web_archive();
    let sha = hex::encode(<sha2::Sha256 as sha2::Digest>::digest(&bytes));
    let source = StaticDownloadSource::default().with(url, bytes);
    let f = fixture_with("drone", r#"{"paired": false}"#, Some(Arc::new(source)));

    let (status, body) = post_json(
        &f.state,
        "/api/plugins/install_from_url",
        json!({"url": url, "expected_sha256": "00", "job_id": "j2"}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(envelope(&body), (12, "sha256_mismatch"));
    assert_eq!(read_job(&f, "j2")["stage"], json!("failed"));

    let (status, body) = post_json(
        &f.state,
        "/api/plugins/install_from_url",
        json!({"url": url, "expected_sha256": sha, "job_id": "j3",
               "requested_permissions": ["event.publish"]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["sha256"], json!(sha));
    assert_eq!(body["granted"], json!(["event.publish"]));
    let job = read_job(&f, "j3");
    assert_eq!(job["stage"], json!("completed"));
    assert_eq!(job["pluginId"], json!(WEB_ID));
}

#[tokio::test]
async fn a_catalog_install_must_pin_the_catalog_digest() {
    let f = fixture();
    let catalog = super::read::bundled_catalog();
    let entry = &catalog["plugins"][0];
    let url = entry["download_url"].as_str().unwrap();
    let (status, body) = post_json(
        &f.state,
        "/api/plugins/install_from_url",
        json!({"url": url, "from_catalog": true}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(envelope(&body), (2, "sha256_required"));
    let (status, body) = post_json(
        &f.state,
        "/api/plugins/install_from_url",
        json!({"url": url, "from_catalog": true, "expected_sha256": "ab".repeat(32)}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(envelope(&body), (12, "catalog_mismatch"));
}

#[tokio::test]
async fn the_catalog_is_served_natively() {
    let f = fixture();
    let (status, body) = get(&f.state, "/api/v1/plugins/catalog").await;
    assert_eq!(status, StatusCode::OK);
    let served: Value = serde_json::from_slice(&body).unwrap();
    let bundled: Value = serde_json::from_str(include_str!(
        "../../../../../src/ados/data/plugin-catalog.json"
    ))
    .unwrap();
    assert_eq!(served, bundled);
}

#[tokio::test]
async fn gcs_assets_manifest_and_attestation_are_served_from_the_install() {
    let f = fixture();
    install_web(&f).await;

    let (status, body) = get(&f.state, "/api/plugins/com.example.web/gcs/index.js").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, b"export default 1;");
    let (status, body) = get(
        &f.state,
        "/api/plugins/com.example.web/gcs/..%2f..%2fstate%2fplugin-state.json",
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let body: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(envelope(&body), (12, "manifest_invalid"));
    let (status, _) = get(&f.state, "/api/plugins/com.example.web/gcs/missing.js").await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let (status, body) = get(&f.state, "/api/plugins/com.example.web/manifest").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, WEB_MANIFEST.as_bytes());

    let (status, body) = get(&f.state, "/api/plugins/com.example.web/attestation").await;
    assert_eq!(status, StatusCode::OK);
    let att: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(att["signature"], Value::Null);
    let manifest_sha = hex::encode(<sha2::Sha256 as sha2::Digest>::digest(WEB_MANIFEST));
    assert!(att["files"]
        .as_array()
        .unwrap()
        .iter()
        .any(|e| e["path"] == json!("manifest.yaml") && e["sha256"] == json!(manifest_sha)));
}

#[tokio::test]
async fn the_capability_token_needs_a_pairing_and_carries_the_grants() {
    let f = fixture();
    install_web(&f).await;
    let (status, body) = post_json(
        &f.state,
        "/api/plugins/capability-token",
        json!({"plugin_id": WEB_ID}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(envelope(&body), (11, "not_paired"));

    let paired = fixture_with(
        "drone",
        r#"{"paired": true, "api_key": "ados_test_pairing_key"}"#,
        None,
    );
    install_web(&paired).await;
    let (status, body) = post_json(
        &paired.state,
        "/api/plugins/capability-token",
        json!({"plugin_id": WEB_ID, "operator_id": "op"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["issuer"], json!("agent:node-7"));
    assert_eq!(
        body["grantedCapabilities"],
        json!(["event.publish", "mavlink.read"])
    );
    let expected = super::token::mint_token(
        "ados_test_pairing_key",
        WEB_ID,
        "node-7",
        "op",
        &["event.publish".to_string(), "mavlink.read".to_string()],
        body["expiresAt"].as_i64().unwrap(),
    );
    assert_eq!(body["token"], json!(expected));
}

#[test]
fn every_builtin_manifest_parses() {
    for (id, yaml) in BUILTIN_MANIFESTS {
        let manifest = PluginManifest::from_yaml_text(yaml).unwrap();
        assert_eq!(manifest.id, id);
    }
}

#[tokio::test]
async fn a_builtin_installs_unsigned_from_the_agent_package() {
    let f = fixture();
    let (status, body) = post_json(
        &f.state,
        "/api/plugins/install_builtin",
        json!({"plugin_id": "io.altnautica.geofence"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["signer_id"], Value::Null);
    let (_, list) = get(&f.state, "/api/plugins").await;
    let list: Value = serde_json::from_slice(&list).unwrap();
    assert_eq!(
        list["installs"][0]["source_uri"],
        json!("builtin:io.altnautica.geofence")
    );
    let (status, body) = post_json(
        &f.state,
        "/api/plugins/install_builtin",
        json!({"plugin_id": "io.altnautica.nope"}),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(envelope(&body), (14, "not_found"));
}

/// Answer one HTTP request on a Unix socket with `response`, handing back the
/// request head it read.
fn serve_once_unix(path: &Path, response: Vec<u8>) -> tokio::task::JoinHandle<String> {
    let listener = tokio::net::UnixListener::bind(path).unwrap();
    tokio::spawn(async move {
        let (mut conn, _) = listener.accept().await.unwrap();
        let head = read_head(&mut conn).await;
        conn.write_all(&response).await.unwrap();
        conn.flush().await.unwrap();
        head
    })
}

/// Read up to the end of an HTTP head.
async fn read_head<S: tokio::io::AsyncRead + Unpin>(conn: &mut S) -> String {
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        if conn.read(&mut byte).await.unwrap() == 0 {
            break;
        }
        head.push(byte[0]);
    }
    String::from_utf8(head).unwrap()
}

#[tokio::test]
async fn enable_records_the_models_the_residual_delivered() {
    let f = fixture();
    install_web(&f).await;
    let models = r#"{"ok":true,"plugin_id":"com.example.web","models":[{"state":"resolved","model_id":"m1"}]}"#;
    let residual = serve_once_unix(
        &f.dir.path().join("residual.sock"),
        format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{models}",
            models.len()
        )
        .into_bytes(),
    );
    let (status, body) =
        post_json(&f.state, "/api/plugins/com.example.web/enable", json!({})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(residual
        .await
        .unwrap()
        .starts_with("POST /api/vision/plugin-models/com.example.web/deliver "));
    let installs = ados_plugin_host::state::load_state(Some(&f.paths.state_path));
    assert_eq!(
        installs[0].model_status,
        Some(json!([{"state": "resolved", "model_id": "m1"}]))
    );
}

fn http_socket(f: &Fixture) -> PathBuf {
    let dir = f.paths.run_dir.join("plugin-http").join(WEB_ID);
    std::fs::create_dir_all(&dir).unwrap();
    dir.join("http.sock")
}

#[tokio::test]
async fn the_passthrough_is_503_without_a_plugin_socket() {
    let f = fixture();
    let (status, body) = get(&f.state, "/api/plugins/com.example.web/x/status").await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        String::from_utf8(body).unwrap(),
        r#"{"detail":"plugin com.example.web is not serving HTTP"}"#
    );
}

#[tokio::test]
async fn the_passthrough_rewrites_the_path_and_drops_the_operator_key() {
    let f = fixture();
    let upstream = serve_once_unix(
        &http_socket(&f),
        b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nhi".to_vec(),
    );
    let (status, body) = send(
        &f.state,
        Request::builder()
            .uri("/api/plugins/com.example.web/x/jobs/a%20b?since=5")
            .header("x-ados-key", "secret")
            .header("x-plugin-header", "kept")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, b"hi");
    let head = upstream.await.unwrap().to_ascii_lowercase();
    assert!(
        head.starts_with("get /jobs/a%20b?since=5 http/1.1\r\n"),
        "{head}"
    );
    assert!(head.contains("x-plugin-header: kept"));
    assert!(!head.contains("x-ados-key"), "{head}");
}

/// Serve the router on loopback TCP, so a WebSocket upgrade runs end to end.
async fn serve_tcp(state: &AppState) -> std::net::SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = crate::routes::build_router(state.clone(), false);
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    addr
}

fn ws_handshake(path: &str, protocols: &str) -> String {
    format!(
        "GET {path} HTTP/1.1\r\nHost: node\r\nConnection: Upgrade\r\nUpgrade: websocket\r\n\
         Sec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
         Sec-WebSocket-Protocol: {protocols}\r\n\r\n"
    )
}

/// Read one unmasked server frame: `(opcode, payload)`.
async fn read_frame(conn: &mut tokio::net::TcpStream) -> (u8, Vec<u8>) {
    let mut hdr = [0u8; 2];
    conn.read_exact(&mut hdr).await.unwrap();
    let mut len = u64::from(hdr[1] & 0x7f);
    if len == 126 {
        let mut ext = [0u8; 2];
        conn.read_exact(&mut ext).await.unwrap();
        len = u64::from(u16::from_be_bytes(ext));
    }
    let mut payload = vec![0u8; len as usize];
    conn.read_exact(&mut payload).await.unwrap();
    (hdr[0] & 0x0f, payload)
}

#[tokio::test]
async fn a_passthrough_websocket_pipes_both_ways_and_answers_the_ticket_protocol() {
    let f = fixture();
    let listener = tokio::net::UnixListener::bind(http_socket(&f)).unwrap();
    let upstream = tokio::spawn(async move {
        let (mut conn, _) = listener.accept().await.unwrap();
        let head = read_head(&mut conn).await;
        conn.write_all(
            b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
              Sec-WebSocket-Accept: s3pPLMBiTxaQ9kYGzzhZRbK+xOo=\r\n\r\n",
        )
        .await
        .unwrap();
        conn.write_all(b"\x81\x05hello").await.unwrap();
        let mut from_client = [0u8; 4];
        conn.read_exact(&mut from_client).await.unwrap();
        (head, from_client)
    });
    let addr = serve_tcp(&f.state).await;
    let mut conn = tokio::net::TcpStream::connect(addr).await.unwrap();
    conn.write_all(
        ws_handshake(
            "/api/plugins/com.example.web/x/live?a=1",
            "ados-ws-ticket, v1|plugins.http:com.example.web|1|2|ff",
        )
        .as_bytes(),
    )
    .await
    .unwrap();
    let response = read_head(&mut conn).await.to_ascii_lowercase();
    assert!(response.starts_with("http/1.1 101"), "{response}");
    assert!(
        response.contains("sec-websocket-protocol: ados-ws-ticket"),
        "{response}"
    );
    assert_eq!(read_frame(&mut conn).await, (0x1, b"hello".to_vec()));
    conn.write_all(b"ping").await.unwrap();
    let (head, from_client) = upstream.await.unwrap();
    let head = head.to_ascii_lowercase();
    assert!(head.starts_with("get /live?a=1 http/1.1\r\n"), "{head}");
    assert!(
        !head.contains("sec-websocket-protocol"),
        "the ticket leaked: {head}"
    );
    assert_eq!(&from_client, b"ping");
}

#[tokio::test]
async fn the_job_stream_sends_the_stage_and_closes_on_a_terminal_one() {
    let f = fixture();
    std::fs::create_dir_all(&f.paths.run_dir).unwrap();
    std::fs::write(
        f.paths.run_dir.join("plugin_install_job-9.json"),
        r#"{"jobId":"job-9","pluginId":"com.example.web","stage":"completed","updatedAt":1}"#,
    )
    .unwrap();
    let addr = serve_tcp(&f.state).await;
    let mut conn = tokio::net::TcpStream::connect(addr).await.unwrap();
    conn.write_all(
        ws_handshake("/api/plugins/jobs/job-9", "ados-ws-ticket, v1|x|1|2|ff").as_bytes(),
    )
    .await
    .unwrap();
    let response = read_head(&mut conn).await.to_ascii_lowercase();
    assert!(response.starts_with("http/1.1 101"), "{response}");
    assert!(response.contains("sec-websocket-protocol: ados-ws-ticket"));
    let (opcode, payload) = read_frame(&mut conn).await;
    assert_eq!(opcode, 0x1);
    let stage: Value = serde_json::from_slice(&payload).unwrap();
    assert_eq!(stage["stage"], json!("completed"));
    assert_eq!(stage["pluginId"], json!(WEB_ID));
    let (opcode, _) = read_frame(&mut conn).await;
    assert_eq!(opcode, 0x8, "a terminal stage closes the stream");
}

#[tokio::test]
async fn pin_and_auto_update_persist_on_the_install() {
    let f = fixture();
    install_web(&f).await;
    let (status, body) = post_json(
        &f.state,
        "/api/plugins/com.example.web/pin",
        json!({"version": "1.0.0"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["pinned_version"], json!("1.0.0"));
    let (status, _) = post_json(
        &f.state,
        "/api/plugins/com.example.web/auto-update",
        json!({"enabled": false}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let installs = ados_plugin_host::state::load_state(Some(&f.paths.state_path));
    assert_eq!(installs[0].pinned_version.as_deref(), Some("1.0.0"));
    assert!(!installs[0].auto_update);
    let (status, body) =
        post_json(&f.state, "/api/plugins/com.example.none/unpin", json!({})).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(envelope(&body), (14, "not_found"));
}
