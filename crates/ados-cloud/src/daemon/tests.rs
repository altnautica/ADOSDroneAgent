use super::*;
use crate::dispatch::loopback;
use crate::dispatch::CommandStatus;
use ados_plugin_host::download::{DownloadBody, DownloadError, DownloadSource};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// A no-network download source (never consulted on the no-plugin paths).
struct NoSource;
impl DownloadSource for NoSource {
    fn open(&self, _url: &str) -> Result<DownloadBody, DownloadError> {
        Err(DownloadError::Unparseable)
    }
}

fn no_source() -> SharedDownload {
    Arc::new(NoSource)
}

/// A per-test seen-jobs ring under a temp dir, never the node's real one.
fn seen_path() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("_seen_jobs.json");
    (dir, path)
}

/// An HTTP client on the crate's preconfigured rustls path (the same one the
/// daemon builds). reqwest needs a crypto provider set even for plain-HTTP
/// loopback requests, so the default `Client::new()` panics with "No provider
/// set" in this no-default-features crate.
fn test_client() -> reqwest::Client {
    reqwest::Client::builder()
        .use_preconfigured_tls(crate::tls::client_config())
        .build()
        .expect("test client builds with the rustls config")
}

fn supervisor() -> (tempfile::TempDir, SharedSupervisor) {
    // A temp-rooted supervisor so the test never touches /var/ados.
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let paths = Paths {
        install_dir: dir.join("plugins"),
        unit_dir: dir.join("units"),
        state_path: dir.join("state/plugin-state.json"),
        log_dir: dir.join("logs"),
        control_dir: dir.join("plugin-host"),
        loopback_guard_state: dir.join("plugin-loopback-guard.json"),
        socket_dir: dir.join("sockets"),
        token_secret: dir.join("secrets/plugin-token-secret"),
        runner: dir.join("bin/ados-plugin-runner"),
        run_dir: dir.join("run"),
        data_root: dir.join("plugin-data"),
        device_id_file: dir.join("device-id"),
    };
    let sup = Arc::new(Mutex::new(PluginSupervisor::new(
        paths, false, None, "1.0.0",
    )));
    (tmp, sup)
}

#[test]
fn board_sidecar_version_matches_registry() {
    assert_eq!(
        BOARD_SIDECAR_VERSION,
        ados_protocol::contracts::sidecar_version("board").unwrap()
    );
}

#[test]
fn live_cloud_supervisor_enforces_signing() {
    // The cloud install path is wired live (the command poll drives
    // install_archive), so the process-wide supervisor MUST enforce
    // signatures. Build it exactly as `main` does and assert the secure
    // default holds with no env override.
    let prev = std::env::var("ADOS_PLUGIN_REQUIRE_SIGNED").ok();
    std::env::remove_var("ADOS_PLUGIN_REQUIRE_SIGNED");
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let paths = Paths {
        install_dir: dir.join("plugins"),
        unit_dir: dir.join("units"),
        state_path: dir.join("state/plugin-state.json"),
        log_dir: dir.join("logs"),
        control_dir: dir.join("plugin-host"),
        loopback_guard_state: dir.join("plugin-loopback-guard.json"),
        socket_dir: dir.join("sockets"),
        token_secret: dir.join("secrets/plugin-token-secret"),
        runner: dir.join("bin/ados-plugin-runner"),
        run_dir: dir.join("run"),
        data_root: dir.join("plugin-data"),
        device_id_file: dir.join("device-id"),
    };
    let sup = PluginSupervisor::production(paths, None, env!("CARGO_PKG_VERSION"))
        .with_ungrantable_caps(ados_plugin_host::realhost::RealHost::ungrantable_caps());
    assert!(
        sup.require_signed(),
        "the live cloud install supervisor must require signed archives"
    );
    if let Some(v) = prev {
        std::env::set_var("ADOS_PLUGIN_REQUIRE_SIGNED", v);
    }
}

/// A one-shot local HTTP server that replies to a single request with the
/// given status line + JSON body, then returns the bound base URL.
async fn mock_once(status_line: &'static str, body: &'static str) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        if let Ok((mut sock, _)) = listener.accept().await {
            // Drain the request head (best-effort) so the client write completes.
            let mut buf = [0u8; 4096];
            let _ = sock.read(&mut buf).await;
            let resp = format!(
                "HTTP/1.1 {status_line}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = sock.write_all(resp.as_bytes()).await;
            let _ = sock.flush().await;
        }
    });
    format!("http://{addr}")
}

/// Serve exactly one HTTP request on loopback, answering `body` as JSON,
/// and hand back the raw request head the client sent.
async fn capture_one_request(
    body: &'static str,
) -> (String, tokio::sync::oneshot::Receiver<String>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.unwrap();
        let mut head = Vec::new();
        let mut buf = [0u8; 4096];
        while !head.windows(4).any(|w| w == b"\r\n\r\n") {
            let n = sock.read(&mut buf).await.unwrap();
            if n == 0 {
                break;
            }
            head.extend_from_slice(&buf[..n]);
        }
        let _ = tx.send(String::from_utf8_lossy(&head).into_owned());
        let resp = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let _ = sock.write_all(resp.as_bytes()).await;
    });
    (format!("http://{addr}"), rx)
}

