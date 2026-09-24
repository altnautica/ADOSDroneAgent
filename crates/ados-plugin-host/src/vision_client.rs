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

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use ados_protocol::frame::{decode_len, HEADER_SIZE, PLUGIN_MAX_FRAME};
use ados_protocol::framebus::methods;
use ados_protocol::plugin::{Envelope, PROTOCOL_VERSION};
use rmpv::{Value, ValueRef};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::unix::OwnedReadHalf;
use tokio::net::UnixStream;
use tokio::sync::{broadcast, mpsc, Mutex};
use tokio::task::JoinHandle;

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

impl Push {
    fn method(self) -> &'static str {
        match self {
            Self::Frames => methods::SUBSCRIBE_FRAMES,
            Self::Detections => methods::SUBSCRIBE_DETECTIONS,
        }
    }

    fn capability(self) -> &'static str {
        match self {
            Self::Frames => "vision.frame.read",
            Self::Detections => "vision.detection.subscribe",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Frames => "frame",
            Self::Detections => "detection",
        }
    }
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
}

impl Drop for VisionClient {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Shared {
    fn push_state(&self, push: Push) -> &Mutex<PushState> {
        match push {
            Push::Frames => &self.frame_push,
            Push::Detections => &self.detection_push,
        }
    }

    /// A plugin subscribed: record the want and arm the current connection.
    async fn arm(&self, push: Push) {
        let mut state = self.push_state(push).lock().await;
        state.wanted = true;
        self.arm_locked(push, &mut state).await;
    }

    /// A new connection is up: re-arm every push a plugin has asked for.
    async fn rearm(&self, push: Push) {
        let mut state = self.push_state(push).lock().await;
        if state.wanted {
            self.arm_locked(push, &mut state).await;
        }
    }

    async fn arm_locked(&self, push: Push, state: &mut PushState) {
        if state.armed {
            return;
        }
        if !self.connected.load(Ordering::Acquire) {
            tracing::debug!(
                push = push.label(),
                "vision engine not connected; push arms when it connects"
            );
            return;
        }
        match self
            .request(push.method(), push.capability(), &Value::Map(vec![]))
            .await
        {
            Ok(_) => {
                state.armed = true;
                tracing::info!(
                    push = push.label(),
                    "vision push armed on the engine connection"
                );
            }
            Err(e) => {
                tracing::warn!(push = push.label(), error = %e, "vision push could not be armed");
            }
        }
    }

    /// Install or clear the connection. Holds both push locks (lock order:
    /// push states, then `request`) so no arm can straddle the swap, and resets
    /// `armed`: the engine's push subscription is per connection.
    async fn set_connection(&self, conn: Option<RequestChannel>) {
        let mut frames = self.frame_push.lock().await;
        let mut detections = self.detection_push.lock().await;
        let mut request = self.request.lock().await;
        self.connected.store(conn.is_some(), Ordering::Release);
        *request = conn;
        frames.armed = false;
        detections.armed = false;
    }

