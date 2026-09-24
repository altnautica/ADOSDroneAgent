//! Supervisor control socket — the cross-process trigger seam for the work that
//! lives in this process: the bind FSM and the ground-station role transition.
//!
//! The bind orchestrator and the role transition run in this (supervisor)
//! process, but they are triggered from the REST front, the FastAPI routes and
//! the cloud auto-pair supervisor, which run in OTHER processes. They reach it
//! over a Unix socket at [`SUPERVISOR_SOCK_NAME`] under the run dir, speaking one newline-JSON request →
//! newline-JSON response per connection:
//!   - `{"op":"start_bind","role":"drone","peer_device_id":null,"source":"operator",
//!      "fleet_id":1,"fleet_slot":3}`
//!     → blocks for the whole rendezvous → `{"ok":true,"session":{…to_json…}}`
//!     or `{"ok":false,"error":"E_BIND_IN_PROGRESS"}` when one already runs.
//!   - `{"op":"bind_status"}` → `{"ok":true,"session":{…}|null}`.
//!   - `{"op":"cancel_bind"}` → aborts the in-flight session → `{"ok":true}`.
//!   - `{"op":"set_role","role":"relay","reason":"rest"}` → blocks for the
//!     transition → `{"ok":true,"role","previous","units_started",
//!     "units_stopped","ts_ms","noop"}`, or `{"ok":false,"error":…}` for a
//!     refused one (`E_INVALID_ROLE`, `E_PROFILE_MISMATCH`, `E_BIND_IN_PROGRESS`).
//!
//! `cancel_bind` arrives on a SEPARATE connection from the blocked `start_bind`,
//! so it routes through [`BindOrchestrator::cancel_current`] (a notify), not the
//! per-call cancel future. The caller (FastAPI) applies its own wall-clock
//! timeout and fires `cancel_bind` on timeout, matching the Python route's
//! `wait_for` + per-request cancel_event.
//!
//! `set_role` is handed to the supervisor loop, which owns the service table,
//! and runs there to completion: a requester that disconnects, or that is killed
//! because it ran inside a unit the transition stopped, does not cut it short.

use std::path::Path;
use std::sync::Arc;

use tokio::sync::{mpsc, oneshot};

use serde::Deserialize;
use serde_json::{json, Value};

use super::keys::FleetIdentity;
use super::orchestrator::{BindOrchestrator, BindStartError};
use super::BindRole;
use crate::role::RoleRequest;

/// The supervisor control socket's file name under the run dir (sibling to
/// mavlink.sock / state.sock): `/run/ados/supervisor.sock` on a root install.
pub const SUPERVISOR_SOCK_NAME: &str = "supervisor.sock";

/// Cap on a single request line so a malformed client can't grow the buffer.
const MAX_REQUEST_BYTES: usize = 64 * 1024;

/// Fixed wait between attempts to bind a command socket that failed to bind.
pub const SOCKET_BIND_RETRY: std::time::Duration = std::time::Duration::from_secs(3);

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
    /// Why a `set_role` was requested (`rest`, `factory_reset`), recorded on the
    /// `role_changed` event.
    #[serde(default)]
    reason: Option<String>,
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

