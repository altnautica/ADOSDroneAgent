//! Async IPC client: the plugin runner side of Contract C.
//!
//! Ports `ados.plugins.ipc_client.PluginIpcClient`. One instance per plugin
//! process. It connects to the supervisor's per-plugin Unix socket, runs the
//! `hello` handshake (echoing the capability token the supervisor minted),
//! then serves two flows on the one connection:
//!
//! * request / response — every call mints a fresh `r<n>` request id, parks a
//!   one-shot waiter in the pending map, writes the request frame, and awaits
//!   the response the reader loop routes back by `request_id`.
//! * event / MAVLink push — the reader loop dispatches `event` envelopes to the
//!   topic-matched event callbacks and `mavlink.deliver` envelopes to the
//!   message-name-matched MAVLink callbacks.
//!
//! The wire is `ados-protocol` unchanged: length-prefixed msgpack [`Envelope`]
//! frames (plugin contract: reject zero length, 4 MiB cap) and the
//! pipe-delimited HMAC [`ados_protocol::plugin::CapabilityToken`].

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ados_protocol::frame::{decode_len, FrameError, HEADER_SIZE, PLUGIN_MAX_FRAME};
use ados_protocol::plugin::{Envelope, PROTOCOL_VERSION};
use rmpv::Value;
use thiserror::Error;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::UnixStream;
use tokio::sync::{oneshot, Mutex as AsyncMutex};
use tokio::task::JoinHandle;

/// Default per-request timeout. Mirrors `DEFAULT_REQUEST_TIMEOUT_S = 5.0`.
pub const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

/// A callback invoked for each delivered event or MAVLink frame. The argument
/// is the delivered envelope's `args` map (topic + payload for events; the
/// `msg_name` + `frame` + `timestamp_ms` map for MAVLink). The callback runs on
/// the reader task, so it should not block; offload heavy work to a channel.
pub type EventCallback = Arc<dyn Fn(Value) + Send + Sync>;

