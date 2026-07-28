//! Supervisor control socket — the cross-process trigger seam for the bind FSM.
//!
//! The bind orchestrator lives in this (supervisor) process, but a bind is
//! triggered from the FastAPI `/wfb/pair/local-bind` route + the cloud auto-pair
//! supervisor, which run in OTHER processes. They reach the orchestrator over a
//! Unix socket at [`SUPERVISOR_SOCK`] speaking one newline-JSON request →
//! newline-JSON response per connection:
//!   - `{"op":"start_bind","role":"drone","peer_device_id":null,"source":"operator",
//!      "fleet_id":1,"fleet_slot":3}`
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

use super::keys::FleetIdentity;
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
    /// The fleet this bind joins. Optional; present only when the caller (the
    /// GCS, which just received the assignment from the ground station's pair
    /// route) has one to deliver.
    #[serde(default)]
    fleet_id: Option<u16>,
    /// The slot the ground station's `FleetRegistry` issued for this device.
    /// Slots are provisioned, never negotiated: a drone that picked its own
    /// could collide with a flying peer's `channel_id`.
    #[serde(default)]
    fleet_slot: Option<u8>,
}

/// Build the fleet assignment to persist with the key, or `None` when the
/// caller supplied no complete assignment.
///
/// BOTH halves are required: a slot without a fleet is unaddressable and a
/// fleet without a slot leaves a drone parked, so a half-filled request writes
/// nothing rather than half an identity over a working one. Pure, so the
/// partial-input rule is unit-testable without a socket.
fn fleet_from_request(fleet_id: Option<u16>, fleet_slot: Option<u8>) -> Option<FleetIdentity> {
    match (fleet_id, fleet_slot) {
        (Some(fleet_id), Some(fleet_slot)) => Some(FleetIdentity {
            fleet_id,
            fleet_slot,
        }),
        _ => None,
    }
}

/// Bind the control socket and serve requests until the listener errors. Run as
/// its own task from the supervisor main loop. Removes a stale socket first and
/// chmods it 0660 (root-owned; the api + cloud services run as root on target).
/// Returns only on a bind error; the accept loop never exits on the happy path.
///
/// The wire is one newline-JSON request → one newline-JSON response per
/// connection, so the shared one-shot RPC server owns the accept loop and the
/// framing; this module supplies only the parse + route via [`dispatch`]. A
/// blocking `start_bind` runs on its connection's own task, so a concurrent
/// `cancel_bind` on a separate connection is still accepted and handled.
pub async fn serve(orch: Arc<BindOrchestrator>, sock_path: &Path) -> std::io::Result<()> {
    // The shared helper owns the create-dir / remove-stale / bind / chmod hygiene
    // (0660, root-owned; the api + cloud services run as root on target).
    let listener = ados_protocol::ipc::bind_command_socket(sock_path, 0o660)?;
    tracing::info!(path = %sock_path.display(), "supervisor control socket listening");
    ados_protocol::ipc::serve_rpc(listener, MAX_REQUEST_BYTES, move |req: Vec<u8>| {
        let orch = orch.clone();
        async move {
            let resp = dispatch(&req, &orch).await;
            serde_json::to_vec(&resp)
                .unwrap_or_else(|_| br#"{"ok":false,"error":"E_ENCODE"}"#.to_vec())
        }
    })
    .await;
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
            let fleet = fleet_from_request(req.fleet_id, req.fleet_slot);
            match orch
                .start_local_bind(
                    role,
                    req.peer_device_id,
                    source,
                    fleet,
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
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::UnixStream;

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

    #[test]
    fn a_fleet_assignment_needs_both_halves() {
        // A slot with no fleet is unaddressable and a fleet with no slot leaves
        // a drone parked, so a half-filled request must write NOTHING rather
        // than stamp half an identity over a working one.
        assert_eq!(
            fleet_from_request(Some(2), Some(7)),
            Some(FleetIdentity {
                fleet_id: 2,
                fleet_slot: 7
            })
        );
        assert_eq!(fleet_from_request(Some(2), None), None);
        assert_eq!(fleet_from_request(None, Some(7)), None);
        assert_eq!(fleet_from_request(None, None), None);
    }

    #[test]
    fn the_start_bind_request_parses_the_fleet_assignment() {
        // The wire seam: the GCS forwards the slot the ground station's registry
        // just issued, and an older caller that omits both fields still parses.
        let with: Request = serde_json::from_slice(
            br#"{"op":"start_bind","role":"drone","fleet_id":4,"fleet_slot":9}"#,
        )
        .unwrap();
        assert_eq!(
            fleet_from_request(with.fleet_id, with.fleet_slot),
            Some(FleetIdentity {
                fleet_id: 4,
                fleet_slot: 9
            })
        );
        let without: Request =
            serde_json::from_slice(br#"{"op":"start_bind","role":"drone"}"#).unwrap();
        assert_eq!(
            fleet_from_request(without.fleet_id, without.fleet_slot),
            None
        );
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
