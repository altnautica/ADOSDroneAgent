use std::sync::Arc;

use super::*;
use crate::bind::orchestrator::BindOrchestrator;
use crate::config::AgentConfig;
use crate::process_manager::NullManager;

/// A fake control plane on `sock` that answers one request with `status`
/// and hands back the raw request it received.
fn fake_control_plane(sock: &Path, status: &'static str) -> tokio::task::JoinHandle<String> {
    let listener = tokio::net::UnixListener::bind(sock).unwrap();
    tokio::spawn(async move {
        let (mut conn, _) = listener.accept().await.unwrap();
        let mut buf = Vec::new();
        let mut chunk = [0u8; 1024];
        // Read until the JSON body (the last thing sent) is complete.
        while !buf.ends_with(b"}") {
            let n = conn.read(&mut chunk).await.unwrap();
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n]);
        }
        let reply = format!("HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        conn.write_all(reply.as_bytes()).await.unwrap();
        String::from_utf8(buf).unwrap()
    })
}

fn ground_station(dir: &Path) -> Supervisor {
    let config_yaml = dir.join("config.yaml");
    std::fs::write(&config_yaml, "agent:\n  profile: ground_station\n").unwrap();
    let role_path = dir.join("mesh/role");
    std::fs::create_dir_all(role_path.parent().unwrap()).unwrap();
    std::fs::write(&role_path, "relay\n").unwrap();
    let mut config = AgentConfig::load_from(&config_yaml, &dir.join("absent.conf"), &role_path);
    config.run_dir = dir.join("run");
    std::fs::create_dir_all(&config.run_dir).unwrap();
    Supervisor::with_process_manager(
        config,
        Arc::new(BindOrchestrator::new()),
        Arc::new(NullManager),
    )
}

#[tokio::test]
async fn set_role_op_persists_role() {
    let dir = tempfile::tempdir().unwrap();
    let mut sup = ground_station(dir.path());
    let sock = dir.path().join("run").join(CONTROL_SOCK_NAME);
    let control = fake_control_plane(&sock, "200 OK");

    let (reply, answer) = tokio::sync::oneshot::channel();
    sup.handle_role_request(RoleRequest {
        target: "direct".to_string(),
        reason: "mesh_identity_missing".to_string(),
        reply,
    })
    .await;

    let reply = answer.await.unwrap();
    assert_eq!(reply["ok"], true, "{reply}");
    assert_eq!(reply["role"], "direct");
    assert_eq!(sup.config().live_role(), "direct");
    let request = control.await.unwrap();
    assert!(
        request.starts_with("PUT /api/config HTTP/1.1\r\n"),
        "{request}"
    );
    let body = request.split("\r\n\r\n").nth(1).unwrap();
    let body: serde_json::Value = serde_json::from_str(body).unwrap();
    assert_eq!(
        body,
        serde_json::json!({"key": "ground_station.role", "value": "direct"})
    );
}

#[tokio::test]
async fn a_refused_transition_persists_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let mut sup = ground_station(dir.path());
    let sock = dir.path().join("run").join(CONTROL_SOCK_NAME);
    let control = fake_control_plane(&sock, "200 OK");
    let (reply, answer) = tokio::sync::oneshot::channel();
    sup.handle_role_request(RoleRequest {
        target: "bogus".to_string(),
        reason: "test".to_string(),
        reply,
    })
    .await;
    assert_eq!(answer.await.unwrap()["error"], "E_INVALID_ROLE");
    // The control plane never saw a connection.
    assert!(tokio::time::timeout(Duration::from_millis(200), control)
        .await
        .is_err());
}

#[tokio::test]
async fn a_non_2xx_answer_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join(CONTROL_SOCK_NAME);
    let control = fake_control_plane(&sock, "423 Locked");
    let err = persist_configured_role(&sock, "relay").await.unwrap_err();
    assert!(err.contains("423"), "{err}");
    control.await.unwrap();
}
