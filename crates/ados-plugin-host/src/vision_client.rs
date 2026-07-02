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
//! Both paths reuse the `ados-protocol` framing and envelope primitives; no
//! wire is re-implemented here.

use std::io;
use std::path::Path;
use std::time::Duration;

use ados_protocol::frame::{decode_len, HEADER_SIZE, PLUGIN_MAX_FRAME};
use ados_protocol::framebus::methods;
use ados_protocol::ipc::connect_with_retry;
use ados_protocol::plugin::{Envelope, PROTOCOL_VERSION};
use rmpv::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::unix::OwnedReadHalf;
use tokio::sync::{broadcast, Mutex};
use tokio::task::JoinHandle;

/// Frame-descriptor fanout depth. A descriptor is tiny (a few dozen bytes); the
/// pixels live in shared memory. Depth bounds how far a stalled subscriber may
/// fall behind before it lags to the tail, which is the right policy for a
/// latest-wins frame stream.
pub const VISION_FRAME_BROADCAST_DEPTH: usize = 256;

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

/// A live connection to the vision engine socket.
///
/// The engine's frame-descriptor pushes (`vision.deliver` event envelopes) fan
/// out on a broadcast channel, one receiver per subscribed plugin. Plugin
/// requests (`register_model` / `infer` / `publish_detection`) are written under
/// a connection mutex and matched to the engine's response by `request_id`.
pub struct VisionClient {
    /// Write half of the engine socket, serialized so concurrent requests do not
    /// interleave frames. The proxy writes a request then awaits its response on
    /// the shared response channel under this same lock, so requests are
    /// strictly one-at-a-time on the wire (the engine answers in order).
    request: Mutex<RequestChannel>,
    /// Frame-descriptor fanout: descriptor bytes pulled from the engine's
    /// `vision.deliver` pushes.
    frames: broadcast::Sender<Vec<u8>>,
    /// Detection-batch fanout: encoded `DetectionBatch` bytes pulled from the
    /// engine's `vision.deliver_detection` pushes.
    detections: broadcast::Sender<Vec<u8>>,
    reader: JoinHandle<()>,
}

/// The request-side half: the socket writer plus the response receiver the
/// reader task feeds. Held behind the connection mutex.
struct RequestChannel {
    write_half: tokio::net::unix::OwnedWriteHalf,
    responses: tokio::sync::mpsc::Receiver<Result<Value, String>>,
}

impl VisionClient {
    /// Connect to the engine socket, then spawn the reader that splits inbound
    /// envelopes into the frame-descriptor fanout (`vision.deliver` events) and
    /// the response channel (everything else). Mirrors the MAVLink client's
    /// connection setup: bounded connect-with-retry, then a read loop.
    pub async fn connect(sock_path: impl AsRef<Path>) -> io::Result<Self> {
        let stream = connect_with_retry(sock_path, 50, Duration::from_millis(20)).await?;
        let (read_half, write_half) = stream.into_split();

        let (frames, _rx) = broadcast::channel(VISION_FRAME_BROADCAST_DEPTH);
        let (detections, _drx) = broadcast::channel(VISION_FRAME_BROADCAST_DEPTH);
        let (resp_tx, resp_rx) = tokio::sync::mpsc::channel::<Result<Value, String>>(64);
        let frames_tx = frames.clone();
        let detections_tx = detections.clone();

        let reader = tokio::spawn(async move {
            read_loop(read_half, frames_tx, detections_tx, resp_tx).await;
        });

        Ok(Self {
            request: Mutex::new(RequestChannel {
                write_half,
                responses: resp_rx,
            }),
            frames,
            detections,
            reader,
        })
    }

    /// A fresh receiver for the engine's frame-descriptor fanout. Each subscribed
    /// plugin holds its own receiver; a slow consumer lags to the tail rather
    /// than blocking the reader. Mirrors [`crate::mavlink_client::MavlinkClient::subscribe`].
    pub fn subscribe_frames(&self) -> broadcast::Receiver<Vec<u8>> {
        self.frames.subscribe()
    }

