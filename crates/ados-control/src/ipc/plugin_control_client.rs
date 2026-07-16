//! The plugin-host control socket client.
//!
//! The plugin host (`ados-plugin-host`) holds each plugin's per-drone config in
//! an in-memory store; a disk write alone is not seen until restart. So a GCS
//! skill toggle (or a per-drone settings change) for a plugin the GCS is not has
//! to reach the LIVE store in the running daemon. The daemon exposes one
//! on-box, daemon-lifetime control socket (`/run/ados/plugins/_control.sock`)
//! for exactly this; this client is its caller.
//!
//! The wire is the same length-prefixed msgpack [`Envelope`] the vision socket
//! speaks (4-byte big-endian length + a msgpack envelope), request/response,
//! one fresh connection per call — config writes are infrequent. An absent
//! socket (the plugin host not up) surfaces as [`PluginControlError::Io`], which
//! the route maps to a 503 so a config write is never silently dropped.
//!
//! Auth: the off-box auth is the LAN pairing-key edge on `PUT
//! /api/plugins/{id}/config` (the same posture as `/api/vision/designate`); by
//! the time a request reaches this socket it is an on-box, trusted caller.

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
/// The default plugin socket directory (matches `DEFAULT_SOCKET_DIR` in the
/// plugin host). Overridable via `ADOS_PLUGIN_SOCKET_DIR` so a test / SITL run
/// points both the daemon and this client at a tempdir.
const PLUGIN_SOCKET_DIR_DEFAULT: &str = "/run/ados/plugins";
/// The control socket file name under the plugin socket dir.
const CONTROL_SOCKET_NAME: &str = "_control.sock";

/// The default control socket path (`ADOS_PLUGIN_SOCKET_DIR`-aware).
pub fn default_control_socket() -> PathBuf {
    let dir = std::env::var("ADOS_PLUGIN_SOCKET_DIR")
        .unwrap_or_else(|_| PLUGIN_SOCKET_DIR_DEFAULT.into());
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
}

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
        self.request(METHOD_CONFIG_SET, Value::Map(args)).await
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
        if let Some(ms) = timeout_ms {
            args.push((Value::from("timeout_ms"), Value::from(ms)));
        }
        self.request(METHOD_TOOL_INVOKE, Value::Map(args)).await
    }

    /// One fresh-connection request/response against the control socket.
    async fn request(&self, method: &str, args: Value) -> Result<Value, PluginControlError> {
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

        let mut stream = UnixStream::connect(&self.socket_path).await?;
        stream.write_all(&frame).await?;
        stream.flush().await?;

        let mut header = [0u8; HEADER_SIZE];
        stream.read_exact(&mut header).await?;
        let len = decode_len(header, PLUGIN_MAX_FRAME, false)
            .map_err(|e| PluginControlError::Frame(format!("response length: {e}")))?;
        let mut body = vec![0u8; len];
        stream.read_exact(&mut body).await?;
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
}