/// Errors surfaced to plugin code by the client.
#[derive(Debug, Error)]
pub enum ClientError {
    #[error("ipc client not connected")]
    NotConnected,
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("framing error: {0}")]
    Frame(#[from] FrameError),
    #[error("envelope codec error: {0}")]
    Envelope(#[from] ados_protocol::plugin::PluginError),
    #[error("request timed out after {0:?}")]
    Timeout(Duration),
    /// The reader loop ended before the response arrived (peer closed).
    #[error("connection closed before response")]
    ConnectionClosed,
    /// The host gated the request: `capability_denied: <cap>`.
    #[error("capability denied: {0}")]
    CapabilityDenied(String),
    /// The manifest spawn allowlist refused the basename:
    /// `allowlist_violation: <basename>`.
    #[error("allowlist violation: {0}")]
    AllowlistViolation(String),
    /// Any other error string the host returned in the envelope `error` field.
    #[error("rpc error: {0}")]
    Rpc(String),
}

type PendingMap = Arc<Mutex<HashMap<String, oneshot::Sender<Envelope>>>>;
type CallbackMap = Arc<Mutex<HashMap<String, Vec<EventCallback>>>>;

/// Async client. One instance per plugin runner process.
pub struct PluginIpcClient {
    plugin_id: String,
    token: String,
    socket_path: PathBuf,
    /// The write half is shared behind an async mutex so concurrent request
    /// senders serialize their frame writes.
    writer: Arc<AsyncMutex<Option<OwnedWriteHalf>>>,
    pending: PendingMap,
    event_callbacks: CallbackMap,
    mavlink_callbacks: CallbackMap,
    reader_task: Mutex<Option<JoinHandle<()>>>,
    next_id: AtomicU64,
    request_timeout: Duration,
}

impl PluginIpcClient {
    /// Build a client for one plugin, bound to a socket path and the capability
    /// token the supervisor minted for this process.
    pub fn new(
        plugin_id: impl Into<String>,
        token: impl Into<String>,
        socket_path: impl AsRef<Path>,
    ) -> Self {
        Self {
            plugin_id: plugin_id.into(),
            token: token.into(),
            socket_path: socket_path.as_ref().to_path_buf(),
            writer: Arc::new(AsyncMutex::new(None)),
            pending: Arc::new(Mutex::new(HashMap::new())),
            event_callbacks: Arc::new(Mutex::new(HashMap::new())),
            mavlink_callbacks: Arc::new(Mutex::new(HashMap::new())),
            reader_task: Mutex::new(None),
            next_id: AtomicU64::new(0),
            request_timeout: DEFAULT_REQUEST_TIMEOUT,
        }
    }

    /// Override the per-request timeout (default [`DEFAULT_REQUEST_TIMEOUT`]).
    pub fn with_request_timeout(mut self, timeout: Duration) -> Self {
        self.request_timeout = timeout;
        self
    }

    /// The plugin id this client carries.
    pub fn plugin_id(&self) -> &str {
        &self.plugin_id
    }

    /// Connect to the socket, spawn the reader loop, and run the `hello`
    /// handshake. Mirrors `connect`: open the connection, start the reader,
    /// then send the handshake request and await the `{"ready": true}` reply.
    pub async fn connect(&self) -> Result<(), ClientError> {
        let stream = UnixStream::connect(&self.socket_path).await?;
        let (read_half, write_half) = stream.into_split();
        {
            let mut w = self.writer.lock().await;
            *w = Some(write_half);
        }
        let task = self.spawn_reader_loop(read_half);
        {
            let mut slot = self.reader_task.lock().expect("reader_task lock");
            *slot = Some(task);
        }
        // Handshake: the host verifies the token then replies {"ready": true}.
        self.send_request("hello", "", Value::Map(vec![])).await?;
        tracing::info!(plugin_id = %self.plugin_id, "plugin ipc client connected");
        Ok(())
    }

    /// Cancel the reader loop and drop the write half. Mirrors `close`.
    pub async fn close(&self) {
        if let Some(task) = self.reader_task.lock().expect("reader_task lock").take() {
            task.abort();
        }
        let mut w = self.writer.lock().await;
        *w = None;
    }

    // ---- Health -------------------------------------------------------

    /// Health probe. Mirrors `ping`: `{"pong": true, "plugin_id": <id>}`.
    pub async fn ping(&self) -> Result<Value, ClientError> {
        Ok(self
            .send_request("ping", "", Value::Map(vec![]))
            .await?
            .args)
    }

    // ---- Events -------------------------------------------------------

    /// Publish an event. Returns the delivered count. Mirrors `event_publish`.
    pub async fn event_publish(&self, topic: &str, payload: Value) -> Result<i64, ClientError> {
        let args = Value::Map(vec![
            (Value::from("topic"), Value::from(topic)),
            (Value::from("payload"), payload),
        ]);
        let resp = self
            .send_request("event.publish", "event.publish", args)
            .await?;
        Ok(map_get_i64(&resp.args, "delivered").unwrap_or(0))
    }

    /// Subscribe to an event topic pattern and register a callback for matched
    /// deliveries. Mirrors `event_subscribe`.
    pub async fn event_subscribe(
        &self,
        topic_pattern: &str,
        callback: EventCallback,
    ) -> Result<(), ClientError> {
        register_callback(&self.event_callbacks, topic_pattern, callback);
        let args = Value::Map(vec![(Value::from("topic"), Value::from(topic_pattern))]);
        self.send_request("event.subscribe", "event.subscribe", args)
            .await?;
        Ok(())
    }

    // ---- MAVLink ------------------------------------------------------

    /// Send a MAVLink frame, optionally from a registered component id.
    /// Mirrors `mavlink_send`.
    pub async fn mavlink_send(
        &self,
        msg_bytes: &[u8],
        component_id: Option<i64>,
    ) -> Result<Value, ClientError> {
        let mut entries = vec![(Value::from("msg_bytes"), Value::Binary(msg_bytes.to_vec()))];
        if let Some(comp) = component_id {
            entries.push((Value::from("component_id"), Value::from(comp)));
        }
        Ok(self
            .send_request("mavlink.send", "mavlink.write", Value::Map(entries))
            .await?
            .args)
    }

    /// Register a MAVLink component of the given kind (camera, gimbal, payload,
    /// peripheral, vio). The required cap is `mavlink.component.<kind>`. Mirrors
    /// `mavlink_register_component`.
    pub async fn mavlink_register_component(
        &self,
        comp_id: i64,
        kind: &str,
    ) -> Result<Value, ClientError> {
        let args = Value::Map(vec![
            (Value::from("component_id"), Value::from(comp_id)),
            (Value::from("kind"), Value::from(kind)),
        ]);
        let cap = format!("mavlink.component.{kind}");
        Ok(self
            .send_request("mavlink.register_component", &cap, args)
            .await?
            .args)
    }

    /// Subscribe to a MAVLink message name and register a callback. MAVLink
    /// deliveries arrive as `mavlink.deliver` envelopes and route by
    /// `msg_name`. Mirrors `mavlink_subscribe`.
    pub async fn mavlink_subscribe(
        &self,
        msg_name: &str,
        callback: EventCallback,
    ) -> Result<(), ClientError> {
        register_callback(&self.mavlink_callbacks, msg_name, callback);
        let args = Value::Map(vec![(Value::from("msg_name"), Value::from(msg_name))]);
        self.send_request("mavlink.subscribe", "mavlink.read", args)
            .await?;
        Ok(())
    }

    // ---- Telemetry ----------------------------------------------------

    /// Extend the heartbeat schema with a new channel. Mirrors
    /// `telemetry_extend`.
    pub async fn telemetry_extend(
        &self,
        channel: &str,
        payload: Value,
    ) -> Result<Value, ClientError> {
        let args = Value::Map(vec![
            (Value::from("channel"), Value::from(channel)),
            (Value::from("payload"), payload),
        ]);
        Ok(self
            .send_request("telemetry.extend", "telemetry.extend", args)
            .await?
            .args)
    }

    // ---- Peripheral manager ------------------------------------------

    /// Register a driver of the given kind, referenced by an opaque id the host
    /// records. The required cap is `sensor.<kind>.register`. Mirrors
    /// `peripheral_register_driver`.
    pub async fn peripheral_register_driver(
        &self,
        kind: &str,
        driver_ref: &str,
    ) -> Result<Value, ClientError> {
        let args = Value::Map(vec![
            (Value::from("kind"), Value::from(kind)),
            (Value::from("driver_ref"), Value::from(driver_ref)),
        ]);
        let cap = format!("sensor.{kind}.register");
        Ok(self
            .send_request("peripheral.register_driver", &cap, args)
            .await?
            .args)
    }

    /// Release a previously-registered driver by handle. Mirrors
    /// `peripheral_unregister_driver`.
    pub async fn peripheral_unregister_driver(
        &self,
        handle_id: &str,
    ) -> Result<Value, ClientError> {
        let args = Value::Map(vec![(Value::from("handle_id"), Value::from(handle_id))]);
        Ok(self
            .send_request("peripheral.unregister_driver", "", args)
            .await?
            .args)
    }

    /// Claim a `/dev/videoN` path, optionally exclusive. Mirrors `camera_claim`.
    pub async fn camera_claim(
        &self,
        device_path: &str,
        exclusive: bool,
    ) -> Result<Value, ClientError> {
        let args = Value::Map(vec![
            (Value::from("device_path"), Value::from(device_path)),
            (Value::from("exclusive"), Value::Boolean(exclusive)),
        ]);
        Ok(self
            .send_request("camera.claim", "sensor.camera.register", args)
            .await?
            .args)
    }

    /// Release a previously-claimed camera path. Mirrors `camera_release`.
    pub async fn camera_release(&self, device_path: &str) -> Result<Value, ClientError> {
        let args = Value::Map(vec![(Value::from("device_path"), Value::from(device_path))]);
        Ok(self
            .send_request("camera.release", "sensor.camera.register", args)
            .await?
            .args)
    }

    /// Pull the latest captured frame from a claimed camera path. Mirrors
    /// `camera_get_frame`.
    pub async fn camera_get_frame(
        &self,
        device_path: &str,
        format: &str,
        timeout_ms: i64,
    ) -> Result<Value, ClientError> {
        let args = Value::Map(vec![
            (Value::from("device_path"), Value::from(device_path)),
            (Value::from("format"), Value::from(format)),
            (Value::from("timeout_ms"), Value::from(timeout_ms)),
        ]);
        Ok(self
            .send_request("camera.get_frame", "sensor.camera.register", args)
            .await?
            .args)
    }

    // ---- Config kv ----------------------------------------------------

    /// Read a config key, returning the host's `value` (nil when absent).
    /// Mirrors `config_get`.
    pub async fn config_get(&self, key: &str, default: Value) -> Result<Value, ClientError> {
        let args = Value::Map(vec![
            (Value::from("key"), Value::from(key)),
            (Value::from("default"), default),
        ]);
        let resp = self.send_request("config.get", "", args).await?;
        Ok(map_get(&resp.args, "value").unwrap_or(Value::Nil))
    }

    /// Write a config key in the given scope (`drone` or `global`). Mirrors
    /// `config_set`.
    pub async fn config_set(
        &self,
        key: &str,
        value: Value,
        scope: &str,
    ) -> Result<Value, ClientError> {
        let args = Value::Map(vec![
            (Value::from("key"), Value::from(key)),
            (Value::from("value"), value),
            (Value::from("scope"), Value::from(scope)),
        ]);
        Ok(self.send_request("config.set", "", args).await?.args)
    }

    // ---- Process spawn (sandboxed) -----------------------------------

    /// Authorize a vendor-binary spawn through the supervisor. The host enforces
    /// the manifest allowlist and audit-logs the attempt. Mirrors
    /// `process_spawn`. The required cap is `process.spawn`.
    pub async fn process_spawn(
        &self,
        basename: &str,
        args: Vec<String>,
        env: Vec<(String, String)>,
    ) -> Result<Value, ClientError> {
        let args_val = Value::Array(args.into_iter().map(Value::from).collect());
        let env_val = Value::Map(
            env.into_iter()
                .map(|(k, v)| (Value::from(k), Value::from(v)))
                .collect(),
        );
        let payload = Value::Map(vec![
            (Value::from("basename"), Value::from(basename)),
            (Value::from("args"), args_val),
            (Value::from("env"), env_val),
        ]);
        Ok(self
            .send_request("process.spawn", "process.spawn", payload)
            .await?
            .args)
    }

    // ------------------------------------------------------------------
    // Internals
    // ------------------------------------------------------------------

    /// Mint a request id, park a waiter, write the frame, then await the routed
    /// response. Mirrors `_send_request`, including the error-string mapping to
    /// typed `capability_denied` / `allowlist_violation` errors.
    async fn send_request(
        &self,
        method: &str,
        capability: &str,
        args: Value,
    ) -> Result<Envelope, ClientError> {
        let n = self.next_id.fetch_add(1, Ordering::Relaxed) + 1;
        let rid = format!("r{n}");
        let env = Envelope {
            version: PROTOCOL_VERSION,
            kind: "request".to_string(),
            method: method.to_string(),
            capability: capability.to_string(),
            args,
            request_id: rid.clone(),
            token: self.token.clone(),
            error: None,
        };
        let frame = env.encode_frame()?;

        let (tx, rx) = oneshot::channel();
        {
            let mut pending = self.pending.lock().expect("pending lock");
            pending.insert(rid.clone(), tx);
        }

        // Write under the shared async lock so concurrent senders serialize.
        {
            let mut guard = self.writer.lock().await;
            let writer = guard.as_mut().ok_or(ClientError::NotConnected);
            match writer {
                Ok(w) => {
                    if let Err(e) = w.write_all(&frame).await.and(w.flush().await) {
                        self.pending.lock().expect("pending lock").remove(&rid);
                        return Err(ClientError::Io(e));
                    }
                }
                Err(_) => {
                    self.pending.lock().expect("pending lock").remove(&rid);
                    return Err(ClientError::NotConnected);
                }
            }
        }

        let response = match tokio::time::timeout(self.request_timeout, rx).await {
            Ok(Ok(env)) => env,
            // The waiter was dropped: the reader loop ended before the response.
            Ok(Err(_)) => {
                self.pending.lock().expect("pending lock").remove(&rid);
                return Err(ClientError::ConnectionClosed);
            }
            Err(_) => {
                self.pending.lock().expect("pending lock").remove(&rid);
                return Err(ClientError::Timeout(self.request_timeout));
            }
        };

        if let Some(err) = &response.error {
            return Err(map_error_string(err));
        }
        Ok(response)
    }

    /// Spawn the reader loop. Owns the read half; routes responses by
    /// `request_id` and dispatches `event` / `mavlink.deliver` pushes to the
    /// registered callbacks. Mirrors `_reader_loop`.
    fn spawn_reader_loop(&self, read_half: OwnedReadHalf) -> JoinHandle<()> {
        let pending = self.pending.clone();
        let event_callbacks = self.event_callbacks.clone();
        let mavlink_callbacks = self.mavlink_callbacks.clone();
        let plugin_id = self.plugin_id.clone();
        tokio::spawn(async move {
            let mut reader = read_half;
            loop {
                match read_envelope(&mut reader).await {
                    Ok(Some(env)) => {
                        if env.kind == "event" {
                            if env.method == "mavlink.deliver" {
                                dispatch_mavlink(&mavlink_callbacks, &env);
                            } else {
                                dispatch_event(&event_callbacks, &env);
                            }
                        } else {
                            // request/response: route to the parked waiter.
                            if let Some(tx) = pending
                                .lock()
                                .expect("pending lock")
                                .remove(&env.request_id)
                            {
                                let _ = tx.send(env);
                            }
                        }
                    }
                    // Clean EOF (peer closed) or a frame error: end the loop.
                    Ok(None) => break,
                    Err(e) => {
                        tracing::warn!(plugin_id = %plugin_id, error = %e, "plugin ipc reader loop ended");
                        break;
                    }
                }
            }
            // Drop every parked waiter so in-flight requests fail fast with
            // ConnectionClosed rather than waiting out their timeout.
            pending.lock().expect("pending lock").clear();
        })
    }
}

/// Read one length-prefixed msgpack envelope. `Ok(None)` on clean EOF before
/// the header. Uses `ados-protocol` framing (plugin contract: reject zero,
/// 4 MiB cap) — no wire re-implementation.
async fn read_envelope<R: AsyncReadExt + Unpin>(
    reader: &mut R,
) -> Result<Option<Envelope>, ClientError> {
    let mut header = [0u8; HEADER_SIZE];
    match reader.read_exact(&mut header).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(ClientError::Io(e)),
    }
    let len = decode_len(header, PLUGIN_MAX_FRAME, true)?;
    let mut body = vec![0u8; len];
    reader.read_exact(&mut body).await?;
    Ok(Some(Envelope::from_msgpack(&body)?))
}

