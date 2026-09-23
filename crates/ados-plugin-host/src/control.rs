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
//! Trust boundary: the socket is NOT in the per-plugin socket dir. Every plugin
//! unit can write `/run/ados/plugins`, and a plugin that reached this socket
//! could rewrite another plugin's config or run another plugin's tools with that
//! plugin's grants. It lives in its own directory, created `0700` by the daemon
//! (root), and the listener re-checks each peer's kernel credentials (root or the
//! operator group only). The off-box auth lives at the `ados-control` HTTP edge
//! (the LAN pairing key when paired), exactly like `POST /api/vision/designate`.
//! The wire is the same length-prefixed msgpack [`Envelope`] every other agent
//! IPC socket speaks, so no new framing.

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

use crate::invoke::{InvokeRegistry, DEFAULT_INVOKE_TIMEOUT};

/// The directory the control socket lives in. Root-only (`0700`) and outside the
/// per-plugin socket dir, so no plugin process can reach it.
pub const DEFAULT_CONTROL_DIR: &str = "/run/ados/plugin-host";

/// The control socket file name under the control dir.
pub const CONTROL_SOCKET_NAME: &str = "_control.sock";

/// How long a caller has to deliver its request frame. A peer that connects and
/// then stalls is dropped instead of pinning a task for the daemon's lifetime.
const REQUEST_READ_DEADLINE: Duration = Duration::from_secs(5);

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
    /// concern (the real store flushes its 0600 JSON file on every set).
    fn apply_config_set(
        &self,
        plugin_id: &str,
        key: &str,
        value: Value,
        scope: &str,
    ) -> Result<String, String>;

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
}

fn arg<'a>(args: &'a Value, key: &str) -> Option<&'a Value> {
    match args {
        Value::Map(m) => m
            .iter()
            .find(|(k, _)| k.as_str() == Some(key))
            .map(|(_, v)| v),
        _ => None,
    }
}

fn arg_str<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    arg(args, key).and_then(|v| v.as_str())
}

fn ok_response(request_id: &str, scope: &str) -> Envelope {
    Envelope {
        version: PROTOCOL_VERSION,
        kind: "response".to_string(),
        method: METHOD_CONFIG_SET.to_string(),
        capability: String::new(),
        args: Value::Map(vec![
            (Value::from("set"), Value::Boolean(true)),
            (Value::from("scope"), Value::from(scope)),
        ]),
        request_id: request_id.to_string(),
        token: String::new(),
        error: None,
    }
}

fn err_response(request_id: &str, message: String) -> Envelope {
    Envelope {
        version: PROTOCOL_VERSION,
        kind: "response".to_string(),
        method: METHOD_CONFIG_SET.to_string(),
        capability: String::new(),
        args: Value::Map(vec![]),
        request_id: request_id.to_string(),
        token: String::new(),
        error: Some(message),
    }
}

/// Answer a `config.get` request with the plugin's effective config.
fn handle_config_get<H: ConfigControl>(host: &H, req: &Envelope) -> Envelope {
    let method = METHOD_CONFIG_GET;
    let Some(plugin_id) = arg_str(&req.args, "plugin_id").filter(|s| !s.is_empty()) else {
        return lifecycle_err(
            &req.request_id,
            method,
            "plugin_id must be a non-empty string".into(),
        );
    };
    match host.config_snapshot(plugin_id) {
        Ok(values) => Envelope {
            version: PROTOCOL_VERSION,
            kind: "response".to_string(),
            method: method.to_string(),
            capability: String::new(),
            args: Value::Map(vec![(Value::from("values"), values)]),
            request_id: req.request_id.clone(),
            token: String::new(),
            error: None,
        },
        Err(e) => lifecycle_err(&req.request_id, method, e),
    }
}