/// Bind the control socket and serve requests for the life of the process. Run
/// as its own task from the supervisor main loop. The shared helper removes a
/// stale socket first and chmods it 0660 (root-owned; the api + cloud services
/// run as root on target).
///
/// A bind that fails is retried on a fixed [`SOCKET_BIND_RETRY`] interval until
/// it succeeds: this socket is how the GCS pairing route reaches the bind FSM,
/// and a supervisor that gave up on it at boot could not be paired until
/// someone restarted it by hand.
///
/// The wire is one newline-JSON request → one newline-JSON response per
/// connection, so the shared one-shot RPC server owns the accept loop and the
/// framing; this module supplies only the parse + route via [`dispatch`]. A
/// blocking `start_bind` runs on its connection's own task, so a concurrent
/// `cancel_bind` on a separate connection is still accepted and handled.
pub async fn serve(
    orch: Arc<BindOrchestrator>,
    roles: mpsc::Sender<RoleRequest>,
    sock_path: &Path,
) {
    let listener = loop {
        match ados_protocol::ipc::bind_command_socket(sock_path, 0o660) {
            Ok(l) => break l,
            Err(e) => {
                tracing::warn!(
                    path = %sock_path.display(),
                    error = %e,
                    "supervisor control socket bind failed; retrying"
                );
                tokio::time::sleep(SOCKET_BIND_RETRY).await;
            }
        }
    };
    tracing::info!(path = %sock_path.display(), "supervisor control socket listening");
    ados_protocol::ipc::serve_rpc(listener, MAX_REQUEST_BYTES, move |req: Vec<u8>| {
        let orch = orch.clone();
        let roles = roles.clone();
        async move {
            let resp = dispatch(&req, &orch, &roles).await;
            serde_json::to_vec(&resp)
                .unwrap_or_else(|_| br#"{"ok":false,"error":"E_ENCODE"}"#.to_vec())
        }
    })
    .await;
}

/// Parse + route one request. Pure async over the orchestrator handle and the
/// supervisor loop's role channel — unit-testable without a socket.
async fn dispatch(
    line: &[u8],
    orch: &Arc<BindOrchestrator>,
    roles: &mpsc::Sender<RoleRequest>,
) -> Value {
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
        "set_role" => {
            let Some(target) = req.role.filter(|r| !r.is_empty()) else {
                return json!({"ok": false, "error": "E_MISSING_ROLE"});
            };
            let (reply, answer) = oneshot::channel();
            let request = RoleRequest {
                target,
                reason: req.reason.unwrap_or_else(|| "operator".to_string()),
                reply,
            };
            if roles.send(request).await.is_err() {
                return json!({"ok": false, "error": "E_SUPERVISOR_STOPPING"});
            }
            answer
                .await
                .unwrap_or_else(|_| json!({"ok": false, "error": "E_SUPERVISOR_STOPPING"}))
        }
        other => json!({"ok": false, "error": format!("E_UNKNOWN_OP: {other}")}),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::UnixStream;

    /// A role channel whose loop is gone, for the bind-op tests.
    fn no_roles() -> mpsc::Sender<RoleRequest> {
        mpsc::channel(1).0
    }

    #[tokio::test]
    async fn dispatch_status_when_idle_is_null_session() {
        let orch = Arc::new(BindOrchestrator::new());
        let v = dispatch(br#"{"op":"bind_status"}"#, &orch, &no_roles()).await;
        assert_eq!(v["ok"], true);
        assert!(v["session"].is_null());
    }

    #[tokio::test]
    async fn dispatch_cancel_is_ok_when_idle() {
        let orch = Arc::new(BindOrchestrator::new());
        let v = dispatch(br#"{"op":"cancel_bind"}"#, &orch, &no_roles()).await;
        assert_eq!(v["ok"], true);
    }

    #[tokio::test]
    async fn dispatch_bad_json_and_bad_op_and_bad_role() {
        let orch = Arc::new(BindOrchestrator::new());
        assert_eq!(dispatch(b"not json", &orch, &no_roles()).await["ok"], false);
        assert_eq!(
            dispatch(br#"{"op":"frob"}"#, &orch, &no_roles()).await["ok"],
            false
        );
        let bad_role = dispatch(br#"{"op":"start_bind","role":"bogus"}"#, &orch, &no_roles()).await;
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
            &no_roles(),
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

    #[tokio::test(start_paused = true)]
    async fn a_socket_that_fails_to_bind_is_retried_until_it_binds() {
        // The parent of the socket path is a regular file, so the first bind
        // fails. Once the obstacle is gone the next fixed-interval attempt must
        // bind: a supervisor that gave up here could not be paired until it was
        // restarted by hand.
        let dir = tempfile::tempdir().unwrap();
        let parent = dir.path().join("run");
        std::fs::write(&parent, b"not a directory").unwrap();
        let sock = parent.join("supervisor.sock");
        let server = tokio::spawn({
            let sock = sock.clone();
            async move { serve(Arc::new(BindOrchestrator::new()), no_roles(), &sock).await }
        });
        tokio::task::yield_now().await;
        assert!(!sock.exists());

        std::fs::remove_file(&parent).unwrap();
        std::fs::create_dir(&parent).unwrap();
        tokio::time::advance(SOCKET_BIND_RETRY + std::time::Duration::from_millis(10)).await;
        for _ in 0..50 {
            if sock.exists() {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert!(sock.exists(), "the retry must bind once the path is usable");
        server.abort();
    }

    #[tokio::test]
    async fn end_to_end_socket_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("supervisor.sock");
        let orch = Arc::new(BindOrchestrator::new());
        let server = tokio::spawn({
            let sock = sock.clone();
            async move { serve(orch, no_roles(), &sock).await }
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
