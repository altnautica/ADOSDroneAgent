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
mod tests;