/// Handle one decoded control request against the host. Pure of I/O so it unit
/// tests directly.
fn handle_request<H: ConfigControl>(host: &H, req: &Envelope) -> Envelope {
    if req.method == METHOD_CONFIG_GET {
        return handle_config_get(host, req);
    }
    if req.method != METHOD_CONFIG_SET {
        return err_response(
            &req.request_id,
            format!("unknown control method: {}", req.method),
        );
    }
    let Some(plugin_id) = arg_str(&req.args, "plugin_id").filter(|s| !s.is_empty()) else {
        return err_response(
            &req.request_id,
            "plugin_id must be a non-empty string".into(),
        );
    };
    let Some(key) = arg_str(&req.args, "key").filter(|s| !s.is_empty()) else {
        return err_response(&req.request_id, "key must be a non-empty string".into());
    };
    let Some(value) = arg(&req.args, "value").cloned() else {
        return err_response(&req.request_id, "value missing".into());
    };
    // Scope defaults to drone (the per-drone namespace a skill toggle lives in);
    // an absent or empty scope is the common case.
    let scope = arg_str(&req.args, "scope")
        .filter(|s| !s.is_empty())
        .unwrap_or("drone");
    match host.apply_config_set(plugin_id, key, value, scope) {
        Ok(effective) => ok_response(&req.request_id, &effective),
        Err(e) => err_response(&req.request_id, e),
    }
}

fn tool_ok(request_id: &str, result: Value) -> Envelope {
    Envelope {
        version: PROTOCOL_VERSION,
        kind: "response".to_string(),
        method: METHOD_TOOL_INVOKE.to_string(),
        capability: String::new(),
        args: result,
        request_id: request_id.to_string(),
        token: String::new(),
        error: None,
    }
}

fn tool_err(request_id: &str, message: String) -> Envelope {
    Envelope {
        version: PROTOCOL_VERSION,
        kind: "response".to_string(),
        method: METHOD_TOOL_INVOKE.to_string(),
        capability: String::new(),
        args: Value::Map(vec![]),
        request_id: request_id.to_string(),
        token: String::new(),
        error: Some(message),
    }
}

/// Handle one `tool.invoke` control request by routing it to the live plugin
/// connection via the invoke registry. Async because it awaits the plugin's
/// reply. `arguments` defaults to an empty map; `timeout_ms` to the registry
/// default. A not-connected / slow / erroring plugin yields an error response,
/// never a hang.
async fn handle_tool_invoke(invoke: &InvokeRegistry, req: &Envelope) -> Envelope {
    let Some(plugin_id) = arg_str(&req.args, "plugin_id").filter(|s| !s.is_empty()) else {
        return tool_err(
            &req.request_id,
            "plugin_id must be a non-empty string".into(),
        );
    };
    let Some(tool) = arg_str(&req.args, "tool").filter(|s| !s.is_empty()) else {
        return tool_err(&req.request_id, "tool must be a non-empty string".into());
    };
    let arguments = arg(&req.args, "arguments")
        .cloned()
        .unwrap_or(Value::Map(vec![]));
    let timeout = arg(&req.args, "timeout_ms")
        .and_then(|v| v.as_u64())
        .map(Duration::from_millis)
        .unwrap_or(DEFAULT_INVOKE_TIMEOUT);
    match invoke.invoke(plugin_id, tool, arguments, timeout).await {
        Ok(result) => tool_ok(&req.request_id, result),
        Err(e) => tool_err(&req.request_id, e),
    }
}

