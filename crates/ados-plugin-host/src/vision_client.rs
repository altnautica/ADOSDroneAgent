//! Client to the vision engine's IPC socket.
//!
//! The vision engine owns the cameras, the shared-memory frame rings, and the
//! inference backend. It serves `/run/ados/vision.sock`, which speaks the same
//! length-prefixed msgpack envelope wire as the plugin RPC socket (4-byte
//! big-endian length + a msgpack [`Envelope`], zero-length rejected). The plugin
//! host does not run inference; it proxies a plugin's vision request to the
//! engine over this socket and returns the engine's response, and it fans the
//! engine's frame-descriptor pushes out to subscribed plugins.
//!
//! Two paths:
//!
//! * Request/response: `register_model`, `infer`, `publish_detection` write one
//!   request envelope toward the engine and read the matching response envelope.
//!   Each call is serialized behind a connection mutex so concurrent plugin
//!   callers do not interleave frames on the single socket.
//! * Frame-descriptor push: a reader task drains the engine's `vision.deliver`
//!   event envelopes and fans the descriptor bytes out on a broadcast channel.
//!   Each subscribed plugin gets its own [`broadcast::Receiver`] from
//!   [`VisionClient::subscribe_frames`]; a slow consumer lags rather than
//!   wedging the reader (drop-on-full), matching the frame transport which is
//!   latest-wins.
//!
//! The connection reconnects forever at a fixed interval, so an engine that
//! comes up after the host, or restarts under it, heals by itself; requests
//! made while it is down fail fast with a transient error.
//!
//! Both paths reuse the `ados-protocol` framing and envelope primitives; no
//! wire is re-implemented here.

mod conn;
mod push;
mod shared;

use conn::{now_ms, run};

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use ados_protocol::frame::{decode_len, HEADER_SIZE, PLUGIN_MAX_FRAME};
use ados_protocol::framebus::methods;
use ados_protocol::plugin::{Envelope, PROTOCOL_VERSION};
use rmpv::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::unix::OwnedReadHalf;
use tokio::net::UnixStream;
use tokio::sync::{broadcast, mpsc, Mutex};
use tokio::task::JoinHandle;

use push::{detection_batch_bytes, frame_descriptor_bytes, peek_push_kind, PushKind};

/// Frame-descriptor fanout depth. A descriptor is tiny (a few dozen bytes); the
/// pixels live in shared memory. Depth bounds how far a stalled subscriber may
/// fall behind before it lags to the tail, which is the right policy for a
/// latest-wins frame stream.
pub const VISION_FRAME_BROADCAST_DEPTH: usize = 256;

/// Fixed wait between connection attempts to the engine socket. There is no
/// cap and no give-up state: the engine may start after the host, restart, or
/// not be installed yet, and the client must come back by itself whenever the
/// socket does.
pub const VISION_RECONNECT_INTERVAL: Duration = Duration::from_secs(2);

/// The error a proxied request gets while no engine connection is up. It is a
/// transient state, not a missing feature: the same call succeeds once the
/// engine socket is back.
pub const VISION_ENGINE_UNAVAILABLE: &str = "vision engine unavailable: not connected, retry";

/// A request the host proxies failed at the engine boundary. The body is the
/// string the host surfaces to the plugin as the response envelope `error`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VisionRpcError(pub String);

impl std::fmt::Display for VisionRpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for VisionRpcError {}

/// A self-healing client to the vision engine socket.
///
/// One task owns the connection and reconnects forever at a fixed interval, so
/// an engine that starts after the host, or restarts under it, heals without a
/// host restart. The engine's frame-descriptor pushes (`vision.deliver` event
/// envelopes) fan out on broadcast channels that outlive any one connection, so
/// a plugin's receiver keeps receiving across an engine restart. Plugin
/// requests (`register_model` / `infer` / `publish_detection` /
/// `designate_track`) are written under a connection mutex and matched to the
/// engine's in-order response; while disconnected they fail fast with
/// [`VISION_ENGINE_UNAVAILABLE`].
pub struct VisionClient {
    shared: Arc<Shared>,
    task: JoinHandle<()>,
}

