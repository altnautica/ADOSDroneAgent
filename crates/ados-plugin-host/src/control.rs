//! The on-box plugin-host control socket.
//!
//! The per-plugin sockets (`server.rs`) are capability-token gated and bound to
//! one plugin's identity, so a plugin can only read/write its OWN config. But a
//! GCS skill toggle (or a per-drone settings change) needs to flip a plugin's
//! per-drone config without being that plugin — the write originates off-box at
//! the operator, lands at the native control surface (`ados-control`), and must
//! reach the LIVE in-memory [`crate::realhost::ConfigStore`] in this running
//! daemon (a disk write alone is not seen until restart). This module is that
//! reach: a single daemon-lifetime Unix socket at `<socket_dir>/_control.sock`
//! that applies an on-box config write to the live store and persists it.
//!
//! Trust boundary: the socket is on-box and bound with the same owner+group mode
//! as the per-plugin sockets (the `ados` group). The off-box auth lives at the
//! `ados-control` HTTP edge (the LAN pairing key when paired), exactly like
//! `POST /api/vision/designate`; by the time a request reaches this socket it is
//! an on-box, trusted caller. The wire is the same length-prefixed msgpack
//! [`Envelope`] every other agent IPC socket speaks, so no new framing.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use ados_protocol::frame::{decode_len, HEADER_SIZE, PLUGIN_MAX_FRAME};
use ados_protocol::plugin::{Envelope, PROTOCOL_VERSION};
use rmpv::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::task::JoinHandle;

/// The control socket file name under the per-plugin socket dir. The leading
/// underscore keeps it out of the `<plugin_id>.sock` namespace (no plugin id is
/// `_control`).
pub const CONTROL_SOCKET_NAME: &str = "_control.sock";

/// The control method that applies a per-plugin config write to the live store.
pub const METHOD_CONFIG_SET: &str = "config.set";

/// The control socket path under a socket dir.
pub fn control_socket_path(socket_dir: &Path) -> PathBuf {
    socket_dir.join(CONTROL_SOCKET_NAME)
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

/// Handle one decoded control request against the host. Pure of I/O so it unit
/// tests directly.
fn handle_request<H: ConfigControl>(host: &H, req: &Envelope) -> Envelope {
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

async fn serve_connection<H: ConfigControl>(host: Arc<H>, mut stream: UnixStream) {
    // One request/response per connection (the client opens fresh per call,
    // matching the vision IPC client). A read/decode failure just drops the
    // connection.
    let mut header = [0u8; HEADER_SIZE];
    if stream.read_exact(&mut header).await.is_err() {
        return;
    }
    let len = match decode_len(header, PLUGIN_MAX_FRAME, true) {
        Ok(l) => l,
        Err(_) => return,
    };
    let mut body = vec![0u8; len];
    if stream.read_exact(&mut body).await.is_err() {
        return;
    }
    let resp = match Envelope::from_msgpack(&body) {
        Ok(req) => handle_request(host.as_ref(), &req),
        Err(e) => err_response("", format!("decode control request: {e}")),
    };
    if let Ok(frame) = resp.encode_frame() {
        let _ = stream.write_all(&frame).await;
        let _ = stream.flush().await;
    }
}

/// Bind the control socket and spawn its accept loop. Mirrors
/// [`crate::server::PluginIpcServer::serve_plugin`]'s bind dance: ensure the
/// dir, unlink a stale socket, bind, set owner+group mode. Returns the bound
/// path and the accept-task handle so the daemon can unlink + abort on shutdown.
pub fn serve_control<H: ConfigControl + 'static>(
    host: Arc<H>,
    socket_dir: PathBuf,
) -> std::io::Result<(PathBuf, JoinHandle<()>)> {
    std::fs::create_dir_all(&socket_dir).ok();
    let path = control_socket_path(&socket_dir);
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path)?;
    set_control_socket_mode(&path)?;

    let task = tokio::spawn(async move {
        loop {
            let stream = match listener.accept().await {
                Ok((s, _addr)) => s,
                Err(_) => break,
            };
            let host = host.clone();
            tokio::spawn(async move {
                serve_connection(host, stream).await;
            });
        }
    });
    Ok((path, task))
}

/// Set the control socket to owner+group rw (0o660), the same mode the per-plugin
/// sockets use, so an `ados`-group on-box service (`ados-control`) can connect.
#[cfg(unix)]
fn set_control_socket_mode(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o660))
}

#[cfg(not(unix))]
fn set_control_socket_mode(_path: &Path) -> std::io::Result<()> {
    Ok(())
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
        let (path, task) = serve_control(host.clone(), dir.path().to_path_buf()).unwrap();

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
}