/// Register a callback under a topic / message-name pattern. Mirrors the
/// `setdefault(pattern, []).append(callback)` of the Python client.
fn register_callback(map: &CallbackMap, pattern: &str, callback: EventCallback) {
    map.lock()
        .expect("callback lock")
        .entry(pattern.to_string())
        .or_default()
        .push(callback);
}

/// Dispatch an `event` delivery to every callback whose pattern matches the
/// envelope's `topic`. Mirrors `_dispatch_event`.
fn dispatch_event(map: &CallbackMap, env: &Envelope) {
    let Some(topic) = map_get_str(&env.args, "topic") else {
        return;
    };
    invoke_matching(map, topic, &env.args);
}

/// Dispatch a `mavlink.deliver` to every callback whose pattern matches the
/// envelope's `msg_name`. Mirrors `_dispatch_mavlink`: the callback sees the
/// full args map (`msg_name`, `frame`, `timestamp_ms`).
fn dispatch_mavlink(map: &CallbackMap, env: &Envelope) {
    let Some(msg_name) = map_get_str(&env.args, "msg_name") else {
        return;
    };
    invoke_matching(map, msg_name, &env.args);
}

/// Invoke every callback whose registered pattern equals `subject` or matches
/// it as a glob. Mirrors the `pattern == subject or fnmatch(...)` test in both
/// Python dispatchers.
fn invoke_matching(map: &CallbackMap, subject: &str, args: &Value) {
    // Snapshot the matched callbacks under the lock, then invoke outside it so a
    // callback cannot deadlock by subscribing from within itself.
    let matched: Vec<EventCallback> = {
        let guard = map.lock().expect("callback lock");
        guard
            .iter()
            .filter(|(pattern, _)| pattern.as_str() == subject || fnmatch(pattern, subject))
            .flat_map(|(_, cbs)| cbs.iter().cloned())
            .collect()
    };
    for cb in matched {
        cb(args.clone());
    }
}