/// State shared between the client handle and its connection task.
struct Shared {
    /// The live connection's request side, `None` while disconnected. The proxy
    /// writes a request then awaits its response under this lock, so requests
    /// are strictly one-at-a-time on the wire (the engine answers in order).
    request: Mutex<Option<RequestChannel>>,
    connected: AtomicBool,
    /// Frame-descriptor fanout: descriptor bytes pulled from the engine's
    /// `vision.deliver` pushes.
    frames: broadcast::Sender<Vec<u8>>,
    /// Detection-batch fanout: encoded `DetectionBatch` bytes pulled from the
    /// engine's `vision.deliver_detection` pushes.
    detections: broadcast::Sender<Vec<u8>>,
    /// Whether the ENGINE has been asked to push on the current connection.
    /// The engine starts a per-connection push task only when it receives
    /// `vision.subscribe_frames` / `vision.subscribe_detections`
    /// (`ados-vision/src/visionsock.rs`), so holding a receiver on the fanout
    /// above is not by itself a subscription: without these the fanout is
    /// permanently empty and a subscribing plugin sees silence with no error.
    /// Armed lazily on the first plugin subscribe, and re-armed on every new
    /// connection once any plugin has asked.
    ///
    /// A mutex held across the engine round-trip, not an `AtomicBool` tested and
    /// set before it. Two plugins subscribing at host startup is the normal
    /// case, and with a test-and-set the second one read `true` while the first
    /// one's request was still in flight, returned, and let its caller hand the
    /// plugin a receiver; if that request then failed, the flag went back to
    /// `false` and NOBODY was armed — the second plugin sat on a permanently
    /// empty fanout with not one log line naming its subscription, which is the
    /// exact silence this flag exists to prevent. Holding the flag makes a
    /// concurrent second subscriber wait for the first attempt's verdict and
    /// retry it.
    ///
    /// Lock order: frame push, then detection push, then `request`; never the
    /// other way round. Keep it that way.
    frame_push: Mutex<PushState>,
    detection_push: Mutex<PushState>,
}

/// One push stream's arming state.
#[derive(Default)]
struct PushState {
    /// A plugin has subscribed at least once, so every new connection re-arms.
    wanted: bool,
    /// The engine accepted the subscribe on the current connection.
    armed: bool,
}

/// The two engine push streams a plugin subscribe arms.
#[derive(Clone, Copy)]
enum Push {
    Frames,
    Detections,
}

/// The request-side half: the socket writer plus the response receiver the
/// reader feeds. Held behind the connection mutex.
struct RequestChannel {
    write_half: tokio::net::unix::OwnedWriteHalf,
    responses: mpsc::Receiver<Result<Value, String>>,
}

impl VisionClient {
    /// Start the connection task for `sock_path`. Never fails: an absent socket
    /// is retried every [`VISION_RECONNECT_INTERVAL`], and requests report
    /// [`VISION_ENGINE_UNAVAILABLE`] until a connection is up.
    pub fn spawn(sock_path: impl AsRef<Path>) -> Self {
        Self::spawn_with_interval(sock_path.as_ref().to_path_buf(), VISION_RECONNECT_INTERVAL)
    }

    pub(crate) fn spawn_with_interval(path: PathBuf, interval: Duration) -> Self {
        let (frames, _rx) = broadcast::channel(VISION_FRAME_BROADCAST_DEPTH);
        let (detections, _drx) = broadcast::channel(VISION_FRAME_BROADCAST_DEPTH);
        let shared = Arc::new(Shared {
            request: Mutex::new(None),
            connected: AtomicBool::new(false),
            frames,
            detections,
            frame_push: Mutex::new(PushState::default()),
            detection_push: Mutex::new(PushState::default()),
        });
        let task = tokio::spawn(run(Arc::clone(&shared), path, interval));
        Self { shared, task }
    }

