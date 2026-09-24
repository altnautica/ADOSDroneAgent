//! The plugin-host control socket client.
//!
//! The plugin host (`ados-plugin-host`) holds each plugin's per-drone config in
//! an in-memory store; a disk write alone is not seen until restart. So a GCS
//! skill toggle (or a per-drone settings change) for a plugin the GCS is not has
//! to reach the LIVE store in the running daemon. The daemon exposes one
//! on-box, daemon-lifetime control socket
//! (`/run/ados/plugin-host/_control.sock`, in a root-only `0700` dir outside the
//! per-plugin socket dir) for exactly this; this client is its caller.
//!
//! The wire is the same length-prefixed msgpack [`Envelope`] the vision socket
//! speaks (4-byte big-endian length + a msgpack envelope), request/response,
//! one fresh connection per call — config writes are infrequent. An absent
//! socket (the plugin host not up) surfaces as [`PluginControlError::Io`], which
//! the route maps to a 503 so a config write is never silently dropped.
//!
//! Auth: the off-box auth is the LAN pairing-key edge on `PUT
//! /api/plugins/{id}/config` (the same posture as `/api/vision/designate`); the
//! socket itself admits root only, which this service runs as.

use std::path::{Path, PathBuf};

use ados_protocol::frame::{decode_len, HEADER_SIZE, PLUGIN_MAX_FRAME};
use ados_protocol::plugin::{Envelope, PROTOCOL_VERSION};
use rmpv::Value;
use thiserror::Error;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

/// The control method that applies a per-plugin config write.
const METHOD_CONFIG_SET: &str = "config.set";
/// The control method that runs one of a plugin's declared MCP tools.
const METHOD_TOOL_INVOKE: &str = "tool.invoke";

/// Read a plugin's effective config (mirrors `ados_plugin_host::control`).
const METHOD_CONFIG_GET: &str = "config.get";
/// The default plugin-host control directory (matches `DEFAULT_CONTROL_DIR` in
/// the plugin host). Overridable via `ADOS_PLUGIN_HOST_DIR` so a test / SITL run
/// points both the daemon and this client at a tempdir.
const PLUGIN_HOST_DIR_DEFAULT: &str = "/run/ados/plugin-host";
/// The control socket file name under the control directory.
const CONTROL_SOCKET_NAME: &str = "_control.sock";

/// The default control socket path (`ADOS_PLUGIN_HOST_DIR`-aware).
pub fn default_control_socket() -> PathBuf {
    let dir =
        std::env::var("ADOS_PLUGIN_HOST_DIR").unwrap_or_else(|_| PLUGIN_HOST_DIR_DEFAULT.into());
    Path::new(&dir).join(CONTROL_SOCKET_NAME)
}

/// A plugin-config control error.
#[derive(Debug, Error)]
pub enum PluginControlError {
    /// The control socket could not be reached or the I/O failed (the plugin
    /// host is not up, or the connection broke). The route maps it to a 503.
    #[error("plugin control socket io failed: {0}")]
    Io(#[from] std::io::Error),
    /// The reply could not be framed/deframed.
    #[error("plugin control frame error: {0}")]
    Frame(String),
    /// The daemon answered with an envelope `error` (a bad request, e.g. an empty
    /// key). The route surfaces it as a 400.
    #[error("{0}")]
    Rpc(String),
    /// The daemon accepted the request but did not answer within the deadline.
    /// The route maps it to a 504.
    #[error("plugin host did not answer in time")]
    Timeout,
}

/// The deadline for a config read or write, which the host answers from memory.
const CONFIG_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// The longest tool run the front forwards; a longer request is clamped so the
/// host never works on an invocation the front has already given up on.
const MAX_TOOL_TIMEOUT_MS: u64 = 120_000;

/// The tool timeout the host applies when none is given (its
/// `DEFAULT_INVOKE_TIMEOUT`).
const DEFAULT_TOOL_TIMEOUT_MS: u64 = 5_000;

/// Headroom over the host's own tool deadline, so the front hears the host's
/// `tool_timeout` answer rather than timing out first.
const TOOL_REPLY_MARGIN_MS: u64 = 5_000;

/// Connects to the plugin-host control socket and runs a single request/response.
#[derive(Clone)]
pub struct PluginControlClient {
    socket_path: PathBuf,
}

impl PluginControlClient {
    /// Build a client for the given socket path.
    pub fn new(socket_path: PathBuf) -> Self {
        Self { socket_path }
    }

    /// Build a client at the default control socket path.
    pub fn default_socket() -> Self {
        Self::new(default_control_socket())
    }

    /// The socket path this client talks to.
    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    /// Apply a per-plugin config write through the live daemon. `value` is any
    /// msgpack value (a bool for a skill toggle, a number for a follow distance).
    /// Returns the daemon's response args (`{set, scope}`).
    pub async fn config_set(
        &self,
        plugin_id: &str,
        key: &str,
        value: Value,
        scope: Option<&str>,
    ) -> Result<Value, PluginControlError> {
        let mut args = vec![
            (Value::from("plugin_id"), Value::from(plugin_id)),
            (Value::from("key"), Value::from(key)),
            (Value::from("value"), value),
        ];
        if let Some(scope) = scope.filter(|s| !s.is_empty()) {
            args.push((Value::from("scope"), Value::from(scope)));
        }
        self.request(METHOD_CONFIG_SET, Value::Map(args), CONFIG_TIMEOUT)
            .await
    }

