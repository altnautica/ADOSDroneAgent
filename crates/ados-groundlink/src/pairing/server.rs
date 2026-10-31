//! The pairing service's Unix-socket RPC (`ados-groundlink --pairing`).
//!
//! One newline-terminated JSON request per connection, read with a 5 s
//! timeout; one newline-terminated JSON reply; close.
//!
//! ```text
//! > {"op": "open_window", "args": {"duration_s": 60}}
//! < {"ok": true, "result": {"opened_at_ms": ..., "closes_at_ms": ..., "code": "..."}}
//! ```
//!
//! Ops: `open_window`, `close_window`, `is_window_open`, `snapshot`, `approve`,
//! `revoke` (receiver side) and `join` (relay side). Failures are
//! `{"ok": false, "error": "..."}`; a failed `join` adds `error_code`.

use std::future::Future;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Map, Value};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::Mutex;
use tokio::task::JoinHandle;

use super::client::{request_join, JOIN_TIMEOUT};
use super::env::PairingEnv;
use super::manager::MeshPairingManager;
use super::rekey::{self, RevocationCache};
use super::relay_rekey::{self, RelayRekeyReceiver, RelayRekeyState};
use super::DEFAULT_ACCEPT_WINDOW_S;

const READ_TIMEOUT: Duration = Duration::from_secs(5);
/// Bound on one request line.
const MAX_REQUEST_BYTES: u64 = 64 * 1024;

/// The pairing service: the accept window, the revocation cache and the relay
/// re-key listener, behind one RPC dispatcher.
pub struct PairingService {
    env: Arc<PairingEnv>,
    revoked: Arc<RevocationCache>,
    manager: Arc<MeshPairingManager>,
    relay_listener: Mutex<Option<JoinHandle<()>>>,
}

impl PairingService {
    pub fn new(env: PairingEnv) -> Arc<Self> {
        let env = Arc::new(env);
        let revoked = Arc::new(RevocationCache::default());
        let manager = MeshPairingManager::new(Arc::clone(&env), Arc::clone(&revoked));
        Arc::new(Self {
            env,
            revoked,
            manager,
            relay_listener: Mutex::new(None),
        })
    }

    pub fn manager(&self) -> &Arc<MeshPairingManager> {
        &self.manager
    }

    /// Resume listening for re-sealed bundles from a join stored on disk.
    pub async fn resume_relay_rekey(&self) {
        if let Some(state) = relay_rekey::load_state(&self.env.mesh_dir) {
            self.listen_for_rekey(state).await;
        }
    }

    async fn listen_for_rekey(&self, state: RelayRekeyState) {
        let mut slot = self.relay_listener.lock().await;
        if let Some(old) = slot.take() {
            old.abort();
        }
        let port = state.port;
        let Some(receiver) = RelayRekeyReceiver::new(Arc::clone(&self.env), state) else {
            return;
        };
        match relay_rekey::listen(receiver).await {
            Ok(handle) => *slot = Some(handle),
            Err(e) => tracing::error!(port, error = %e, "relay_rekey_listen_failed"),
        }
    }

    /// Dispatch one op to its reply.
    pub async fn handle_op(&self, op: &str, args: &Map<String, Value>) -> Value {
        match self.dispatch(op, args).await {
            Ok(result) => json!({"ok": true, "result": result}),
            Err(reply) => reply,
        }
    }

    async fn dispatch(&self, op: &str, args: &Map<String, Value>) -> Result<Value, Value> {
        match op {
            "open_window" => {
                let duration_s = duration_arg(args.get("duration_s"))?;
                let w = self.manager.open_window(duration_s).await.map_err(error)?;
                Ok(json!({
                    "opened_at_ms": w.opened_at_ms,
                    "closes_at_ms": w.closes_at_ms,
                    "code": w.code,
                }))
            }
            "close_window" => {
                let was_open = self.manager.is_window_open().await;
                self.manager.close_window().await;
                Ok(json!({"closed": was_open}))
            }
            "is_window_open" => Ok(json!({"open": self.manager.is_window_open().await})),
            "snapshot" => Ok(self.manager.snapshot().await),
            "approve" => {
                let device_id = device_id_arg(args)?;
                let bundle = self
                    .env
                    .build_invite_bundle()
                    .ok_or_else(|| error("mesh not initialized"))?;
                let blob = self
                    .manager
                    .approve(&device_id, &bundle)
                    .await
                    .map_err(error)?
                    .ok_or_else(|| error("pair request not found or window closed"))?;
                Ok(json!({
                    "approved": true,
                    "invite_blob_hex": hex::encode(blob),
                    "issued_at_ms": bundle.issued_at_ms,
                    "expires_at_ms": bundle.expires_at_ms,
                }))
            }
            "revoke" => {
                let device_id = device_id_arg(args)?;
                let revoked = rekey::revoke(&self.env, &self.revoked, &device_id)
                    .await
                    .map_err(|e| error(e.to_string()))?;
                Ok(json!({"revoked": revoked}))
            }
            "join" => self.join(args).await,
            _ => Err(error(format!("unknown op: {op}"))),
        }
    }

