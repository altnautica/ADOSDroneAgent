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
use ados_protocol::node_info::NodeInfo;
use ados_protocol::plugin::{CapabilityToken, Envelope, PROTOCOL_VERSION, TELEMETRY_STATE_TOPIC};
use ados_protocol::vision_rpc;
use rmpv::Value;
use thiserror::Error;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::UnixStream;
use tokio::sync::{oneshot, watch, Mutex as AsyncMutex};
use tokio::task::JoinHandle;

/// Default per-request timeout. Mirrors `DEFAULT_REQUEST_TIMEOUT_S = 5.0`.
pub const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

/// Slack added on top of the relay's own record-write bound before the client
/// gives up on a `cloud.records.put`. The host forwards the write and waits up
/// to [`ados_protocol::cloud_publish::RECORD_REPLY_TIMEOUT`] for the cloud's
/// answer; the client must outlast that wait or it reports a timeout for a
/// write the cloud then accepts.
const RECORD_REPLY_SLACK: Duration = Duration::from_secs(2);

/// The offload link a plugin running perception offload reports through
/// [`PluginIpcClient::offload_advertise`]. The host stamps the schema version
/// and write time and publishes it as the offload-link sidecar
/// ([`ados_protocol::offload_link::OffloadLink`]) the perception-tier decision
/// reads.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OffloadAdvertisement {
    /// A compute node resolved and is reachable.
    pub paired: bool,
    /// The bearer to that node is healthy enough to offload over.
    pub bearer_acceptable: bool,
    /// The node's job-API address (`host:port`).
    pub target: Option<String>,
    /// The node's device id, when discovery reported one.
    pub device_id: Option<String>,
    /// The detector model id the offload session runs.
    pub model_id: Option<String>,
}

/// The host's token-rotation push. Wire contract, pinned here because the
/// plugin side must adopt the token the host re-minted or it cannot reconnect
/// after the rotation; the host half is `ados_plugin_host::TOKEN_REFRESH_METHOD`
/// and the Python client's is `_TOKEN_REFRESH_METHOD`.
const TOKEN_REFRESH_METHOD: &str = "token.refresh";

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

/// A vision request this client could not build.
pub(crate) fn vision_args_error(e: vision_rpc::VisionArgsError) -> ClientError {
    ClientError::Rpc(format!("vision request encode failed: {e}"))
}

type PendingMap = Arc<Mutex<HashMap<String, oneshot::Sender<Envelope>>>>;

/// Runs one host-to-plugin `tool.invoke`: `(tool name, arguments)` to the
/// tool result, or an error string the host relays to the MCP caller. The
/// host waits a bounded time (5 s) for the reply, so a long operation returns
/// a job handle instead of blocking.
pub type ToolHandler = Arc<
    dyn Fn(
            String,
            Value,
        )
            -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Value, String>> + Send>>
        + Send
        + Sync,
>;

/// The method name of a host-to-plugin tool invocation.
pub const TOOL_INVOKE_METHOD: &str = "tool.invoke";
type CallbackMap = Arc<Mutex<HashMap<String, Vec<EventCallback>>>>;