    /// Whether a connection to the engine is live right now.
    pub fn is_connected(&self) -> bool {
        self.shared.connected.load(Ordering::Acquire)
    }

    /// Wait up to `timeout` for a live connection. Returns whether one is up.
    pub async fn connected_within(&self, timeout: Duration) -> bool {
        let deadline = tokio::time::Instant::now() + timeout;
        while !self.is_connected() {
            if tokio::time::Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        true
    }

    /// A fresh receiver for the engine's frame-descriptor fanout. Each subscribed
    /// plugin holds its own receiver; a slow consumer lags to the tail rather
    /// than blocking the reader. It survives reconnects. Mirrors
    /// [`crate::frame_link::FrameLink::subscribe`].
    pub fn subscribe_frames(&self) -> broadcast::Receiver<Vec<u8>> {
        self.shared.frames.subscribe()
    }

    /// A fresh receiver for the engine's detection-batch fanout. Same contract
    /// as [`Self::subscribe_frames`].
    pub fn subscribe_detections(&self) -> broadcast::Receiver<Vec<u8>> {
        self.shared.detections.subscribe()
    }

    /// Ask the ENGINE to push frame descriptors. Idempotent per connection: the
    /// flag is held across the round-trip and set only once the engine has
    /// accepted, so a refused attempt is retried by the next subscriber. Called
    /// while disconnected, it records the want and the push arms as soon as the
    /// engine connects.
    pub async fn arm_frame_push(&self) {
        self.shared.arm(Push::Frames).await;
    }

    /// Ask the ENGINE to push detection batches. Same arming contract as
    /// [`Self::arm_frame_push`].
    pub async fn arm_detection_push(&self) {
        self.shared.arm(Push::Detections).await;
    }

    /// Proxy a `register_model` request to the engine and return its response
    /// `args`.
    pub async fn register_model(&self, args: &Value) -> Result<Value, VisionRpcError> {
        self.shared
            .request(methods::REGISTER_MODEL, "vision.model.register", args)
            .await
    }

    /// Proxy an `infer` request to the engine and return its response `args`.
    pub async fn infer(&self, args: &Value) -> Result<Value, VisionRpcError> {
        self.shared
            .request(methods::INFER, "vision.model.register", args)
            .await
    }

    /// Proxy a `publish_detection` request to the engine and return its response
    /// `args`.
    pub async fn publish_detection(&self, args: &Value) -> Result<Value, VisionRpcError> {
        self.shared
            .request(methods::PUBLISH_DETECTION, "vision.detection.publish", args)
            .await
    }

    /// Proxy a `designate_track` request to the engine (set the follow target)
    /// and return its response `args`.
    pub async fn designate_track(&self, args: &Value) -> Result<Value, VisionRpcError> {
        self.shared
            .request(methods::DESIGNATE_TRACK, "vision.track.designate", args)
            .await
    }

    /// Proxy a `camera.control.set` request to the engine (exposure, gain or
    /// auto exposure on a camera it owns) and return its response `args`.
    pub async fn camera_control_set(&self, args: &Value) -> Result<Value, VisionRpcError> {
        use ados_protocol::camera_control::{CAMERA_CONTROL_CAPABILITY, CAMERA_CONTROL_METHOD};
        self.shared
            .request(CAMERA_CONTROL_METHOD, CAMERA_CONTROL_CAPABILITY, args)
            .await
    }

    /// Ask the engine to drop every model `plugin_id` registered. Host-internal:
    /// no plugin request reaches this method.
    pub async fn unregister_owner(&self, plugin_id: &str) -> Result<Value, VisionRpcError> {
        self.shared
            .request(
                ados_protocol::vision_rpc::UNREGISTER_OWNER,
                "",
                &ados_protocol::vision_rpc::unregister_owner_args(plugin_id),
            )
            .await
    }
}

impl Drop for VisionClient {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[cfg(test)]
mod tests;
