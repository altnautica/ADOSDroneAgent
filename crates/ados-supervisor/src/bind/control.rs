//! Supervisor control socket — the cross-process trigger seam for the bind FSM.
//!
//! The bind orchestrator lives in this (supervisor) process, but a bind is
//! triggered from the FastAPI `/wfb/pair/local-bind` route + the cloud auto-pair
//! supervisor, which run in OTHER processes. They reach the orchestrator over a
//! Unix socket at [`SUPERVISOR_SOCK`] speaking one newline-JSON request →
//! newline-JSON response per connection:
//!   - `{"op":"start_bind","role":"drone","peer_device_id":null,"source":"operator"}`
//!     → blocks for the whole rendezvous → `{"ok":true,"session":{…to_json…}}`
//!     or `{"ok":false,"error":"E_BIND_IN_PROGRESS"}` when one already runs.
//!   - `{"op":"bind_status"}` → `{"ok":true,"session":{…}|null}`.
//!   - `{"op":"cancel_bind"}` → aborts the in-flight session → `{"ok":true}`.
//!
//! `cancel_bind` arrives on a SEPARATE connection from the blocked `start_bind`,
//! so it routes through [`BindOrchestrator::cancel_current`] (a notify), not the
//! per-call cancel future. The caller (FastAPI) applies its own wall-clock
//! timeout and fires `cancel_bind` on timeout, matching the Python route's
//! `wait_for` + per-request cancel_event.

use std::path::Path;
use std::sync::Arc;

use serde::Deserialize;
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};

use super::orchestrator::{BindOrchestrator, BindStartError};
use super::BindRole;

/// Supervisor control socket path (sibling to mavlink.sock / state.sock).
pub const SUPERVISOR_SOCK: &str = "/run/ados/supervisor.sock";

/// Cap on a single request line so a malformed client can't grow the buffer.
const MAX_REQUEST_BYTES: usize = 64 * 1024;

#[derive(Debug, Deserialize)]
struct Request {
    op: String,
    #[serde(default)]
    role: Option<String>,
    #[serde(default)]
    peer_device_id: Option<String>,
    #[serde(default)]
    source: Option<String>,
}

/// Bind the control socket and serve requests until the listener errors. Run as
/// its own task from the supervisor main loop. Removes a stale socket first and
/// chmods it 0660 (root-owned; the api + cloud services run as root on target).
/// Returns only on a bind error; the accept loop never exits on the happy path.
pub async fn serve(orch: Arc<BindOrchestrator>, sock_path: &Path) -> std::io::Result<()> {
    // A stale socket from a prior run makes bind() fail with EADDRINUSE.
    let _ = std::fs::remove_file(sock_path);
    if let Some(parent) = sock_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let listener = UnixListener::bind(sock_path)?;
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(sock_path, std::fs::Permissions::from_mode(0o660));
    }
    tracing::info!(path = %sock_path.display(), "supervisor control socket listening");

    loop {
        match listener.accept().await {
            Ok((stream, _addr)) => {
                let orch = orch.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle_conn(stream, orch).await {
                        tracing::debug!(error = %e, "control conn error");
                    }
                });
            }
            Err(e) => {
                tracing::warn!(error = %e, "control accept failed");
                // Brief backoff so a persistent accept error can't hot-spin.
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            }
        }
    }
}

/// Read one newline-terminated request, dispatch it, write one newline-
/// terminated response.
async fn handle_conn(mut stream: UnixStream, orch: Arc<BindOrchestrator>) -> std::io::Result<()> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            break; // EOF before newline — dispatch whatever we have.
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.contains(&b'\n') || buf.len() > MAX_REQUEST_BYTES {
            break;
        }
    }
    let line = match buf.iter().position(|&b| b == b'\n') {
        Some(i) => &buf[..i],
        None => &buf[..],
    };
    let resp = dispatch(line, &orch).await;
    let mut body = serde_json::to_vec(&resp)
        .unwrap_or_else(|_| br#"{"ok":false,"error":"E_ENCODE"}"#.to_vec());
    body.push(b'\n');
    stream.write_all(&body).await?;
    stream.flush().await?;
    Ok(())
}

