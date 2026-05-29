//! The `/run/ados/vision.sock` request/response server.
//!
//! The engine serves this socket; the plugin host is the only client. It speaks
//! the same wire as the plugin RPC socket: 4-byte big-endian length-prefixed
//! msgpack [`ados_protocol::plugin::Envelope`] frames (zero-length rejected).
//! The host has already gated each call on the matching vision capability before
//! it reaches this socket, so the server does not re-check tokens; being on the
//! socket is the authorization.
//!
//! Request methods (the [`ados_protocol::framebus::methods`] constants carried
//! in `Envelope::method`):
//!
//! - `vision.subscribe_frames` — start streaming frame descriptors to this
//!   connection. Every published descriptor is pushed as a `vision.deliver`
//!   event envelope whose `args` map carries the encoded descriptor as a binary
//!   `descriptor` field (the host fans these out to subscribed plugins).
//! - `vision.register_model` — register a model; `args` is the msgpack
//!   [`ModelMetadata`] map.
//! - `vision.infer` — run a registered engine-run model against one frame; the
//!   frame is named by `{shm_name, slot, seq}` (read from the ring) or carried
//!   inline as a binary `frame` field plus `{width, height, format}`.
//! - `vision.publish_detection` — publish a [`DetectionBatch`] (`args` is the
//!   batch map). Used by plugin-side models.
//!
//! Each request gets one response envelope sharing the request's `request_id`;
//! an error sets the envelope `error` field, which the host surfaces to the
//! plugin verbatim.

use std::path::Path;
use std::sync::Arc;

use ados_protocol::frame::{decode_len, HEADER_SIZE, PLUGIN_MAX_FRAME};
use ados_protocol::framebus::{
    methods, DetectionBatch, FrameDescriptor, FrameFormat, ModelMetadata,
};
use ados_protocol::plugin::{Envelope, PROTOCOL_VERSION};
use anyhow::{anyhow, Result};
use rmpv::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};

use crate::engine::VisionEngine;

/// Bind `vision.sock` and serve clients until `cancel` is notified.
pub async fn serve(
    engine: Arc<VisionEngine>,
    socket_path: &str,
    cancel: Arc<tokio::sync::Notify>,
) -> Result<()> {
    if let Some(parent) = Path::new(socket_path).parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let _ = std::fs::remove_file(socket_path);
    let listener = UnixListener::bind(socket_path)?;
    // The host connects as a peer; 0o660 keeps the socket off the world while
    // still reachable by the agent group (matches the plugin socket policy).
    set_socket_perms(socket_path);
    tracing::info!(path = %socket_path, "vision_sock_listening");

    loop {
        tokio::select! {
            accepted = listener.accept() => {
                match accepted {
                    Ok((stream, _addr)) => {
                        let engine = engine.clone();
                        let cancel = cancel.clone();
                        tokio::spawn(async move {
                            if let Err(e) = handle_client(engine, stream, cancel).await {
                                tracing::debug!(error = %e, "vision_sock_client_ended");
                            }
                        });
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "vision_sock_accept_failed");
                        break;
                    }
                }
            }
            _ = cancel.notified() => break,
        }
    }
    let _ = std::fs::remove_file(socket_path);
    Ok(())
}

#[cfg(target_os = "linux")]
fn set_socket_perms(path: &str) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o660));
}

#[cfg(not(target_os = "linux"))]
fn set_socket_perms(_path: &str) {}