    /// A fresh receiver for the engine's detection-batch fanout. Each subscribed
    /// plugin holds its own receiver; a slow consumer lags to the tail rather
    /// than blocking the reader. Mirrors [`Self::subscribe_frames`].
    pub fn subscribe_detections(&self) -> broadcast::Receiver<Vec<u8>> {
        self.detections.subscribe()
    }

    /// Proxy a `register_model` request to the engine and return its response
    /// `args`.
    pub async fn register_model(&self, args: &Value) -> Result<Value, VisionRpcError> {
        self.request(methods::REGISTER_MODEL, "vision.model.register", args)
            .await
    }

    /// Proxy an `infer` request to the engine and return its response `args`.
    pub async fn infer(&self, args: &Value) -> Result<Value, VisionRpcError> {
        self.request(methods::INFER, "vision.model.register", args)
            .await
    }

    /// Proxy a `publish_detection` request to the engine and return its response
    /// `args`.
    pub async fn publish_detection(&self, args: &Value) -> Result<Value, VisionRpcError> {
        self.request(methods::PUBLISH_DETECTION, "vision.detection.publish", args)
            .await
    }

    /// Proxy a `designate_track` request to the engine (set the follow target)
    /// and return its response `args`.
    pub async fn designate_track(&self, args: &Value) -> Result<Value, VisionRpcError> {
        self.request(methods::DESIGNATE_TRACK, "vision.track.designate", args)
            .await
    }

    /// Write one request envelope toward the engine and await the response. The
    /// connection mutex serializes the write+await so the engine's in-order
    /// responses match the requests. A transport failure or an engine `error`
    /// becomes a [`VisionRpcError`] the host surfaces to the plugin verbatim.
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

        let mut chan = self.request.lock().await;
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

impl Drop for VisionClient {
    fn drop(&mut self) {
        self.reader.abort();
    }
}

/// Drain the engine socket, routing each inbound envelope. `vision.deliver`
/// event envelopes carry a frame descriptor whose `descriptor` bytes are fanned
/// out on `frames`; every other envelope is a response and is forwarded on
/// `responses`. A clean EOF or a malformed/oversized header stops the loop and
/// closes both channels, so callers awaiting a response see the close and any
/// frame subscriber sees the channel end on its next recv.
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

        let client = VisionClient::connect(&path).await.unwrap();
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
        server.broadcast(deliver_envelope(&bytes)).await;

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

        let client = VisionClient::connect(&path).await.unwrap();
        let mut rx = client.subscribe_detections();
        tokio::time::sleep(Duration::from_millis(50)).await;

        let batch = DetectionBatch {
            v: VISION_DETECTION_VERSION,
            model_id: "m".into(),
            camera_id: "uvc-0".into(),
            frame_id: 1,
            ts_ms: 1,
            detections: vec![],
        };
        let bytes = batch.to_msgpack().unwrap();
        server.broadcast(deliver_detection_envelope(&bytes)).await;

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

        let client = VisionClient::connect(&path).await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;

        // The engine answers the next request with a fixed response.
        let result = Value::Map(vec![(Value::from("registered"), Value::Boolean(true))]);
        server.broadcast(response_envelope(result.clone())).await;

        let args = Value::Map(vec![(Value::from("model_id"), Value::from("m1"))]);
        let got = client.register_model(&args).await.unwrap();
        assert_eq!(got, result);
    }

    #[tokio::test]
    async fn engine_error_surfaces_as_rpc_error() {
        let path = temp_sock("error");
        let (server, _inbound) = IpcBroadcast::bind(&path, 256, false, None).await.unwrap();

        let client = VisionClient::connect(&path).await.unwrap();
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
            .broadcast(encode_frame(&body, PLUGIN_MAX_FRAME).unwrap())
            .await;

        let args = Value::Map(vec![(Value::from("model_id"), Value::from("missing"))]);
        let err = client.infer(&args).await.unwrap_err();
        assert_eq!(err, VisionRpcError("model not found".to_string()));
    }
}