/// Handle a lifecycle control request (`token.rotate` / `plugin.reconcile`).
///
/// A daemon with no reconciler wired answers with an explicit error naming the
/// gap, never a bare success: the caller is a lifecycle controller that has
/// just told an operator a revoke took effect, so a false ack here is the exact
/// failure the control socket exists to close.
fn handle_lifecycle(lifecycle: Option<&Arc<dyn LifecycleControl>>, req: &Envelope) -> Envelope {
    let Some(lifecycle) = lifecycle else {
        return lifecycle_err(
            &req.request_id,
            &req.method,
            "plugin host has no lifecycle reconciler wired; the change applies \
             when it next reads state"
                .into(),
        );
    };
    if req.method == METHOD_PLUGIN_RECONCILE {
        let (started, stopped, serving) = lifecycle.reconcile();
        return Envelope {
            version: PROTOCOL_VERSION,
            kind: "response".to_string(),
            method: req.method.clone(),
            capability: String::new(),
            args: Value::Map(vec![
                (Value::from("started"), Value::from(started as u64)),
                (Value::from("stopped"), Value::from(stopped as u64)),
                (Value::from("serving"), Value::from(serving as u64)),
            ]),
            request_id: req.request_id.clone(),
            token: String::new(),
            error: None,
        };
    }
    let Some(plugin_id) = arg_str(&req.args, "plugin_id").filter(|s| !s.is_empty()) else {
        return lifecycle_err(
            &req.request_id,
            &req.method,
            "plugin_id must be a non-empty string".into(),
        );
    };
    match lifecycle.rotate_token(plugin_id) {
        Ok(pushed) => Envelope {
            version: PROTOCOL_VERSION,
            kind: "response".to_string(),
            method: req.method.clone(),
            capability: String::new(),
            args: Value::Map(vec![
                (Value::from("rotated"), Value::Boolean(true)),
                // False means the token was written but no session was open to
                // receive it, which is correct for an enabled plugin that has
                // not connected yet. The caller reports the difference rather
                // than claiming the live plugin was updated.
                (Value::from("delivered"), Value::Boolean(pushed)),
            ]),
            request_id: req.request_id.clone(),
            token: String::new(),
            error: None,
        },
        Err(e) => lifecycle_err(&req.request_id, &req.method, e),
    }
}