/// Serve one client connection. Reads request envelopes, dispatches each, and
/// writes the response. A `subscribe_frames` request additionally spawns a push
/// task that streams descriptors on the same connection for its lifetime.
async fn handle_client(
    engine: Arc<VisionEngine>,
    stream: UnixStream,
    cancel: Arc<tokio::sync::Notify>,
) -> Result<()> {
    let (mut read_half, write_half) = stream.into_split();
    // The writer is shared between the request-response path and the frame push
    // task, so both serialize their frames through one mutex.
    let writer = Arc::new(tokio::sync::Mutex::new(write_half));
    let mut frame_task: Option<tokio::task::JoinHandle<()>> = None;

    loop {
        let mut header = [0u8; HEADER_SIZE];
        tokio::select! {
            r = read_half.read_exact(&mut header) => {
                if r.is_err() {
                    break;
                }
            }
            _ = cancel.notified() => break,
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
            Ok(e) => e,
            Err(_) => break,
        };

        if env.method == methods::SUBSCRIBE_FRAMES {
            // Acknowledge, then start (or restart) the descriptor push task.
            send_response(&writer, &env.request_id, ok_map(&[("subscribed", Value::Boolean(true))]), None).await?;
            if let Some(t) = frame_task.take() {
                t.abort();
            }
            frame_task = Some(spawn_frame_push(engine.clone(), writer.clone(), filter_camera(&env.args)));
            continue;
        }

        let (args, err) = dispatch(&engine, &env).await;
        send_response(&writer, &env.request_id, args, err).await?;
    }

    if let Some(t) = frame_task.take() {
        t.abort();
    }
    Ok(())
}

/// Run the request method and return `(response_args, optional_error)`.
async fn dispatch(engine: &Arc<VisionEngine>, env: &Envelope) -> (Value, Option<String>) {
    let result = match env.method.as_str() {
        m if m == methods::REGISTER_MODEL => handle_register(engine, &env.args).await,
        m if m == methods::INFER => handle_infer(engine, &env.args).await,
        m if m == methods::PUBLISH_DETECTION => handle_publish(engine, &env.args).await,
        other => Err(anyhow!("unknown vision method {other}")),
    };
    match result {
        Ok(args) => (args, None),
        Err(e) => (Value::Map(Vec::new()), Some(e.to_string())),
    }
}

async fn handle_register(engine: &Arc<VisionEngine>, args: &Value) -> Result<Value> {
    let meta: ModelMetadata = decode_args(args)?;
    let model_id = meta.id.clone();
    let (exec, had_backend) = engine.register_model(meta).await?;
    Ok(ok_map(&[
        ("registered", Value::Boolean(true)),
        ("model_id", Value::from(model_id)),
        ("execution", Value::from(execution_str(exec))),
        ("backend_loaded", Value::Boolean(had_backend)),
    ]))
}

async fn handle_infer(engine: &Arc<VisionEngine>, args: &Value) -> Result<Value> {
    let req: InferRequest = decode_args(args)?;
    let (frame, width, height, format) = req.resolve_frame()?;
    let detections = engine
        .infer(&req.model_id, &frame, width, height, format)
        .await?;
    let batch = DetectionBatch {
        model_id: req.model_id.clone(),
        camera_id: req.camera_id.clone().unwrap_or_default(),
        frame_id: req.frame_id.unwrap_or(0),
        ts_ms: req.ts_ms.unwrap_or(0),
        detections,
    };
    // Encode the batch as a binary field so the host returns it unchanged.
    let bytes = batch
        .to_msgpack()
        .map_err(|e| anyhow!("encode detection batch: {e}"))?;
    Ok(ok_map(&[
        ("model_id", Value::from(req.model_id)),
        ("batch", Value::Binary(bytes)),
    ]))
}

async fn handle_publish(engine: &Arc<VisionEngine>, args: &Value) -> Result<Value> {
    let batch: DetectionBatch = decode_args(args)?;
    let reached = engine.publish_detection(batch);
    Ok(ok_map(&[("subscribers", Value::from(reached as u64))]))
}