#[tokio::test]
async fn the_command_poll_sends_the_key_as_a_header_and_only_the_device_id_as_a_query() {
    let (base, head) = capture_one_request(r#"{"commands":[]}"#).await;
    let http = test_client();
    let (_dir, seen) = seen_path();
    let (_sup_dir, sup) = supervisor();
    let mut executed = dispatch::executed_commands::ExecutedCommands::load(&seen);
    poll_commands_once(
        &http,
        &base,
        "k-secret",
        "dev-7",
        &sup,
        &no_source(),
        &mut executed,
    )
    .await;
    let head = head.await.unwrap();
    let request_line = head.lines().next().unwrap();
    assert!(
        request_line.starts_with("GET /agent/commands?deviceId=dev-7 "),
        "{request_line}"
    );
    assert!(
        !request_line.contains("k-secret"),
        "the key leaked into the URL"
    );
    assert!(
        head.to_ascii_lowercase().contains("x-ados-key: k-secret"),
        "{head}"
    );
}

#[tokio::test]
async fn unknown_command_acks_failed_not_completed() {
    // The catch-all must never fabricate success for a command with no
    // handler. No HTTP is issued on this path (route_for returns None).
    let http = test_client();
    let (_sup_dir, sup) = supervisor();
    let (_dir, seen) = seen_path();
    let cmd = serde_json::json!({"_id": "c1", "command": "totally_unknown"});
    let r = dispatch_command(&http, "totally_unknown", &cmd, &sup, &no_source(), &seen).await;
    assert_eq!(r.status, CommandStatus::Failed);
    assert_eq!(r.result["message"], "not implemented: totally_unknown");
}

#[tokio::test]
async fn restart_of_rejected_name_acks_failed() {
    // The local restart route returns `{"status":"error"}` with HTTP 200 for
    // a unit not in its allowlist; the forwarder must ack failed, not
    // completed. Exercised end-to-end through the real loopback HTTP path.
    let base = mock_once(
        "200 OK",
        r#"{"status":"error","message":"Unknown service: bogus"}"#,
    )
    .await;
    let http = test_client();
    let args = serde_json::json!({"name": "bogus"});
    let route = loopback::route_for("restart_service", &args).unwrap();
    let r = loopback::forward_to(&http, &base, "restart_service", &args, &route).await;
    assert_eq!(r.status, CommandStatus::Failed);
    assert_eq!(r.result["message"], "Unknown service: bogus");
}

#[tokio::test]
async fn restart_of_allowed_name_acks_completed() {
    // The happy path: the route confirms the restart with `{"status":"ok"}`,
    // and the forwarder carries that through as completed with the route's
    // payload in `data`.
    let base = mock_once(
        "200 OK",
        r#"{"status":"ok","message":"Restarted ados-video","unit":"ados-video"}"#,
    )
    .await;
    let http = test_client();
    let args = serde_json::json!({"name": "ados-video"});
    let route = loopback::route_for("restart_service", &args).unwrap();
    let r = loopback::forward_to(&http, &base, "restart_service", &args, &route).await;
    assert_eq!(r.status, CommandStatus::Completed);
    assert_eq!(r.result["message"], "Restarted ados-video");
    assert_eq!(r.data.unwrap()["unit"], "ados-video");
}

#[tokio::test]
async fn scan_peripherals_forwards_and_returns_real_data() {
    let base = mock_once("200 OK", r#"[{"name":"USB 0bda:a81a","type":"usb"}]"#).await;
    let http = test_client();
    let args = serde_json::Value::Null;
    let route = loopback::route_for("scan_peripherals", &args).unwrap();
    let r = loopback::forward_to(&http, &base, "scan_peripherals", &args, &route).await;
    assert_eq!(r.status, CommandStatus::Completed);
    assert!(r.data.unwrap().is_array());
}

#[tokio::test]
async fn restart_without_name_acks_failed_in_dispatch() {
    // A restart_service with no name has no route; dispatch_command must fail
    // it honestly rather than POST to a malformed path.
    let http = test_client();
    let (_sup_dir, sup) = supervisor();
    let (_dir, seen) = seen_path();
    let cmd = serde_json::json!({"_id": "c2", "command": "restart_service", "args": {}});
    let r = dispatch_command(&http, "restart_service", &cmd, &sup, &no_source(), &seen).await;
    assert_eq!(r.status, CommandStatus::Failed);
    assert_eq!(r.result["message"], "not implemented: restart_service");
}

#[tokio::test]
async fn plugin_command_for_unknown_plugin_acks_failed() {
    // A plugin lifecycle command routes in-process to the supervisor; an
    // enable of a plugin that was never installed is a real failed ACK, not
    // a fabricated success.
    let http = test_client();
    let (_sup_dir, sup) = supervisor();
    let (_dir, seen) = seen_path();
    let cmd = serde_json::json!({
        "_id": "c3",
        "command": "plugin.enable",
        "args": {"pluginId": "com.example.never-installed", "jobId": "j-unknown"}
    });
    let r = dispatch_command(&http, "plugin.enable", &cmd, &sup, &no_source(), &seen).await;
    assert_eq!(r.status, CommandStatus::Failed);
}

#[test]
fn should_emit_requires_both_an_api_key_and_a_cloud_url() {
    // Paired but no cloud URL (local mode) → off.
    assert!(!should_emit(Some("k"), ""));
    // Cloud URL but unpaired → off.
    assert!(!should_emit(None, "https://relay.example/convex"));
    // Paired AND a live cloud URL → on.
    assert!(should_emit(Some("k"), "https://relay.example/convex"));
}

fn config_with_profile(profile: &str) -> CloudConfig {
    let mut config = CloudConfig::default();
    config.agent.profile = profile.to_string();
    config
}

#[test]
fn the_advertised_profile_is_the_wire_form_and_agrees_with_the_bind_role() {
    // The internal form reached the cloud verbatim, so a default install
    // advertised `auto` and a ground station advertised `ground_station` —
    // neither of which the fleet view classifies. A missing profile.conf
    // keeps the test independent of the host it runs on.
    let dir = tempfile::tempdir().unwrap();
    let no_conf = dir.path().join("profile.conf");
    let wire = |raw: &str| config_with_profile(raw).wire_profile_at(&no_conf);
    assert_eq!(wire("ground_station"), "ground-station");
    assert_eq!(wire("ground-station"), "ground-station");
    assert_eq!(wire("drone"), "drone");
    assert_eq!(wire("workstation"), "workstation");
    assert_eq!(wire("compute"), "compute");
    // `auto` / empty / unknown resolve to the drone form, which is also the
    // bind role's safe default — one discrimination, so the two agree.
    for raw in ["auto", "", "nonsense"] {
        assert_eq!(wire(raw), "drone");
    }
    for raw in ["ground_station", "ground-station"] {
        assert_eq!(pair_role_for(&wire(raw)), "gs");
    }
    for raw in ["drone", "auto", "", "workstation"] {
        assert_eq!(pair_role_for(&wire(raw)), "drone");
    }
}

#[test]
fn the_relay_block_folds_onto_the_one_heartbeat_without_diverting_identity() {
    // The GS relay state reaches the row through the single producer. It
    // may set the relay + profile/role columns, but never the identity the
    // heartbeat base owns — a folded producer that could rewrite `deviceId`
    // would file a ground station's status under another node.
    let mut body = serde_json::json!({
        "deviceId": "gs-1",
        "version": "9.9.9",
        "uptimeSeconds": 4242,
        "boardName": "rpi4b",
        "profile": "ground-station",
    });
    let relay = gs_bridge::GsHeartbeat {
        device_id: "impostor".to_string(),
        version: "0.0.0".to_string(),
        uptime_seconds: 1,
        profile: "ground-station".to_string(),
        role: Some("receiver".to_string()),
        uplink: "wlan0".to_string(),
        mqtt_connected: true,
        throttle_state: "warn_80".to_string(),
        forwarding_video: true,
        forwarding_telemetry: true,
        ts_ms: 1234,
        telemetry: Some(serde_json::json!({"armed": true})),
    };
    fold_relay_state(&mut body, Some(&relay));
    // The relay block landed, under the camelCase keys the mutation declares.
    assert_eq!(body["uplink"], "wlan0");
    assert_eq!(body["mqttConnected"], true);
    assert_eq!(body["throttleState"], "warn_80");
    assert_eq!(body["forwardingVideo"], true);
    assert_eq!(body["forwardingTelemetry"], true);
    assert_eq!(body["tsMs"], 1234);
    assert_eq!(body["role"], "receiver");
    assert_eq!(body["telemetry"]["armed"], true);
    // The board enrichment the other producer used to wipe survives.
    assert_eq!(body["boardName"], "rpi4b");
    // Identity is the base's, not the folded slice's.
    assert_eq!(body["deviceId"], "gs-1");
    assert_eq!(body["version"], "9.9.9");
    assert_eq!(body["uptimeSeconds"], 4242);
}

#[test]
fn a_drone_heartbeat_is_untouched_by_the_relay_fold() {
    // No bridge published anything (a drone, or a GS before its first
    // republish): the payload must be byte-identical.
    let before = serde_json::json!({"deviceId": "d1", "boardName": "rock-5c-lite"});
    let mut body = before.clone();
    fold_relay_state(&mut body, None);
    assert_eq!(body, before);
}