fn lifecycle_err(request_id: &str, method: &str, message: String) -> Envelope {
    Envelope {
        version: PROTOCOL_VERSION,
        kind: "response".to_string(),
        method: method.to_string(),
        capability: String::new(),
        args: Value::Map(vec![]),
        request_id: request_id.to_string(),
        token: String::new(),
        error: Some(message),
    }
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
        Ok(req) if req.method == METHOD_TOKEN_ROTATE || req.method == METHOD_PLUGIN_RECONCILE => {
            handle_lifecycle(lifecycle.as_ref(), &req)
        }
        Ok(req) => handle_request(host.as_ref(), &req),
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

    let task = tokio::spawn(async move {
        loop {
            let stream = match listener.accept().await {
                Ok((s, _addr)) => s,
                Err(_) => break,
            };
            let host = host.clone();
            let invoke = invoke.clone();
            let lifecycle = lifecycle.clone();
            tokio::spawn(async move {
                serve_connection(host, invoke, lifecycle, stream).await;
            });
        }
    });
    Ok((path, task))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// A stub host recording the last applied write, with a configurable result.
    #[derive(Default)]
    struct StubHost {
        last: Mutex<Option<(String, String, Value, String)>>,
        fail: Option<String>,
    }

    impl ConfigControl for StubHost {
        fn apply_config_set(
            &self,
            plugin_id: &str,
            key: &str,
            value: Value,
            scope: &str,
        ) -> Result<String, String> {
            if let Some(err) = &self.fail {
                return Err(err.clone());
            }
            *self.last.lock().unwrap() = Some((
                plugin_id.to_string(),
                key.to_string(),
                value,
                scope.to_string(),
            ));
            // drone with an empty agent degrades to global in the real store;
            // the stub just echoes the requested scope.
            Ok(scope.to_string())
        }

        fn config_snapshot(&self, plugin_id: &str) -> Result<Value, String> {
            if let Some(err) = &self.fail {
                return Err(err.clone());
            }
            let last = self.last.lock().unwrap().clone();
            Ok(Value::Map(match last {
                Some((p, k, v, _)) if p == plugin_id => vec![(Value::from(k), v)],
                _ => vec![],
            }))
        }
    }

    fn request(args: Vec<(Value, Value)>) -> Envelope {
        Envelope {
            version: PROTOCOL_VERSION,
            kind: "request".to_string(),
            method: METHOD_CONFIG_SET.to_string(),
            capability: String::new(),
            args: Value::Map(args),
            request_id: "ctl-test".to_string(),
            token: String::new(),
            error: None,
        }
    }

    #[test]
    fn applies_a_config_write_and_echoes_scope() {
        let host = StubHost::default();
        let req = request(vec![
            (
                Value::from("plugin_id"),
                Value::from("com.altnautica.follow-me"),
            ),
            (Value::from("key"), Value::from("active")),
            (Value::from("value"), Value::Boolean(true)),
            (Value::from("scope"), Value::from("drone")),
        ]);
        let resp = handle_request(&host, &req);
        assert_eq!(resp.error, None);
        let last = host.last.lock().unwrap().clone().expect("a write");
        assert_eq!(last.0, "com.altnautica.follow-me");
        assert_eq!(last.1, "active");
        assert_eq!(last.2, Value::Boolean(true));
        assert_eq!(last.3, "drone");
    }

    #[test]
    fn defaults_scope_to_drone_when_absent() {
        let host = StubHost::default();
        let req = request(vec![
            (Value::from("plugin_id"), Value::from("p")),
            (Value::from("key"), Value::from("k")),
            (Value::from("value"), Value::from(3)),
        ]);
        let resp = handle_request(&host, &req);
        assert_eq!(resp.error, None);
        assert_eq!(host.last.lock().unwrap().clone().unwrap().3, "drone");
    }

    #[test]
    fn rejects_a_missing_plugin_id() {
        let host = StubHost::default();
        let req = request(vec![
            (Value::from("key"), Value::from("k")),
            (Value::from("value"), Value::from(1)),
        ]);
        let resp = handle_request(&host, &req);
        assert!(resp.error.unwrap().contains("plugin_id"));
        assert!(host.last.lock().unwrap().is_none());
    }

    #[test]
    fn rejects_a_missing_value() {
        let host = StubHost::default();
        let req = request(vec![
            (Value::from("plugin_id"), Value::from("p")),
            (Value::from("key"), Value::from("k")),
        ]);
        let resp = handle_request(&host, &req);
        assert!(resp.error.unwrap().contains("value"));
    }

    #[test]
    fn surfaces_a_host_error() {
        let host = StubHost {
            fail: Some("scope must be drone or global, got nonsense".to_string()),
            ..StubHost::default()
        };
        let req = request(vec![
            (Value::from("plugin_id"), Value::from("p")),
            (Value::from("key"), Value::from("k")),
            (Value::from("value"), Value::from(1)),
            (Value::from("scope"), Value::from("nonsense")),
        ]);
        let resp = handle_request(&host, &req);
        assert!(resp.error.unwrap().contains("scope must be"));
    }

    #[test]
    fn rejects_an_unknown_method() {
        let host = StubHost::default();
        let mut req = request(vec![]);
        req.method = "config.delete".to_string();
        let resp = handle_request(&host, &req);
        assert!(resp.error.unwrap().contains("unknown control method"));
    }

    /// End-to-end over a real bound socket: a client envelope round-trips and the
    /// write lands on the host.
    #[tokio::test]
    async fn round_trips_over_a_bound_socket() {
        let dir = tempfile::tempdir().unwrap();
        let host = Arc::new(StubHost::default());
        let invoke = Arc::new(InvokeRegistry::new());
        let (path, task) =
            serve_control(host.clone(), invoke, None, dir.path().to_path_buf()).unwrap();

        let req = request(vec![
            (Value::from("plugin_id"), Value::from("p")),
            (Value::from("key"), Value::from("active")),
            (Value::from("value"), Value::Boolean(true)),
        ]);
        let mut stream = UnixStream::connect(&path).await.unwrap();
        stream
            .write_all(&req.encode_frame().unwrap())
            .await
            .unwrap();
        stream.flush().await.unwrap();

        let mut header = [0u8; HEADER_SIZE];
        stream.read_exact(&mut header).await.unwrap();
        let len = decode_len(header, PLUGIN_MAX_FRAME, false).unwrap();
        let mut body = vec![0u8; len];
        stream.read_exact(&mut body).await.unwrap();
        let resp = Envelope::from_msgpack(&body).unwrap();
        assert_eq!(resp.error, None);
        assert_eq!(host.last.lock().unwrap().clone().unwrap().1, "active");

        task.abort();
    }

    /// The control socket is reachable by root only: it lives in its own `0700`
    /// dir outside the per-plugin socket dir the plugin units can write, the
    /// socket itself is `0600`, and the accept policy refuses a plugin process.
    #[tokio::test]
    async fn the_control_socket_is_root_only_and_refuses_a_plugin_peer() {
        let dir = tempfile::tempdir().unwrap();
        let control_dir = dir.path().join("plugin-host");
        let host = Arc::new(StubHost::default());
        let invoke = Arc::new(InvokeRegistry::new());
        let (path, task) = serve_control(host, invoke, None, control_dir.clone()).unwrap();

        assert_eq!(path.parent(), Some(control_dir.as_path()));
        let dir_mode = std::fs::metadata(&control_dir)
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(dir_mode & 0o777, 0o700, "control dir must be owner-only");
        let sock_mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(
            sock_mode & 0o777,
            0o600,
            "control socket must be owner-only"
        );
        assert!(
            !Path::new(DEFAULT_CONTROL_DIR).starts_with(crate::server::DEFAULT_SOCKET_DIR),
            "the control dir must not sit under the per-plugin socket dir"
        );

        // A plugin process: uid 1000 with only its own group, against a root
        // daemon on a host that has the operator group.
        assert!(!ados_protocol::ipc::operator_peer_allowed(
            1000,
            &[1000],
            0,
            Some(990)
        ));
        // An operator-group member is admitted by the same policy.
        assert!(ados_protocol::ipc::operator_peer_allowed(
            1000,
            &[1000, 990],
            0,
            Some(990)
        ));

        task.abort();
    }

    fn tool_request(plugin_id: &str, tool: &str) -> Envelope {
        Envelope {
            version: PROTOCOL_VERSION,
            kind: "request".to_string(),
            method: METHOD_TOOL_INVOKE.to_string(),
            capability: String::new(),
            args: Value::Map(vec![
                (Value::from("plugin_id"), Value::from(plugin_id)),
                (Value::from("tool"), Value::from(tool)),
                (Value::from("arguments"), Value::Map(vec![])),
            ]),
            request_id: "ctl-inv".to_string(),
            token: String::new(),
            error: None,
        }
    }

    #[tokio::test]
    async fn tool_invoke_against_no_connection_errors() {
        let invoke = InvokeRegistry::new();
        let resp = handle_tool_invoke(&invoke, &tool_request("com.x.p", "t")).await;
        assert_eq!(resp.method, METHOD_TOOL_INVOKE);
        assert!(resp.error.unwrap().contains("plugin_not_running"));
    }

    #[tokio::test]
    async fn tool_invoke_routes_to_a_registered_connection() {
        let invoke = InvokeRegistry::new();
        let (tx, mut rx) = tokio::sync::mpsc::channel::<crate::invoke::InvokeRequest>(4);
        invoke.register("com.x.p", tx);
        // A fake connection replies with the tool name echoed.
        let responder = tokio::spawn(async move {
            let req = rx.recv().await.unwrap();
            let _ = req.reply.send(Ok(Value::Map(vec![(
                Value::from("ran"),
                Value::from(req.tool),
            )])));
        });
        let resp = handle_tool_invoke(&invoke, &tool_request("com.x.p", "greet")).await;
        assert_eq!(resp.error, None);
        assert_eq!(
            resp.args,
            Value::Map(vec![(Value::from("ran"), Value::from("greet"))])
        );
        responder.await.unwrap();
    }

    #[tokio::test]
    async fn tool_invoke_rejects_a_missing_tool_name() {
        let invoke = InvokeRegistry::new();
        let mut req = tool_request("com.x.p", "t");
        // Drop the tool arg.
        req.args = Value::Map(vec![(Value::from("plugin_id"), Value::from("com.x.p"))]);
        let resp = handle_tool_invoke(&invoke, &req).await;
        assert!(resp.error.unwrap().contains("tool must be"));
    }
}