    /// Write one request envelope toward the engine and await the response. The
    /// connection mutex serializes the write+await so the engine's in-order
    /// responses match the requests. No connection, a transport failure, or an
    /// engine `error` becomes a [`VisionRpcError`] the host surfaces to the
    /// plugin verbatim.
    async fn request(
        &self,
        method: &str,
        capability: &str,
        args: &Value,
    ) -> Result<Value, VisionRpcError> {
        let env = Envelope {
            version: PROTOCOL_VERSION,
            kind: "request".to_string(),
            method: method.to_string(),
            capability: capability.to_string(),
            args: args.clone(),
            request_id: format!("vis-{}", now_ms()),
            token: String::new(),
            error: None,
        };
        let frame = env
            .encode_frame()
            .map_err(|e| VisionRpcError(format!("encode failed: {e}")))?;

        let mut guard = self.request.lock().await;
        let Some(chan) = guard.as_mut() else {
            return Err(VisionRpcError(VISION_ENGINE_UNAVAILABLE.to_string()));
        };
        chan.write_half
            .write_all(&frame)
            .await
            .map_err(|e| VisionRpcError(format!("vision engine unavailable: {e}")))?;
        chan.write_half
            .flush()
            .await
            .map_err(|e| VisionRpcError(format!("vision engine unavailable: {e}")))?;
        match chan.responses.recv().await {
            Some(Ok(args)) => Ok(args),
            Some(Err(msg)) => Err(VisionRpcError(msg)),
            None => Err(VisionRpcError("vision engine closed".to_string())),
        }
    }
}

/// The connection task: connect, serve until the engine goes away, clear the
/// connection, wait the fixed interval, repeat. Forever.
async fn run(shared: Arc<Shared>, path: PathBuf, interval: Duration) {
    // One line per outage, not one per attempt.
    let mut reported_absent = false;
    loop {
        match UnixStream::connect(&path).await {
            Ok(stream) => {
                reported_absent = false;
                let (read_half, write_half) = stream.into_split();
                let (resp_tx, resp_rx) = mpsc::channel::<Result<Value, String>>(64);
                shared
                    .set_connection(Some(RequestChannel {
                        write_half,
                        responses: resp_rx,
                    }))
                    .await;
                tracing::info!(path = %path.display(), "vision engine connected");

                // The reader and the re-arm run together: the re-arm's
                // subscribe requests need the reader to deliver their responses.
                // The re-arm then parks, so only the reader ends the select.
                let reader = read_loop(
                    read_half,
                    shared.frames.clone(),
                    shared.detections.clone(),
                    resp_tx,
                );
                let rearm = async {
                    shared.rearm(Push::Frames).await;
                    shared.rearm(Push::Detections).await;
                    std::future::pending::<()>().await
                };
                tokio::select! {
                    () = reader => {}
                    () = rearm => {}
                }

                shared.set_connection(None).await;
                tracing::warn!(
                    path = %path.display(),
                    "vision engine connection lost; reconnecting"
                );
            }
            Err(e) if !reported_absent => {
                reported_absent = true;
                tracing::warn!(
                    path = %path.display(),
                    error = %e,
                    "vision engine socket unavailable; retrying until it appears"
                );
            }
            Err(e) => {
                tracing::debug!(path = %path.display(), error = %e, "vision engine socket unavailable");
            }
        }
        tokio::time::sleep(interval).await;
    }
}

/// Drain the engine socket, routing each inbound envelope. `vision.deliver`
/// event envelopes carry a frame descriptor whose `descriptor` bytes are fanned
/// out on `frames`; every other envelope is a response and is forwarded on
/// `responses`. A clean EOF or a malformed/oversized header stops the loop and
/// drops `responses`, so a caller awaiting a response sees the close. The
/// fanouts stay open: subscribers resume on the next connection.
///
/// A push whose fanout has no receivers is dropped without being decoded; see
/// [`peek_push_kind`].
async fn read_loop(
    mut read_half: OwnedReadHalf,
    frames: broadcast::Sender<Vec<u8>>,
    detections: broadcast::Sender<Vec<u8>>,
    responses: tokio::sync::mpsc::Sender<Result<Value, String>>,
) {
    loop {
        let mut header = [0u8; HEADER_SIZE];
        if read_half.read_exact(&mut header).await.is_err() {
            break;
        }
        let len = match decode_len(header, PLUGIN_MAX_FRAME, true) {
            Ok(n) => n,
            Err(_) => break,
        };
        let mut body = vec![0u8; len];
        if read_half.read_exact(&mut body).await.is_err() {
            break;
        }
        // Nobody holds a receiver on the fanout this push would feed: drop it
        // before the decode. Arming is process-lifetime and there is no engine
        // `vision.unsubscribe_frames`, so one short-lived plugin that
        // subscribed once would otherwise leave the host decoding every
        // camera's descriptors at capture rate for the rest of the process,
        // allocating the payload twice per frame for a `send` that can only
        // return `Err`. The probe is skipped entirely while both fanouts have
        // subscribers, so the fully-subscribed path pays nothing for it.
        if frames.receiver_count() == 0 || detections.receiver_count() == 0 {
            match peek_push_kind(&body) {
                PushKind::Frame if frames.receiver_count() == 0 => continue,
                PushKind::Detection if detections.receiver_count() == 0 => continue,
                _ => {}
            }
        }
        let env = match Envelope::from_msgpack(&body) {
            Ok(env) => env,
            Err(_) => break,
        };
        if env.method == methods::DELIVER_FRAME {
            if let Some(descriptor) = frame_descriptor_bytes(&env.args) {
                // A send with no receivers returns Err; that is fine, the next
                // subscriber resumes at the tail.
                let _ = frames.send(descriptor);
            }
            continue;
        }
        if env.method == methods::DELIVER_DETECTION {
            if let Some(batch) = detection_batch_bytes(&env.args) {
                let _ = detections.send(batch);
            }
            continue;
        }
        // A response: forward the error if set, else the args map. If the
        // receiver is gone the requester already moved on, so stop.
        let payload = match env.error {
            Some(msg) => Err(msg),
            None => Ok(env.args),
        };
        if responses.send(payload).await.is_err() {
            break;
        }
    }
}

/// Which of the engine's two push events an envelope body is, if either.
enum PushKind {
    Frame,
    Detection,
    Other,
}

/// Classify an envelope body by its `method` field without decoding it.
///
/// `rmpv`'s borrowing reader walks the bytes and copies no payload, so a push
/// the host is about to drop costs one structural walk instead of a full
/// [`Envelope`] — which allocates the binary `descriptor` once in `args` and
/// again in the extraction below. Anything that is not recognisably one of the
/// two push methods is [`PushKind::Other`] and takes the normal decode path, so
/// a response is never misrouted by a partial read.
fn peek_push_kind(body: &[u8]) -> PushKind {
    let Ok(ValueRef::Map(entries)) = rmpv::decode::read_value_ref(&mut &body[..]) else {
        return PushKind::Other;
    };
    for (key, value) in entries {
        let ValueRef::String(key) = key else { continue };
        if key.as_str() != Some("method") {
            continue;
        }
        let ValueRef::String(method) = value else {
            return PushKind::Other;
        };
        return match method.as_str() {
            Some(methods::DELIVER_FRAME) => PushKind::Frame,
            Some(methods::DELIVER_DETECTION) => PushKind::Detection,
            _ => PushKind::Other,
        };
    }
    PushKind::Other
}

/// Extract the frame-descriptor bytes from a `vision.deliver` envelope. The
/// engine carries the encoded [`ados_protocol::framebus::FrameDescriptor`] as a
/// binary `descriptor` field; the host forwards those bytes unchanged to the
/// plugin. Returns `None` if the field is absent or not binary.
fn frame_descriptor_bytes(args: &Value) -> Option<Vec<u8>> {
    match args {
        Value::Map(entries) => entries
            .iter()
            .find(|(k, _)| k.as_str() == Some("descriptor"))
            .and_then(|(_, v)| match v {
                Value::Binary(b) => Some(b.clone()),
                _ => None,
            }),
        _ => None,
    }
}

/// Extract the detection-batch bytes from a `vision.deliver_detection` envelope.
/// The engine carries the encoded [`ados_protocol::framebus::DetectionBatch`] as
/// a binary `batch` field; the host forwards those bytes unchanged to the
/// plugin. Returns `None` if the field is absent or not binary.
fn detection_batch_bytes(args: &Value) -> Option<Vec<u8>> {
    match args {
        Value::Map(entries) => entries
            .iter()
            .find(|(k, _)| k.as_str() == Some("batch"))
            .and_then(|(_, v)| match v {
                Value::Binary(b) => Some(b.clone()),
                _ => None,
            }),
        _ => None,
    }
}

/// Wall-clock unix milliseconds, used to tag each request id.
fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ados_protocol::frame::encode_frame;
    use ados_protocol::framebus::FrameDescriptor;
    use ados_protocol::framebus::FrameFormat;
    use ados_protocol::framebus::FRAMEBUS_DESCRIPTOR_VERSION;
    use ados_protocol::ipc::IpcBroadcast;