/// Map the host's error string to the typed [`ClientError`]. Mirrors the
/// `_send_request` error-prefix handling: `capability_denied:` and
/// `allowlist_violation:` get dedicated variants; everything else is `Rpc`.
fn map_error_string(err: &str) -> ClientError {
    if let Some(rest) = err.strip_prefix("capability_denied:") {
        ClientError::CapabilityDenied(rest.trim().to_string())
    } else if let Some(rest) = err.strip_prefix("allowlist_violation:") {
        ClientError::AllowlistViolation(rest.trim().to_string())
    } else if err.contains("not permitted") {
        // The per-topic publish/subscribe refusal carries no machine prefix;
        // surface it as a capability denial like the Python client does.
        ClientError::CapabilityDenied(err.to_string())
    } else {
        ClientError::Rpc(err.to_string())
    }
}

// ---------------------------------------------------------------------------
// msgpack-map field readers (the args are an rmpv::Value::Map)
// ---------------------------------------------------------------------------

fn map_get(args: &Value, key: &str) -> Option<Value> {
    match args {
        Value::Map(entries) => entries
            .iter()
            .find(|(k, _)| k.as_str() == Some(key))
            .map(|(_, v)| v.clone()),
        _ => None,
    }
}

fn map_get_str<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    match args {
        Value::Map(entries) => entries
            .iter()
            .find(|(k, _)| k.as_str() == Some(key))
            .and_then(|(_, v)| v.as_str()),
        _ => None,
    }
}

