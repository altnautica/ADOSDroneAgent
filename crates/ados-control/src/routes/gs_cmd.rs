//! The ground-station data-plane command-socket client.
//!
//! The mesh-write and WFB-pair routes have no in-process pair manager to call
//! from the front; the data-plane service (`ados-groundlink`) owns the gateway
//! preference and the WFB rx-key install/unpair, and exposes them on a Unix
//! command socket. This module is the small client both route modules forward
//! through, over the shared [`crate::ipc::cmd`] exchange. (Role changes go to the
//! supervisor instead; see `gs_mesh_write`.)
//!
//! The reply is returned raw (with its transport `ok` flag intact); each route
//! maps `ok:true`/`ok:false` to its own response shape. An unreachable socket / a
//! read error / a closed-before-reply / an unparseable reply / no reply within
//! the deadline all yield `None` so the caller can take its no-link posture (a
//! 503 — the front cannot drive the systemd/`batctl` work itself).

use std::path::PathBuf;
use std::time::Duration;

use serde_json::Value;

/// A key install restarts the receive unit before the op replies, so the
/// deadline covers a systemd restart rather than an in-memory answer.
const GROUNDLINK_CMD_TIMEOUT: Duration = Duration::from_secs(30);

/// The data-plane command socket (`/run/ados/groundlink-cmd.sock`), honouring
/// `ADOS_RUN_DIR` (the same override the sibling sockets + sidecars resolve under).
fn groundlink_cmd_sock() -> PathBuf {
    PathBuf::from(std::env::var("ADOS_RUN_DIR").unwrap_or_else(|_| "/run/ados".to_string()))
        .join("groundlink-cmd.sock")
}

/// One exchange with the data-plane command socket. Returns the parsed object
/// reply (its `ok` flag intact), or `None` when no object reply arrived within
/// [`GROUNDLINK_CMD_TIMEOUT`].
pub async fn groundlink_cmd_roundtrip(request: &Value) -> Option<Value> {
    crate::ipc::cmd::roundtrip_object(&groundlink_cmd_sock(), request, GROUNDLINK_CMD_TIMEOUT)
        .await
        .ok()
        .map(Value::Object)
}