/// Async client. One instance per plugin runner process.
pub struct PluginIpcClient {
    plugin_id: String,
    /// The capability token this client presents, shared with the reader loop
    /// because the host ROTATES it: a permission change or a pre-expiry
    /// re-mint arrives as a `token.refresh` push, and the copy presented at
    /// the next `hello` has to be the current one or the plugin cannot
    /// reconnect.
    token: Arc<Mutex<String>>,
    socket_path: PathBuf,
    /// The write half is shared behind an async mutex so concurrent request
    /// senders serialize their frame writes.
    writer: Arc<AsyncMutex<Option<OwnedWriteHalf>>>,
    pending: PendingMap,
    event_callbacks: CallbackMap,
    mavlink_callbacks: CallbackMap,
    /// Frame-descriptor callbacks for `vision.deliver` pushes, keyed by the
    /// camera id a subscriber filtered on. The wildcard key `*` receives every
    /// camera's frames. The reader loop routes a delivered descriptor by its
    /// `camera_id` to the matching key plus the wildcard.
    vision_callbacks: CallbackMap,
    /// Detection-batch callbacks, keyed by camera id (plus the wildcard key).
    /// Separate from `vision_callbacks` because a frame descriptor and a
    /// detection batch are different payloads on different subscriptions.
    detection_callbacks: CallbackMap,
    /// Front-panel button callbacks. Keyed on the wildcard only: a press has no
    /// subject to filter on, so every subscriber sees every press.
    button_callbacks: CallbackMap,
    /// MSP byte-plane callbacks. Keyed on the wildcard only: MSP has no per-
    /// message topic, so every subscriber sees every FC->host chunk.
    msp_callbacks: CallbackMap,
    /// Auxiliary-stream application-datagram callbacks for
    /// `radio.aux_stream.deliver` pushes. Keyed on the wildcard only: an app
    /// datagram has no subject to filter on, so every subscriber sees every
    /// frame (the plugin's own codec routes by channel).
    aux_callbacks: CallbackMap,
    /// The handler for host-to-plugin `tool.invoke` requests; `None` until the
    /// plugin registers one, and such a request is then answered with an error.
    tool_handler: Arc<Mutex<Option<ToolHandler>>>,
    reader_task: Mutex<Option<JoinHandle<()>>>,
    /// `true` once the reader loop ended on its own: the host closed the
    /// session or the stream broke. [`Self::close`] aborts the loop instead, so
    /// a deliberate close never reads as a loss.
    session_lost: Arc<watch::Sender<bool>>,
    next_id: AtomicU64,
    request_timeout: Duration,
}

/// The wildcard vision-callback key: a subscriber that did not filter to a
/// single camera registers here and receives every camera's frames.
const VISION_ANY_CAMERA: &str = "*";

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
            token: Arc::new(Mutex::new(token.into())),
            socket_path: socket_path.as_ref().to_path_buf(),
            writer: Arc::new(AsyncMutex::new(None)),
            pending: Arc::new(Mutex::new(HashMap::new())),
            event_callbacks: Arc::new(Mutex::new(HashMap::new())),
            mavlink_callbacks: Arc::new(Mutex::new(HashMap::new())),
            vision_callbacks: Arc::new(Mutex::new(HashMap::new())),
            detection_callbacks: Arc::new(Mutex::new(HashMap::new())),
            button_callbacks: Arc::new(Mutex::new(HashMap::new())),
            msp_callbacks: Arc::new(Mutex::new(HashMap::new())),
            aux_callbacks: Arc::new(Mutex::new(HashMap::new())),
            tool_handler: Arc::new(Mutex::new(None)),
            reader_task: Mutex::new(None),
            session_lost: Arc::new(watch::channel(false).0),
            next_id: AtomicU64::new(0),
            request_timeout: DEFAULT_REQUEST_TIMEOUT,
        }
    }

    /// Override the per-request timeout (default [`DEFAULT_REQUEST_TIMEOUT`]).
    pub fn with_request_timeout(mut self, timeout: Duration) -> Self {
        self.request_timeout = timeout;
        self
    }

    /// Register the handler that answers host-to-plugin `tool.invoke`
    /// requests (the plugin's MCP tools). Replaces any earlier handler.
    pub fn set_tool_handler(&self, handler: ToolHandler) {
        *self.tool_handler.lock().expect("tool handler lock") = Some(handler);
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
        self.session_lost.send_replace(false);
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

    /// Resolve once the host session is lost: the host closed the connection
    /// (a plugin-host restart) or the stream broke. The client never
    /// reconnects — a restarted host re-mints this plugin's socket and token
    /// and hands them to a fresh process — so a plugin that sees this should
    /// exit non-zero and let its unit's `Restart=` bring it back.
    /// [`crate::run_plugin`] does exactly that. Pending until then, including
    /// after a deliberate [`Self::close`].
    pub async fn session_lost(&self) {
        let mut rx = self.session_lost.subscribe();
        // The sender lives as long as `self`, so the wait cannot error out.
        let _ = rx.wait_for(|lost| *lost).await;
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
}

mod cloud;
mod config;
mod dispatch;
mod mavlink;
mod media;
mod peripheral;
mod transport;
mod vision;
pub use dispatch::driver_kind_capability;
use dispatch::*;
pub use media::WhepAnswer;

#[cfg(test)]
mod tests;
#[cfg(test)]
mod tests_host;