/// Spawn the per-connection frame-descriptor push task. Every published
/// descriptor (optionally filtered to one camera) is wrapped in a
/// `vision.deliver` event envelope and written to the connection. A lagged
/// subscriber skips to the tail (latest-wins); a write error ends the task.
fn spawn_frame_push(
    engine: Arc<VisionEngine>,
    writer: Arc<tokio::sync::Mutex<tokio::net::unix::OwnedWriteHalf>>,
    camera_filter: Option<String>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut rx = engine.subscribe_frames();
        loop {
            match rx.recv().await {
                Ok(desc) => {
                    if let Some(want) = &camera_filter {
                        if &desc.camera_id != want {
                            continue;
                        }
                    }
                    let frame = match deliver_frame(&desc) {
                        Ok(f) => f,
                        Err(_) => continue,
                    };
                    let mut w = writer.lock().await;
                    if w.write_all(&frame).await.is_err() || w.flush().await.is_err() {
                        break;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    })
}

/// Build a `vision.deliver` event frame carrying the descriptor as a binary
/// `descriptor` field, ready to write.
fn deliver_frame(desc: &FrameDescriptor) -> Result<Vec<u8>> {
    let bytes = desc
        .to_msgpack()
        .map_err(|e| anyhow!("encode descriptor: {e}"))?;
    let env = Envelope {
        version: PROTOCOL_VERSION,
        kind: "event".to_string(),
        method: methods::DELIVER_FRAME.to_string(),
        capability: "vision.frame.read".to_string(),
        args: Value::Map(vec![(Value::from("descriptor"), Value::Binary(bytes))]),
        request_id: format!("vis-frame-{}", desc.seq),
        token: String::new(),
        error: None,
    };
    env.encode_frame()
        .map_err(|e| anyhow!("encode deliver envelope: {e}"))
}

/// Write a response envelope sharing `request_id`. An `error` sets the envelope
/// error field and an empty args map.
async fn send_response(
    writer: &Arc<tokio::sync::Mutex<tokio::net::unix::OwnedWriteHalf>>,
    request_id: &str,
    args: Value,
    error: Option<String>,
) -> Result<()> {
    let env = Envelope {
        version: PROTOCOL_VERSION,
        kind: "response".to_string(),
        method: "response".to_string(),
        capability: String::new(),
        args: if error.is_some() { Value::Map(Vec::new()) } else { args },
        request_id: request_id.to_string(),
        token: String::new(),
        error,
    };
    let frame = env
        .encode_frame()
        .map_err(|e| anyhow!("encode response: {e}"))?;
    let mut w = writer.lock().await;
    w.write_all(&frame).await?;
    w.flush().await?;
    Ok(())
}

/// The optional `camera_id` filter on a `subscribe_frames` request.
fn filter_camera(args: &Value) -> Option<String> {
    match args {
        Value::Map(entries) => entries
            .iter()
            .find(|(k, _)| k.as_str() == Some("camera_id"))
            .and_then(|(_, v)| v.as_str().map(str::to_owned)),
        _ => None,
    }
}

/// Build a response args map from string-keyed pairs.
fn ok_map(pairs: &[(&str, Value)]) -> Value {
    Value::Map(
        pairs
            .iter()
            .map(|(k, v)| (Value::from(*k), v.clone()))
            .collect(),
    )
}

/// The wire string for a model execution kind.
fn execution_str(e: ados_protocol::framebus::ModelExecution) -> &'static str {
    use ados_protocol::framebus::ModelExecution::*;
    match e {
        EngineRun => "engine_run",
        PluginSide => "plugin_side",
    }
}

/// Decode an rmpv args map into a typed struct via msgpack round-trip. The args
/// arrive as an `rmpv::Value`; re-encode and decode into `T` so the same named
/// fields the contract uses bind directly.
fn decode_args<T: serde::de::DeserializeOwned>(args: &Value) -> Result<T> {
    let bytes = rmp_serde::to_vec_named(args).map_err(|e| anyhow!("re-encode args: {e}"))?;
    rmp_serde::from_slice(&bytes).map_err(|e| anyhow!("decode args: {e}"))
}

/// An `infer` request. The frame is either named in the shared ring
/// (`shm_name` + `slot` + `seq`) or carried inline as a binary `frame` field
/// with explicit dimensions.
#[derive(Debug, serde::Deserialize)]
struct InferRequest {
    model_id: String,
    #[serde(default)]
    camera_id: Option<String>,
    #[serde(default)]
    frame_id: Option<u64>,
    #[serde(default)]
    ts_ms: Option<i64>,
    // Ring-named frame.
    #[serde(default)]
    shm_name: Option<String>,
    #[serde(default)]
    slot: Option<u32>,
    #[serde(default)]
    seq: Option<u64>,
    // Inline frame.
    #[serde(default)]
    frame: Option<serde_bytes_compat::Bytes>,
    #[serde(default)]
    width: Option<u32>,
    #[serde(default)]
    height: Option<u32>,
    #[serde(default)]
    format: Option<FrameFormat>,
}

impl InferRequest {
    /// Resolve the frame bytes plus dimensions/format. The width, height, and
    /// format always come from the request (the host knows them from the
    /// descriptor it holds); the bytes come either from an inline binary `frame`
    /// field or from the named ring slot (mapped read-only, seqlock-validated).
    /// A torn or stale ring read is an error so the caller retries with a fresh
    /// descriptor.
    fn resolve_frame(&self) -> Result<(Vec<u8>, u32, u32, FrameFormat)> {
        let width = self.width.ok_or_else(|| anyhow!("infer needs width"))?;
        let height = self.height.ok_or_else(|| anyhow!("infer needs height"))?;
        let format = self.format.ok_or_else(|| anyhow!("infer needs format"))?;
        if let Some(bytes) = &self.frame {
            return Ok((bytes.0.clone(), width, height, format));
        }
        let shm_name = self
            .shm_name
            .as_deref()
            .ok_or_else(|| anyhow!("infer needs a shm_name or an inline frame"))?;
        let slot = self.slot.ok_or_else(|| anyhow!("ring frame needs slot"))?;
        let seq = self.seq.ok_or_else(|| anyhow!("ring frame needs seq"))?;
        let bytes = read_ring_frame(shm_name, slot, seq)?;
        Ok((bytes, width, height, format))
    }
}

/// Map a `/dev/shm` ring read-only and read the named slot, validating the
/// seqlock. Linux-only (no `/dev/shm` off Linux); off Linux this errors, which
/// pushes a caller toward the inline path the tests use.
#[cfg(target_os = "linux")]
fn read_ring_frame(shm_name: &str, slot: u32, seq: u64) -> Result<Vec<u8>> {
    use ados_protocol::framebus::{read_slot, RingLayout};
    let dir = std::env::var("ADOS_SHM_DIR").unwrap_or_else(|_| "/dev/shm".to_string());
    let path = std::path::PathBuf::from(dir).join(shm_name);
    let file = std::fs::OpenOptions::new()
        .read(true)
        .open(&path)
        .map_err(|e| anyhow!("open ring {shm_name}: {e}"))?;
    // SAFETY: the ring file is sized by the writer; the mapping is read-only and
    // bounded to the file length, and a torn read is caught by the seqlock.
    let map = unsafe { memmap2::Mmap::map(&file)? };
    let layout = RingLayout::read_header(&map[..]).ok_or_else(|| anyhow!("bad ring header"))?;
    read_slot(&map[..], &layout, slot, seq)
        .map_err(|e| anyhow!("ring read: {e}"))?
        .ok_or_else(|| anyhow!("ring slot {slot} no longer holds seq {seq} (torn/stale)"))
}

#[cfg(not(target_os = "linux"))]
fn read_ring_frame(_shm_name: &str, _slot: u32, _seq: u64) -> Result<Vec<u8>> {
    Err(anyhow!(
        "ring-named frames require /dev/shm; use an inline frame off Linux"
    ))
}

/// A tiny `serde_bytes`-equivalent so a msgpack binary field deserializes into
/// owned bytes without pulling the `serde_bytes` crate.
mod serde_bytes_compat {
    use serde::de::{Deserialize, Deserializer};

    #[derive(Debug, Clone, Default)]
    pub struct Bytes(pub Vec<u8>);

    impl<'de> Deserialize<'de> for Bytes {
        fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
        where
            D: Deserializer<'de>,
        {
            // rmpv decodes a msgpack `bin` to a Value::Binary; route through it
            // so both `bin` and `array<u8>` shapes are accepted.
            let v = rmpv::Value::deserialize(deserializer)?;
            let bytes = match v {
                rmpv::Value::Binary(b) => b,
                rmpv::Value::Array(items) => items
                    .into_iter()
                    .filter_map(|i| i.as_u64().map(|n| n as u8))
                    .collect(),
                _ => Vec::new(),
            };
            Ok(Bytes(bytes))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::MockBackend;
    use ados_protocol::framebus::{ModelExecution, ModelKind};

    fn engine() -> Arc<VisionEngine> {
        VisionEngine::new(Box::new(MockBackend), 4)
    }

    fn req_env(method: &str, args: Value) -> Envelope {
        Envelope {
            version: PROTOCOL_VERSION,
            kind: "request".into(),
            method: method.into(),
            capability: String::new(),
            args,
            request_id: "rid-1".into(),
            token: String::new(),
            error: None,
        }
    }

    #[tokio::test]
    async fn register_model_dispatch_returns_registered() {
        let e = engine();
        let meta = ModelMetadata {
            id: "com.example.m".into(),
            kind: ModelKind::Detection,
            execution: ModelExecution::EngineRun,
            input_width: 8,
            input_height: 8,
            input_format: FrameFormat::Rgb24,
            output_classes: vec!["x".into()],
            model_path: None,
        };
        let args: Value = rmp_serde::from_slice(&meta.to_msgpack().unwrap()).unwrap();
        let (resp, err) = dispatch(&e, &req_env(methods::REGISTER_MODEL, args)).await;
        assert!(err.is_none());
        // The response carries registered=true and the model id.
        let map = as_map(&resp);
        assert_eq!(get(&map, "registered"), Some(Value::Boolean(true)));
        assert_eq!(get(&map, "model_id"), Some(Value::from("com.example.m")));
        assert_eq!(get(&map, "execution"), Some(Value::from("engine_run")));
        assert_eq!(e.model_count().await, 1);
    }

    #[tokio::test]
    async fn infer_inline_frame_returns_a_batch() {
        let e = engine();
        // Register an engine-run model.
        let meta = ModelMetadata {
            id: "m".into(),
            kind: ModelKind::Detection,
            execution: ModelExecution::EngineRun,
            input_width: 2,
            input_height: 2,
            input_format: FrameFormat::Rgb24,
            output_classes: vec![],
            model_path: None,
        };
        e.register_model(meta).await.unwrap();

        let args = Value::Map(vec![
            (Value::from("model_id"), Value::from("m")),
            (Value::from("camera_id"), Value::from("uvc-0")),
            (Value::from("frame_id"), Value::from(7u64)),
            (Value::from("frame"), Value::Binary(vec![0u8; 12])),
            (Value::from("width"), Value::from(2u32)),
            (Value::from("height"), Value::from(2u32)),
            (Value::from("format"), Value::from("rgb24")),
        ]);
        let (resp, err) = dispatch(&e, &req_env(methods::INFER, args)).await;
        assert!(err.is_none(), "infer errored: {err:?}");
        let map = as_map(&resp);
        // The batch comes back as a binary field decodable to a DetectionBatch.
        let batch_bytes = match get(&map, "batch") {
            Some(Value::Binary(b)) => b,
            other => panic!("expected binary batch, got {other:?}"),
        };
        let batch = DetectionBatch::from_msgpack(&batch_bytes).unwrap();
        assert_eq!(batch.model_id, "m");
        assert_eq!(batch.camera_id, "uvc-0");
        assert_eq!(batch.frame_id, 7);
        assert!(batch.detections.is_empty()); // mock backend
    }

    #[tokio::test]
    async fn infer_unknown_model_returns_error() {
        let e = engine();
        let args = Value::Map(vec![
            (Value::from("model_id"), Value::from("nope")),
            (Value::from("frame"), Value::Binary(vec![0u8; 4])),
            (Value::from("width"), Value::from(1u32)),
            (Value::from("height"), Value::from(1u32)),
            (Value::from("format"), Value::from("rgb24")),
        ]);
        let (_resp, err) = dispatch(&e, &req_env(methods::INFER, args)).await;
        assert!(err.is_some());
        assert!(err.unwrap().contains("unknown model"));
    }

    #[tokio::test]
    async fn publish_detection_dispatch_counts_subscribers() {
        let e = engine();
        let _rx = e.subscribe_detections();
        let batch = DetectionBatch {
            model_id: "m".into(),
            camera_id: "c".into(),
            frame_id: 1,
            ts_ms: 0,
            detections: vec![],
        };
        let args: Value = rmp_serde::from_slice(&batch.to_msgpack().unwrap()).unwrap();
        let (resp, err) = dispatch(&e, &req_env(methods::PUBLISH_DETECTION, args)).await;
        assert!(err.is_none());
        let map = as_map(&resp);
        assert_eq!(get(&map, "subscribers"), Some(Value::from(1u64)));
    }

    #[tokio::test]
    async fn unknown_method_returns_error() {
        let e = engine();
        let (_resp, err) = dispatch(&e, &req_env("vision.bogus", Value::Map(vec![]))).await;
        assert!(err.is_some());
        assert!(err.unwrap().contains("unknown vision method"));
    }

    #[test]
    fn deliver_frame_carries_descriptor_binary() {
        let desc = FrameDescriptor {
            camera_id: "uvc-0".into(),
            frame_id: 1,
            ts_ms: 1,
            width: 8,
            height: 8,
            format: FrameFormat::Rgb24,
            shm_name: "ados-vision-uvc-0".into(),
            slot: 0,
            seq: 3,
            byte_len: 192,
        };
        let frame = deliver_frame(&desc).unwrap();
        // Strip the length prefix and decode the envelope.
        let body = &frame[HEADER_SIZE..];
        let env = Envelope::from_msgpack(body).unwrap();
        assert_eq!(env.method, methods::DELIVER_FRAME);
        assert_eq!(env.kind, "event");
        // The descriptor round-trips out of the binary field.
        let bytes = match &env.args {
            Value::Map(entries) => entries
                .iter()
                .find(|(k, _)| k.as_str() == Some("descriptor"))
                .and_then(|(_, v)| match v {
                    Value::Binary(b) => Some(b.clone()),
                    _ => None,
                })
                .unwrap(),
            _ => panic!("args not a map"),
        };
        assert_eq!(FrameDescriptor::from_msgpack(&bytes).unwrap(), desc);
    }

    #[test]
    fn filter_camera_reads_optional_id() {
        let with = Value::Map(vec![(Value::from("camera_id"), Value::from("fpv"))]);
        assert_eq!(filter_camera(&with).as_deref(), Some("fpv"));
        assert_eq!(filter_camera(&Value::Map(vec![])), None);
    }

    // --- small helpers --------------------------------------------------
    fn as_map(v: &Value) -> Vec<(Value, Value)> {
        match v {
            Value::Map(m) => m.clone(),
            _ => panic!("not a map"),
        }
    }
    fn get(map: &[(Value, Value)], key: &str) -> Option<Value> {
        map.iter()
            .find(|(k, _)| k.as_str() == Some(key))
            .map(|(_, v)| v.clone())
    }
}
