//! The on-box plugin-host control socket.
//!
//! The per-plugin sockets (`server.rs`) are capability-token gated and bound to
//! one plugin's identity, so a plugin can only read/write its OWN config. But a
//! GCS skill toggle (or a per-drone settings change) needs to flip a plugin's
//! per-drone config without being that plugin — the write originates off-box at
//! the operator, lands at the native control surface (`ados-control`), and must
//! reach the LIVE in-memory [`crate::realhost::ConfigStore`] in this running
//! daemon (a disk write alone is not seen until restart). This module is that
//! reach: a single daemon-lifetime Unix socket at
//! `/run/ados/plugin-host/_control.sock` that applies an on-box config write to
//! the live store and persists it.
//!
//! Trust boundary: the socket is NOT in the per-plugin socket dir. A plugin that
//! reached this socket could rewrite another plugin's config or run another
//! plugin's tools with that plugin's grants, so it sits outside every plugin's
//! mount namespace. It lives in its own directory, created `0700` by the daemon
//! (root), and the listener re-checks each peer's kernel credentials (root or the
//! operator group only). The off-box auth lives at the `ados-control` HTTP edge
//! (the LAN pairing key when paired), exactly like `POST /api/vision/designate`.
//! The wire is the same length-prefixed msgpack [`Envelope`] every other agent
//! IPC socket speaks, so no new framing.

use std::future::Future;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use ados_protocol::frame::{decode_len, HEADER_SIZE, PLUGIN_MAX_FRAME};
use ados_protocol::plugin::{Envelope, PROTOCOL_VERSION};
use rmpv::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::task::JoinHandle;

use crate::args::{arg_str, map_get};
use crate::invoke::{InvokeRegistry, DEFAULT_INVOKE_TIMEOUT};

mod handlers;

use handlers::{err_response, handle_lifecycle, handle_request, handle_tool_invoke};

/// The directory the control socket lives in. Root-only (`0700`) and outside the
/// per-plugin socket dir, so no plugin process can reach it.
pub const DEFAULT_CONTROL_DIR: &str = "/run/ados/plugin-host";

/// The control socket file name under the control dir.
pub const CONTROL_SOCKET_NAME: &str = "_control.sock";

/// How long a caller has to deliver its request frame. A peer that connects and
/// then stalls is dropped instead of pinning a task for the daemon's lifetime.
const REQUEST_READ_DEADLINE: Duration = Duration::from_secs(5);

/// Most control connections served at once. The callers are the agent's own
/// routes and the reconciler, one short request each; the cap bounds the tasks
/// and fds a stuck caller can hold.
const CONTROL_MAX_CONNECTIONS: usize = 32;

/// The control method that applies a per-plugin config write to the live store.
pub const METHOD_CONFIG_SET: &str = "config.set";

/// The control method that reads a plugin's live config as the plugin itself
/// sees it on this drone (global keys with the drone's own keys over them), so
/// a GCS can show a plugin's settings after a reload. Args: `{plugin_id}`;
/// answers `{values: {key: value, ...}}`.
pub const METHOD_CONFIG_GET: &str = "config.get";

/// The control method that runs one of a plugin's declared MCP tools on its live
/// connection and returns the result. The off-box authorization is the
/// `ados-control` HTTP edge (the MCP-token scope gate); by the time it reaches
/// this socket it is an on-box trusted caller, and the plugin host gates the
/// send on the plugin's own token carrying `mcp.expose`.
pub const METHOD_TOOL_INVOKE: &str = "tool.invoke";

/// The control method that re-mints one plugin's capability token from the
/// current grant set and pushes it into the plugin's live session.
///
/// This is how a grant or a revoke becomes effective without restarting
/// anything: the lifecycle controller writes state, then pokes this, and the
/// plugin's next request is gated against the new set. Args: `{plugin_id}`.
pub const METHOD_TOKEN_ROTATE: &str = "token.rotate";

/// The control method that re-reads plugin state and brings the served sockets
/// in line with it.
///
/// This is how a freshly enabled plugin gets a socket and a token without a
/// daemon restart. The controller calls it BEFORE starting the plugin unit, so
/// the runner finds both already in place. Args: none.
pub const METHOD_PLUGIN_RECONCILE: &str = "plugin.reconcile";

/// The control method that deletes a removed plugin's data dir. The lifecycle
/// controller that removed the plugin may lack the privileges to enter a tree
/// the plugin's own user owns; the plugin host has them. Refused while the
/// plugin is still installed. Args: `{plugin_id}`.
pub const METHOD_PLUGIN_PURGE_DATA: &str = "plugin.purge_data";

/// The control method that creates (`op: ensure`) or removes (`op: remove`) a
/// plugin's system user. Only the plugin host's unit may write the account
/// database; every other lifecycle controller asks it here. Removal is refused
/// while the plugin is still installed. Args: `{plugin_id, op}`.
pub const METHOD_PLUGIN_ACCOUNT: &str = "plugin.account";

/// The control socket path under a control dir.
pub fn control_socket_path(control_dir: &Path) -> PathBuf {
    control_dir.join(CONTROL_SOCKET_NAME)
}

/// The host capability the control socket drives: a config write into the live
/// store, resolving the per-drone scope the same way a plugin's own `config.set`
/// does. Implemented by [`crate::realhost::RealHost`]; a trait keeps this module
/// testable against a stub without a full host.
pub trait ConfigControl: Send + Sync {
    /// Apply a config write. Returns the effective scope (`drone`/`global`) on
    /// success, or a human error string. Persistence is the implementation's
    /// concern (the real store flushes its 0600 JSON file on every set), which
    /// is why the write is async.
    fn apply_config_set(
        &self,
        plugin_id: &str,
        key: &str,
        value: Value,
        scope: &str,
    ) -> impl Future<Output = Result<String, String>> + Send;