    async fn join(&self, args: &Map<String, Value>) -> Result<Value, Value> {
        let code = match args.get("code") {
            Some(Value::String(s)) => s.clone(),
            Some(Value::Null) | None => String::new(),
            Some(other) => other.to_string(),
        };
        let host = args
            .get("receiver_host")
            .and_then(Value::as_str)
            .filter(|h| !h.is_empty());
        let port = match args.get("receiver_port") {
            None | Some(Value::Null) => None,
            Some(v) => match v.as_u64() {
                Some(0) => None,
                Some(p) => Some(u16::try_from(p).map_err(|_| error("receiver_port out of range"))?),
                None => return Err(error("receiver_port must be an integer")),
            },
        };
        match request_join(&self.env, &code, host, port, JOIN_TIMEOUT).await {
            Ok(joined) => {
                if let Err(e) = relay_rekey::save_state(&self.env.mesh_dir, &joined.rekey) {
                    tracing::error!(error = %e, "relay_rekey_state_write_failed");
                }
                self.listen_for_rekey(joined.rekey).await;
                Ok(json!({"mesh_id": joined.mesh_id, "receiver_host": joined.receiver_host}))
            }
            Err(e) => Err(json!({"ok": false, "error": e.message, "error_code": e.code})),
        }
    }
}

fn error(message: impl Into<String>) -> Value {
    json!({"ok": false, "error": message.into()})
}

fn device_id_arg(args: &Map<String, Value>) -> Result<String, Value> {
    let id = match args.get("device_id") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Null) | None => String::new(),
        Some(other) => other.to_string(),
    };
    if id.is_empty() {
        return Err(error("device_id required"));
    }
    Ok(id)
}

fn duration_arg(v: Option<&Value>) -> Result<u64, Value> {
    let secs = match v {
        None => Some(DEFAULT_ACCEPT_WINDOW_S as i64),
        Some(Value::Number(n)) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)),
        Some(Value::String(s)) => s.trim().parse::<i64>().ok(),
        Some(_) => None,
    };
    match secs {
        Some(s) if s >= 0 => Ok(s as u64),
        _ => Err(error("duration_s must be a non-negative integer")),
    }
}

/// Parse one request line into its reply.
pub async fn reply_for_line(service: &PairingService, line: &[u8]) -> Value {
    let request: Value = match serde_json::from_slice(line) {
        Ok(v) => v,
        Err(e) => return error(format!("malformed request: {e}")),
    };
    let Some(request) = request.as_object() else {
        return error("malformed request: request must be object");
    };
    let op = match request.get("op") {
        Some(Value::String(s)) => s.clone(),
        None | Some(Value::Null) => String::new(),
        Some(other) => other.to_string(),
    };
    let empty = Map::new();
    let args = match request.get("args") {
        None | Some(Value::Null) => &empty,
        Some(Value::Object(m)) if m.is_empty() => &empty,
        Some(Value::Object(m)) => m,
        Some(_) => return error("malformed request: args must be object"),
    };
    service.handle_op(&op, args).await
}

async fn serve_connection(service: Arc<PairingService>, stream: UnixStream) {
    let (read, mut write) = stream.into_split();
    let mut reader = BufReader::new(read.take(MAX_REQUEST_BYTES));
    let mut line = Vec::new();
    match tokio::time::timeout(READ_TIMEOUT, reader.read_until(b'\n', &mut line)).await {
        Ok(Ok(n)) if n > 0 => {}
        _ => return,
    }
    let reply = reply_for_line(&service, &line).await;
    let mut body = reply.to_string().into_bytes();
    body.push(b'\n');
    if let Err(e) = write.write_all(&body).await {
        tracing::debug!(error = %e, "pairing_daemon_reply_failed");
    }
    let _ = write.shutdown().await;
}

/// Serve the RPC on `path` until `shutdown` resolves. A stale socket is
/// unlinked first; the socket is root-only (0600) because approve and revoke
/// admit or eject relays.
pub async fn serve(
    path: &Path,
    service: Arc<PairingService>,
    shutdown: impl Future<Output = ()>,
) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    let listener = UnixListener::bind(path)?;
    if let Err(e) = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)) {
        tracing::warn!(error = %e, "pairing_daemon_socket_chmod_failed");
    }
    tracing::info!(socket = %path.display(), "pairing_daemon_ready");
    tokio::pin!(shutdown);
    loop {
        tokio::select! {
            _ = &mut shutdown => break,
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => {
                    tokio::spawn(serve_connection(Arc::clone(&service), stream));
                }
                Err(e) => {
                    tracing::warn!(error = %e, "pairing_daemon_accept_failed");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            },
        }
    }
    tracing::info!("pairing_daemon_stopping");
    if let Some(l) = service.relay_listener.lock().await.take() {
        l.abort();
    }
    let _ = std::fs::remove_file(path);
    Ok(())
}