    fn temp_sock(name: &str) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "ados-visclient-test-{}-{}.sock",
            std::process::id(),
            name
        ));
        let _ = std::fs::remove_file(&p);
        p
    }

    /// A client on a fast reconnect interval, once its first connection is up.
    async fn connected_client(path: &Path) -> VisionClient {
        let client =
            VisionClient::spawn_with_interval(path.to_path_buf(), Duration::from_millis(50));
        assert!(client.connected_within(Duration::from_secs(2)).await);
        client
    }

    /// A fake engine that answers every host request with an empty success
    /// response and reports each request's method on the returned channel.
    fn answering_engine(
        server: std::sync::Arc<IpcBroadcast>,
        mut inbound: tokio::sync::mpsc::Receiver<ados_protocol::ipc::InboundCommand>,
    ) -> (
        tokio::task::JoinHandle<()>,
        tokio::sync::mpsc::UnboundedReceiver<String>,
    ) {
        let (seen_tx, seen_rx) = tokio::sync::mpsc::unbounded_channel();
        let engine = tokio::spawn(async move {
            while let Some(cmd) = inbound.recv().await {
                let req = Envelope::from_msgpack(&cmd.payload).expect("a request envelope");
                let _ = seen_tx.send(req.method.clone());
                server
                    .broadcast(response_envelope(Value::Map(vec![])).into())
                    .await;
            }
        });
        (engine, seen_rx)
    }

    fn sample_descriptor(frame_id: u64) -> FrameDescriptor {
        FrameDescriptor {
            v: FRAMEBUS_DESCRIPTOR_VERSION,
            camera_id: "uvc-0".into(),
            frame_id,
            ts_ms: 1,
            width: 64,
            height: 48,
            format: FrameFormat::Rgb24,
            shm_name: "ados-vision-uvc-0".into(),
            slot: 0,
            seq: frame_id,
            byte_len: (64 * 48 * 3) as u32,
        }
    }

    async fn next_method(seen: &mut tokio::sync::mpsc::UnboundedReceiver<String>) -> String {
        tokio::time::timeout(Duration::from_secs(2), seen.recv())
            .await
            .expect("a host request within timeout")
            .expect("engine task alive")
    }

    #[tokio::test]
    async fn an_engine_that_starts_after_the_host_is_picked_up() {
        let path = temp_sock("late");
        let client = VisionClient::spawn_with_interval(path.clone(), Duration::from_millis(50));
        let mut rx = client.subscribe_frames();

        // No engine yet: a request is a transient refusal, and a plugin
        // subscribe records the want instead of failing it.
        let err = client
            .register_model(&Value::Map(vec![]))
            .await
            .unwrap_err();
        assert_eq!(err, VisionRpcError(VISION_ENGINE_UNAVAILABLE.to_string()));
        client.arm_frame_push().await;

        let (server, inbound) = IpcBroadcast::bind(&path, 256, false, Some(16))
            .await
            .unwrap();
        let server = std::sync::Arc::new(server);
        let (_engine, mut seen) = answering_engine(server.clone(), inbound.unwrap());
        assert!(client.connected_within(Duration::from_secs(2)).await);

        // The subscribe made while down is armed on the new connection.
        assert_eq!(next_method(&mut seen).await, methods::SUBSCRIBE_FRAMES);
        let descriptor = sample_descriptor(1);
        server
            .broadcast(deliver_envelope(&descriptor.to_msgpack().unwrap()).into())
            .await;
        let got = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("descriptor within timeout")
            .expect("descriptor, not lagged/closed");
        assert_eq!(FrameDescriptor::from_msgpack(&got).unwrap(), descriptor);

        client.register_model(&Value::Map(vec![])).await.unwrap();
        assert_eq!(next_method(&mut seen).await, methods::REGISTER_MODEL);
    }

    #[tokio::test]
    async fn an_engine_restart_reconnects_and_rearms_the_same_subscriber() {
        let path = temp_sock("restart");
        let (first, inbound) = IpcBroadcast::bind(&path, 256, false, Some(16))
            .await
            .unwrap();
        let first = std::sync::Arc::new(first);
        let (first_engine, mut seen) = answering_engine(first.clone(), inbound.unwrap());
        let client = connected_client(&path).await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        let mut rx = client.subscribe_frames();
        client.arm_frame_push().await;
        assert_eq!(next_method(&mut seen).await, methods::SUBSCRIBE_FRAMES);

        // The engine goes away (its answering task holds the other handle).
        first_engine.abort();
        let _ = first_engine.await;
        drop(first);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while client.is_connected() && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(!client.is_connected(), "the lost engine must be noticed");
        let err = client.infer(&Value::Map(vec![])).await.unwrap_err();
        assert_eq!(err, VisionRpcError(VISION_ENGINE_UNAVAILABLE.to_string()));

        // It comes back: the client reconnects and re-arms the push by itself,
        // and the plugin's original receiver keeps receiving.
        let (second, inbound) = IpcBroadcast::bind(&path, 256, false, Some(16))
            .await
            .unwrap();
        let second = std::sync::Arc::new(second);
        let (_engine, mut seen) = answering_engine(second.clone(), inbound.unwrap());
        assert!(client.connected_within(Duration::from_secs(2)).await);
        assert_eq!(next_method(&mut seen).await, methods::SUBSCRIBE_FRAMES);

        let descriptor = sample_descriptor(2);
        second
            .broadcast(deliver_envelope(&descriptor.to_msgpack().unwrap()).into())
            .await;
        let got = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("descriptor within timeout")
            .expect("descriptor, not lagged/closed");
        assert_eq!(FrameDescriptor::from_msgpack(&got).unwrap(), descriptor);
    }

    fn deliver_envelope(descriptor: &[u8]) -> Vec<u8> {
        let env = Envelope {
            version: PROTOCOL_VERSION,
            kind: "event".to_string(),
            method: methods::DELIVER_FRAME.to_string(),
            capability: "vision.frame.read".to_string(),
            args: Value::Map(vec![(
                Value::from("descriptor"),
                Value::Binary(descriptor.to_vec()),
            )]),
            request_id: "vis-frame-1".to_string(),
            token: String::new(),
            error: None,
        };
        let body = env.to_msgpack().unwrap();
        encode_frame(&body, PLUGIN_MAX_FRAME).unwrap()
    }

    fn response_envelope(request_kind: Value) -> Vec<u8> {
        let env = Envelope {
            version: PROTOCOL_VERSION,
            kind: "response".to_string(),
            method: "response".to_string(),
            capability: String::new(),
            args: request_kind,
            request_id: "vis-1".to_string(),
            token: String::new(),
            error: None,
        };
        let body = env.to_msgpack().unwrap();
        encode_frame(&body, PLUGIN_MAX_FRAME).unwrap()
    }

    #[tokio::test]
    async fn frame_descriptors_fan_out_to_a_subscriber() {
        let path = temp_sock("fanout");
        // The engine side: a broadcast socket the host connects to. The plugin
        // contract rejects zero-length frames, so reject_zero is true.
        let (server, _inbound) = IpcBroadcast::bind(&path, 256, false, None).await.unwrap();

        let client = connected_client(&path).await;
        let mut rx = client.subscribe_frames();
        tokio::time::sleep(Duration::from_millis(50)).await;

        let descriptor = FrameDescriptor {
            v: FRAMEBUS_DESCRIPTOR_VERSION,
            camera_id: "uvc-0".into(),
            frame_id: 1,
            ts_ms: 1,
            width: 64,
            height: 48,
            format: FrameFormat::Rgb24,
            shm_name: "ados-vision-uvc-0".into(),
            slot: 0,
            seq: 1,
            byte_len: (64 * 48 * 3) as u32,
        };
        let bytes = descriptor.to_msgpack().unwrap();
        server.broadcast(deliver_envelope(&bytes).into()).await;

        let got = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("descriptor within timeout")
            .expect("descriptor, not lagged/closed");
        assert_eq!(FrameDescriptor::from_msgpack(&got).unwrap(), descriptor);
    }

    fn deliver_detection_envelope(batch: &[u8]) -> Vec<u8> {
        let env = Envelope {
            version: PROTOCOL_VERSION,
            kind: "event".to_string(),
            method: methods::DELIVER_DETECTION.to_string(),
            capability: "vision.detection.subscribe".to_string(),
            args: Value::Map(vec![(Value::from("batch"), Value::Binary(batch.to_vec()))]),
            request_id: "vis-det-uvc-0-1".to_string(),
            token: String::new(),
            error: None,
        };
        let body = env.to_msgpack().unwrap();
        encode_frame(&body, PLUGIN_MAX_FRAME).unwrap()
    }

    #[tokio::test]
    async fn detection_batches_fan_out_to_a_subscriber() {
        use ados_protocol::framebus::{DetectionBatch, VISION_DETECTION_VERSION};
        let path = temp_sock("det-fanout");
        let (server, _inbound) = IpcBroadcast::bind(&path, 256, false, None).await.unwrap();

        let client = connected_client(&path).await;
        let mut rx = client.subscribe_detections();
        tokio::time::sleep(Duration::from_millis(50)).await;

        let batch = DetectionBatch {
            v: VISION_DETECTION_VERSION,
            model_id: "m".into(),
            camera_id: "uvc-0".into(),
            frame_id: 1,
            ts_ms: 1,
            frame_width: 640,
            frame_height: 480,
            detections: vec![],
        };
        let bytes = batch.to_msgpack().unwrap();
        server
            .broadcast(deliver_detection_envelope(&bytes).into())
            .await;

        let got = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("batch within timeout")
            .expect("batch, not lagged/closed");
        assert_eq!(DetectionBatch::from_msgpack(&got).unwrap(), batch);
    }

    #[tokio::test]
    async fn request_returns_the_engine_response_args() {
        let path = temp_sock("request");
        let (server, _inbound) = IpcBroadcast::bind(&path, 256, false, None).await.unwrap();

        let client = connected_client(&path).await;
        tokio::time::sleep(Duration::from_millis(50)).await;

        // The engine answers the next request with a fixed response.
        let result = Value::Map(vec![(Value::from("registered"), Value::Boolean(true))]);
        server
            .broadcast(response_envelope(result.clone()).into())
            .await;

        let args = Value::Map(vec![(Value::from("model_id"), Value::from("m1"))]);
        let got = client.register_model(&args).await.unwrap();
        assert_eq!(got, result);
    }

    #[tokio::test]
    async fn engine_error_surfaces_as_rpc_error() {
        let path = temp_sock("error");
        let (server, _inbound) = IpcBroadcast::bind(&path, 256, false, None).await.unwrap();

        let client = connected_client(&path).await;
        tokio::time::sleep(Duration::from_millis(50)).await;

        let env = Envelope {
            version: PROTOCOL_VERSION,
            kind: "response".to_string(),
            method: "response".to_string(),
            capability: String::new(),
            args: Value::Map(vec![]),
            request_id: "vis-1".to_string(),
            token: String::new(),
            error: Some("model not found".to_string()),
        };
        let body = env.to_msgpack().unwrap();
        server
            .broadcast(encode_frame(&body, PLUGIN_MAX_FRAME).unwrap().into())
            .await;

        let args = Value::Map(vec![(Value::from("model_id"), Value::from("missing"))]);
        let err = client.infer(&args).await.unwrap_err();
        assert_eq!(err, VisionRpcError("model not found".to_string()));
    }

    #[tokio::test]
    async fn a_refused_arm_is_retried_by_the_concurrent_second_subscriber() {
        let path = temp_sock("arm-race");
        // `Some(16)` hands the fake engine the host's own requests, so the test
        // can count how many subscribe attempts actually reached the wire.
        let (server, inbound) = IpcBroadcast::bind(&path, 256, false, Some(16))
            .await
            .unwrap();
        let mut inbound = inbound.expect("inbound channel requested");
        let server = std::sync::Arc::new(server);

        let client = connected_client(&path).await;
        tokio::time::sleep(Duration::from_millis(50)).await;

        // The engine refuses the first subscribe and accepts the second.
        let engine_server = server.clone();
        let engine = tokio::spawn(async move {
            let mut seen = Vec::new();
            for attempt in 0..2 {
                let cmd = inbound.recv().await.expect("a host request");
                let req = Envelope::from_msgpack(&cmd.payload).expect("a request envelope");
                seen.push(req.method.clone());
                let reply = Envelope {
                    version: PROTOCOL_VERSION,
                    kind: "response".to_string(),
                    method: "response".to_string(),
                    capability: String::new(),
                    args: Value::Map(vec![]),
                    request_id: req.request_id,
                    token: String::new(),
                    error: (attempt == 0).then(|| "engine busy".to_string()),
                };
                let body = reply.to_msgpack().unwrap();
                engine_server
                    .broadcast(encode_frame(&body, PLUGIN_MAX_FRAME).unwrap().into())
                    .await;
            }
            seen
        });

        // Two plugins subscribing at host startup, the normal case. A
        // test-and-set flag let the second one return on the first one's
        // in-flight `true` and then be stranded when that attempt failed.
        tokio::join!(client.arm_frame_push(), client.arm_frame_push());

        let seen = tokio::time::timeout(Duration::from_secs(2), engine)
            .await
            .expect("the engine answered both attempts")
            .unwrap();
        assert_eq!(
            seen,
            vec![
                methods::SUBSCRIBE_FRAMES.to_string(),
                methods::SUBSCRIBE_FRAMES.to_string()
            ],
            "the refused arm must be retried by the second subscriber"
        );

        // Armed now, so a third subscriber returns without another round-trip —
        // there is no engine task left to answer one.
        tokio::time::timeout(Duration::from_millis(500), client.arm_frame_push())
            .await
            .expect("an armed connection does not re-request");
    }

    #[tokio::test]
    async fn a_frame_push_with_no_subscriber_is_dropped_before_the_decode() {
        let path = temp_sock("idle-fanout");
        let (server, _inbound) = IpcBroadcast::bind(&path, 256, false, None).await.unwrap();

        let client = connected_client(&path).await;
        tokio::time::sleep(Duration::from_millis(50)).await;

        // A push arriving while no plugin holds a receiver, then a response.
        // The reader must stay on the wire: dropping the push must not consume
        // or reorder the response behind it.
        server
            .broadcast(deliver_envelope(b"not-a-descriptor").into())
            .await;
        let result = Value::Map(vec![(Value::from("registered"), Value::Boolean(true))]);
        server
            .broadcast(response_envelope(result.clone()).into())
            .await;

        let args = Value::Map(vec![(Value::from("model_id"), Value::from("m1"))]);
        let got = client.register_model(&args).await.unwrap();
        assert_eq!(got, result);
    }
}