    /// Read a plugin's effective per-drone config from the live daemon: the map
    /// the plugin itself reads on this drone.
    pub async fn config_get(&self, plugin_id: &str) -> Result<Value, PluginControlError> {
        let args = Value::Map(vec![(Value::from("plugin_id"), Value::from(plugin_id))]);
        let resp = self
            .request(METHOD_CONFIG_GET, args, CONFIG_TIMEOUT)
            .await?;
        Ok(match resp {
            Value::Map(m) => m
                .into_iter()
                .find(|(k, _)| k.as_str() == Some("values"))
                .map(|(_, v)| v)
                .unwrap_or(Value::Map(vec![])),
            _ => Value::Map(vec![]),
        })
    }

    /// Run one of a plugin's declared MCP tools on its live connection through
    /// the daemon and return the tool's result value. `arguments` is the tool's
    /// argument value (usually a map); `timeout_ms` bounds the wait (None → the
    /// daemon default). A tool error / not-connected plugin surfaces as
    /// [`PluginControlError::Rpc`]; an unreachable daemon as
    /// [`PluginControlError::Io`], which the route maps to a 503.
    pub async fn tool_invoke(
        &self,
        plugin_id: &str,
        tool: &str,
        arguments: Value,
        timeout_ms: Option<u64>,
    ) -> Result<Value, PluginControlError> {
        let mut args = vec![
            (Value::from("plugin_id"), Value::from(plugin_id)),
            (Value::from("tool"), Value::from(tool)),
            (Value::from("arguments"), arguments),
        ];
        let tool_ms = timeout_ms
            .unwrap_or(DEFAULT_TOOL_TIMEOUT_MS)
            .min(MAX_TOOL_TIMEOUT_MS);
        args.push((Value::from("timeout_ms"), Value::from(tool_ms)));
        let bound = std::time::Duration::from_millis(tool_ms + TOOL_REPLY_MARGIN_MS);
        self.request(METHOD_TOOL_INVOKE, Value::Map(args), bound)
            .await
    }

    /// One fresh-connection request/response against the control socket,
    /// bounded end to end by `bound`.
    async fn request(
        &self,
        method: &str,
        args: Value,
        bound: std::time::Duration,
    ) -> Result<Value, PluginControlError> {
        let env = Envelope {
            version: PROTOCOL_VERSION,
            kind: "request".to_string(),
            method: method.to_string(),
            capability: String::new(),
            args,
            request_id: "ctl-plugin-config".to_string(),
            token: String::new(),
            error: None,
        };
        let frame = env
            .encode_frame()
            .map_err(|e| PluginControlError::Frame(format!("encode envelope: {e}")))?;

        let exchange = async {
            let mut stream = UnixStream::connect(&self.socket_path).await?;
            stream.write_all(&frame).await?;
            stream.flush().await?;

            let mut header = [0u8; HEADER_SIZE];
            stream.read_exact(&mut header).await?;
            let len = decode_len(header, PLUGIN_MAX_FRAME, false)
                .map_err(|e| PluginControlError::Frame(format!("response length: {e}")))?;
            let mut body = vec![0u8; len];
            stream.read_exact(&mut body).await?;
            Ok::<_, PluginControlError>(body)
        };
        let body = tokio::time::timeout(bound, exchange)
            .await
            .map_err(|_| PluginControlError::Timeout)??;
        let resp = Envelope::from_msgpack(&body)
            .map_err(|e| PluginControlError::Frame(format!("decode envelope: {e}")))?;
        if let Some(err) = resp.error {
            return Err(PluginControlError::Rpc(err));
        }
        Ok(resp.args)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A config write against an absent socket is an I/O error (the route maps it
    /// to a 503), not a panic.
    #[tokio::test]
    async fn config_set_against_absent_socket_is_io_error() {
        let dir = tempfile::tempdir().unwrap();
        let client = PluginControlClient::new(dir.path().join("absent.sock"));
        let err = client
            .config_set("p", "active", Value::Boolean(true), Some("drone"))
            .await
            .unwrap_err();
        assert!(
            matches!(err, PluginControlError::Io(_)),
            "expected Io: {err:?}"
        );
    }

    /// A tool invoke against an absent socket is likewise an I/O error (a 503),
    /// never a panic.
    #[tokio::test]
    async fn tool_invoke_against_absent_socket_is_io_error() {
        let dir = tempfile::tempdir().unwrap();
        let client = PluginControlClient::new(dir.path().join("absent.sock"));
        let err = client
            .tool_invoke("p", "greet", Value::Map(vec![]), None)
            .await
            .unwrap_err();
        assert!(
            matches!(err, PluginControlError::Io(_)),
            "expected Io: {err:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_host_that_accepts_and_stalls_times_out() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("control.sock");
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        let server = tokio::spawn(async move {
            let (conn, _) = listener.accept().await.unwrap();
            tokio::time::sleep(std::time::Duration::from_secs(600)).await;
            drop(conn);
        });
        let client = PluginControlClient::new(path);
        let err = client.config_get("p").await.unwrap_err();
        assert!(matches!(err, PluginControlError::Timeout), "{err:?}");
        server.abort();
    }
}