fn map_get_i64(args: &Value, key: &str) -> Option<i64> {
    match args {
        Value::Map(entries) => entries
            .iter()
            .find(|(k, _)| k.as_str() == Some(key))
            .and_then(|(_, v)| v.as_i64()),
        _ => None,
    }
}

/// Minimal glob matcher supporting `*` (any run, including across `.`) and `?`
/// (one char). Matches the host's `topic_matches` semantics for callback
/// routing, mirroring the Python client's `fnmatch.fnmatchcase`.
fn fnmatch(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0usize, 0usize);
    let (mut star_p, mut star_t): (Option<usize>, usize) = (None, 0);
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star_p = Some(pi);
            star_t = ti;
            pi += 1;
        } else if let Some(sp) = star_p {
            pi = sp + 1;
            star_t += 1;
            ti = star_t;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_ids_increment_and_are_r_prefixed() {
        let client = PluginIpcClient::new("com.example.demo", "tok", "/run/ados/plugins/x.sock");
        // The id stream is `r1`, `r2`, ... exactly like the Python `_next_id`.
        let a = client.next_id.fetch_add(1, Ordering::Relaxed) + 1;
        let b = client.next_id.fetch_add(1, Ordering::Relaxed) + 1;
        assert_eq!(format!("r{a}"), "r1");
        assert_eq!(format!("r{b}"), "r2");
    }

    #[test]
    fn fnmatch_routes_topics_like_the_host() {
        assert!(fnmatch("mavlink.*", "mavlink.heartbeat"));
        assert!(fnmatch("plugin.demo.*", "plugin.demo.metric"));
        assert!(fnmatch("HEARTBEAT", "HEARTBEAT"));
        assert!(!fnmatch("mavlink.*", "mavlinkx"));
        assert!(!fnmatch("vehicle.armed", "vehicle.disarmed"));
    }

    #[test]
    fn callback_map_dispatches_to_exact_and_glob_matches() {
        use std::sync::atomic::{AtomicUsize, Ordering as O};
        let map: CallbackMap = Arc::new(Mutex::new(HashMap::new()));
        let exact = Arc::new(AtomicUsize::new(0));
        let glob = Arc::new(AtomicUsize::new(0));
        let e = exact.clone();
        let g = glob.clone();
        register_callback(
            &map,
            "HEARTBEAT",
            Arc::new(move |_| {
                e.fetch_add(1, O::Relaxed);
            }),
        );
        register_callback(
            &map,
            "HEART*",
            Arc::new(move |_| {
                g.fetch_add(1, O::Relaxed);
            }),
        );
        let env = Envelope {
            version: PROTOCOL_VERSION,
            kind: "event".into(),
            method: "mavlink.deliver".into(),
            capability: String::new(),
            args: Value::Map(vec![(Value::from("msg_name"), Value::from("HEARTBEAT"))]),
            request_id: "evt".into(),
            token: String::new(),
            error: None,
        };
        dispatch_mavlink(&map, &env);
        assert_eq!(exact.load(O::Relaxed), 1);
        assert_eq!(glob.load(O::Relaxed), 1);
    }

    #[test]
    fn error_string_maps_to_typed_variants() {
        assert!(matches!(
            map_error_string("capability_denied: mission.read"),
            ClientError::CapabilityDenied(c) if c == "mission.read"
        ));
        assert!(matches!(
            map_error_string("allowlist_violation: ffmpeg"),
            ClientError::AllowlistViolation(b) if b == "ffmpeg"
        ));
        assert!(matches!(
            map_error_string("publish not permitted on topic mavlink.x"),
            ClientError::CapabilityDenied(_)
        ));
        assert!(matches!(
            map_error_string("something else"),
            ClientError::Rpc(m) if m == "something else"
        ));
    }

    #[test]
    fn map_readers_extract_fields() {
        let args = Value::Map(vec![
            (Value::from("topic"), Value::from("plugin.demo.x")),
            (Value::from("delivered"), Value::from(3i64)),
        ]);
        assert_eq!(map_get_str(&args, "topic"), Some("plugin.demo.x"));
        assert_eq!(map_get_i64(&args, "delivered"), Some(3));
        assert!(map_get(&args, "missing").is_none());
    }
}
