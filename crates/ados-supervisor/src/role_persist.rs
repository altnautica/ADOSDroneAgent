//! The supervisor loop's handling of a `set_role` request: run the transition,
//! then persist `ground_station.role` so the configured role matches the role
//! the node now runs and a reboot restores it.
//!
//! The config write goes through the control plane (`PUT /api/config` on its
//! Unix socket, which the control plane treats as an on-box caller), the one
//! writer of `config.yaml`, rather than a second YAML writer here.

use std::path::Path;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::lifecycle::Supervisor;
use crate::role::{role_reply, RoleRequest};

/// The control plane's Unix socket file name under the run dir.
pub const CONTROL_SOCK_NAME: &str = "control.sock";

/// Ceiling on the whole config write (connect, request, response).
const PERSIST_TIMEOUT: Duration = Duration::from_secs(5);

/// Cap on the response bytes read back; only the status line matters.
const MAX_RESPONSE_BYTES: u64 = 64 * 1024;

/// `PUT /api/config {"key":"ground_station.role","value":role}` over the
/// control socket at `sock`. `Ok` only on a 2xx answer.
pub async fn persist_configured_role(sock: &Path, role: &str) -> Result<(), String> {
    let body = serde_json::json!({"key": "ground_station.role", "value": role}).to_string();
    let request = format!(
        "PUT /api/config HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let exchange = async {
        let mut stream = tokio::net::UnixStream::connect(sock)
            .await
            .map_err(|e| format!("connect {}: {e}", sock.display()))?;
        stream
            .write_all(request.as_bytes())
            .await
            .map_err(|e| format!("write: {e}"))?;
        let mut response = Vec::new();
        stream
            .take(MAX_RESPONSE_BYTES)
            .read_to_end(&mut response)
            .await
            .map_err(|e| format!("read: {e}"))?;
        Ok::<_, String>(response)
    };
    let response = tokio::time::timeout(PERSIST_TIMEOUT, exchange)
        .await
        .map_err(|_| "timed out".to_string())??;
    let status_line = response
        .split(|b| *b == b'\n')
        .next()
        .map(String::from_utf8_lossy)
        .unwrap_or_default();
    let status = status_line.split_whitespace().nth(1).unwrap_or("");
    if status.starts_with('2') && status.len() == 3 {
        Ok(())
    } else {
        Err(format!("control plane answered {:?}", status_line.trim()))
    }
}

impl Supervisor {
    /// Serve one `set_role` request from the control socket: run the
    /// transition, persist the resulting role as the configured one, and send
    /// the reply. A refused transition persists nothing. A failed config write
    /// is logged and does not fail the transition, which already ran.
    pub async fn handle_role_request(&mut self, req: RoleRequest) {
        let outcome = self.apply_role(&req.target, &req.reason).await;
        if let Ok(result) = &outcome {
            let sock = self.config().run_dir.join(CONTROL_SOCK_NAME);
            match persist_configured_role(&sock, &result.role).await {
                Ok(()) => tracing::info!(role = %result.role, "role_config_persisted"),
                Err(e) => {
                    tracing::warn!(role = %result.role, error = %e, "role_config_persist_failed")
                }
            }
        }
        let _ = req.reply.send(role_reply(outcome));
    }
}

#[cfg(test)]
mod tests {
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
            let reply =
                format!("HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
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
}