/// Parse + route one request to the orchestrator. Pure async over the
/// orchestrator handle — unit-testable without a socket.
async fn dispatch(line: &[u8], orch: &Arc<BindOrchestrator>) -> Value {
    let req: Request = match serde_json::from_slice(line) {
        Ok(r) => r,
        Err(e) => return json!({"ok": false, "error": format!("E_BAD_REQUEST: {e}")}),
    };
    match req.op.as_str() {
        "start_bind" => {
            let Some(role) = req.role.as_deref().and_then(BindRole::parse) else {
                return json!({"ok": false, "error": "E_BAD_ROLE"});
            };
            let source = req.source.as_deref().unwrap_or("operator");
            match orch
                .start_local_bind(
                    role,
                    req.peer_device_id,
                    source,
                    std::future::pending::<()>(),
                )
                .await
            {
                Ok(session) => json!({"ok": true, "session": session}),
                Err(BindStartError::Busy) => {
                    json!({"ok": false, "error": "E_BIND_IN_PROGRESS"})
                }
            }
        }
        "bind_status" => json!({"ok": true, "session": orch.status().await}),
        "cancel_bind" => {
            orch.cancel_current();
            json!({"ok": true})
        }
        other => json!({"ok": false, "error": format!("E_UNKNOWN_OP: {other}")}),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn dispatch_status_when_idle_is_null_session() {
        let orch = Arc::new(BindOrchestrator::new());
        let v = dispatch(br#"{"op":"bind_status"}"#, &orch).await;
        assert_eq!(v["ok"], true);
        assert!(v["session"].is_null());
    }

    #[tokio::test]
    async fn dispatch_cancel_is_ok_when_idle() {
        let orch = Arc::new(BindOrchestrator::new());
        let v = dispatch(br#"{"op":"cancel_bind"}"#, &orch).await;
        assert_eq!(v["ok"], true);
    }

    #[tokio::test]
    async fn dispatch_bad_json_and_bad_op_and_bad_role() {
        let orch = Arc::new(BindOrchestrator::new());
        assert_eq!(dispatch(b"not json", &orch).await["ok"], false);
        assert_eq!(dispatch(br#"{"op":"frob"}"#, &orch).await["ok"], false);
        let bad_role = dispatch(br#"{"op":"start_bind","role":"bogus"}"#, &orch).await;
        assert_eq!(bad_role["ok"], false);
        assert_eq!(bad_role["error"], "E_BAD_ROLE");
    }

    #[tokio::test]
    async fn dispatch_start_bind_drone_fails_preflight_off_rig() {
        // No /etc/bind.key on the dev host → the FSM lands on "failed", and the
        // op still returns ok:true with the terminal session (a successful RPC
        // carrying a failed bind, which is what the FastAPI route relays).
        let orch = Arc::new(BindOrchestrator::new());
        let v = dispatch(
            br#"{"op":"start_bind","role":"drone","source":"operator"}"#,
            &orch,
        )
        .await;
        assert_eq!(v["ok"], true);
        assert_eq!(v["session"]["state"], "failed");
        assert_eq!(v["session"]["role"], "drone");
    }

    #[tokio::test]
    async fn end_to_end_socket_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("supervisor.sock");
        let orch = Arc::new(BindOrchestrator::new());
        let server = tokio::spawn({
            let sock = sock.clone();
            async move { serve(orch, &sock).await }
        });
        // Wait for the socket file to appear (bind happens inside serve()).
        for _ in 0..50 {
            if sock.exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let mut client = UnixStream::connect(&sock).await.unwrap();
        client
            .write_all(b"{\"op\":\"bind_status\"}\n")
            .await
            .unwrap();
        let mut resp = Vec::new();
        // Read until the server writes its newline-terminated reply + closes.
        let mut chunk = [0u8; 256];
        loop {
            let n = client.read(&mut chunk).await.unwrap();
            if n == 0 {
                break;
            }
            resp.extend_from_slice(&chunk[..n]);
            if resp.contains(&b'\n') {
                break;
            }
        }
        let v: Value = serde_json::from_slice(resp.split(|&b| b == b'\n').next().unwrap()).unwrap();
        assert_eq!(v["ok"], true);
        assert!(v["session"].is_null());
        server.abort();
    }
}