    /// The plugin's effective per-drone config as a map, or a human error.
    fn config_snapshot(&self, plugin_id: &str) -> Result<Value, String>;
}

/// The lifecycle half of the control surface: the two operations that keep a
/// live daemon equal to what a lifecycle controller just wrote.
///
/// Implemented by [`crate::reconcile::PluginReconciler`]. A trait keeps this
/// module testable against a stub, and keeps the control socket usable in a
/// daemon that has no reconciler wired (the methods then answer with an
/// explicit "not wired" error rather than silently succeeding).
pub trait LifecycleControl: Send + Sync {
    /// Re-mint `plugin_id`'s token. `Ok(true)` when a live session received it.
    fn rotate_token(&self, plugin_id: &str) -> Result<bool, String>;
    /// Reconcile served sockets against state. Returns `(started, stopped, serving)`.
    fn reconcile(&self) -> (usize, usize, usize);
    /// Delete a removed plugin's data dir.
    fn purge_data(&self, plugin_id: &str) -> Result<(), String>;
    /// Create (`ensure`) or remove a plugin's system user.
    fn account(&self, plugin_id: &str, ensure: bool) -> Result<(), String>;
}

async fn serve_connection<H: ConfigControl>(
    host: Arc<H>,
    invoke: Arc<InvokeRegistry>,
    lifecycle: Option<Arc<dyn LifecycleControl>>,
    mut stream: UnixStream,
) {
    // One request/response per connection (the client opens fresh per call,
    // matching the vision IPC client). A read/decode failure, or a request that
    // does not arrive within the deadline, just drops the connection.
    let body = match tokio::time::timeout(REQUEST_READ_DEADLINE, read_request(&mut stream)).await {
        Ok(Ok(Some(body))) => body,
        _ => return,
    };
    let resp = match Envelope::from_msgpack(&body) {
        Ok(req) if req.method == METHOD_TOOL_INVOKE => {
            handle_tool_invoke(invoke.as_ref(), &req).await
        }
        Ok(req)
            if req.method == METHOD_TOKEN_ROTATE
                || req.method == METHOD_PLUGIN_RECONCILE
                || req.method == METHOD_PLUGIN_PURGE_DATA
                || req.method == METHOD_PLUGIN_ACCOUNT =>
        {
            handle_lifecycle(lifecycle.as_ref(), &req)
        }
        Ok(req) => handle_request(host.as_ref(), &req).await,
        Err(e) => err_response("", format!("decode control request: {e}")),
    };
    if let Ok(frame) = resp.encode_frame() {
        let _ = stream.write_all(&frame).await;
        let _ = stream.flush().await;
    }
}

/// Read one length-prefixed request frame. `None` on a bad length or a short
/// read.
async fn read_request(stream: &mut UnixStream) -> std::io::Result<Option<Vec<u8>>> {
    let mut header = [0u8; HEADER_SIZE];
    stream.read_exact(&mut header).await?;
    let Ok(len) = decode_len(header, PLUGIN_MAX_FRAME, true) else {
        return Ok(None);
    };
    let mut body = vec![0u8; len];
    stream.read_exact(&mut body).await?;
    Ok(Some(body))
}

/// Create the control dir and lock it to its owner (`0700`). The daemon runs as
/// root, so nothing but root can traverse to the socket inside it. A dir this
/// process cannot lock down is an error, never a silently wider socket.
fn prepare_control_dir(control_dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(control_dir)?;
    std::fs::set_permissions(control_dir, std::fs::Permissions::from_mode(0o700))
}

/// Bind the control socket and spawn its accept loop. The control dir is
/// created `0700`, then the shared command-plane helper unlinks a stale socket,
/// binds it `0600`, and admits only root and operator-group peers on accept.
/// Returns the bound path and the accept-task handle so the daemon can unlink +
/// abort on shutdown.
///
/// `lifecycle` is the reconciler the `token.rotate` / `plugin.reconcile`
/// methods act through. `None` is a daemon with no reconciler, where those two
/// methods answer with an explicit error.
pub fn serve_control<H: ConfigControl + 'static>(
    host: Arc<H>,
    invoke: Arc<InvokeRegistry>,
    lifecycle: Option<Arc<dyn LifecycleControl>>,
    control_dir: PathBuf,
) -> std::io::Result<(PathBuf, JoinHandle<()>)> {
    prepare_control_dir(&control_dir)?;
    let path = control_socket_path(&control_dir);
    let listener = ados_protocol::ipc::bind_command_socket(&path, 0o600)?;

    let permits = Arc::new(tokio::sync::Semaphore::new(CONTROL_MAX_CONNECTIONS));
    let task = tokio::spawn(async move {
        loop {
            let stream = match listener.accept().await {
                Ok((s, _addr)) => s,
                Err(e) => {
                    // A transient accept error (fd pressure) must not end the
                    // control surface for the daemon's lifetime.
                    tracing::warn!(error = %e, "control socket accept failed");
                    tokio::time::sleep(crate::server::ACCEPT_RETRY_INTERVAL).await;
                    continue;
                }
            };
            // Past the cap the connection is dropped unanswered; the callers
            // treat an unanswered control call as unreachable and retry.
            let Ok(permit) = permits.clone().try_acquire_owned() else {
                tracing::warn!("control socket at its connection cap; dropping a connection");
                continue;
            };
            let host = host.clone();
            let invoke = invoke.clone();
            let lifecycle = lifecycle.clone();
            tokio::spawn(async move {
                serve_connection(host, invoke, lifecycle, stream).await;
                drop(permit);
            });
        }
    });
    Ok((path, task))
}

#[cfg(test)]
mod tests;
