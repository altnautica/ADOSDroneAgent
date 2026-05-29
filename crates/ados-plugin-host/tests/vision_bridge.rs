//! End-to-end round trips through the vision bridge of the plugin host.
//!
//! A client built only from `ados-protocol` types handshakes against a live
//! [`PluginIpcServer`] backed by a test host that exposes a vision
//! frame-descriptor stream. The test proves two things over the wire:
//!
//! * `vision.subscribe_frames` is gated on `vision.frame.read`, and once granted
//!   it arms the push stream so frame descriptors arrive as `vision.deliver`
//!   events, mirroring the MAVLink frame pump.
//! * the request/response vision methods route through the host and return its
//!   response.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use ados_plugin_host::host::{HostError, HostResult, HostServices};
use ados_plugin_host::{EventBus, PluginIpcServer};
use ados_protocol::frame::{decode_len, HEADER_SIZE, PLUGIN_MAX_FRAME};
use ados_protocol::framebus::{methods, FrameDescriptor, FrameFormat};
use ados_protocol::plugin::{Envelope, TokenIssuer, PROTOCOL_VERSION};
use rmpv::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::sync::broadcast;

const PLUGIN_ID: &str = "com.example.vision";

/// A host that hands out a vision frame-descriptor stream and answers the three
/// request methods with a fixed marker so the route is observable end-to-end.
struct VisionTestHost {
    frames: broadcast::Sender<Vec<u8>>,
}

impl VisionTestHost {
    fn new() -> Self {
        let (frames, _rx) = broadcast::channel(256);
        Self { frames }
    }

    /// Publish a frame descriptor as if the vision engine pushed it.
    fn push(&self, descriptor: &FrameDescriptor) {
        let _ = self.frames.send(descriptor.to_msgpack().unwrap());
    }
}

impl HostServices for VisionTestHost {
    fn vision_subscribe_stream(
        &self,
        _plugin_id: &str,
        _camera_id: &str,
    ) -> Option<broadcast::Receiver<Vec<u8>>> {
        Some(self.frames.subscribe())
    }

    async fn vision_register_model(
        &self,
        _plugin_id: &str,
        _args: &Value,
    ) -> Result<HostResult, HostError> {
        Ok(Value::Map(vec![(
            Value::from("registered"),
            Value::Boolean(true),
        )]))
    }
}

fn caps(items: &[&str]) -> BTreeSet<String> {
    items.iter().map(|s| s.to_string()).collect()
}

fn request(method: &str, token: &str, args: Value) -> Envelope {
    Envelope {
        version: PROTOCOL_VERSION,
        kind: "request".to_string(),
        method: method.to_string(),
        capability: String::new(),
        args,
        request_id: format!("req-{method}"),
        token: token.to_string(),
        error: None,
    }
}

async fn send(stream: &mut UnixStream, env: &Envelope) {
    let frame = env.encode_frame().expect("encode frame");
    stream.write_all(&frame).await.expect("write frame");
    stream.flush().await.expect("flush");
}

async fn recv(stream: &mut UnixStream) -> Envelope {
    let mut header = [0u8; HEADER_SIZE];
    stream.read_exact(&mut header).await.expect("read header");
    let len = decode_len(header, PLUGIN_MAX_FRAME, true).expect("decode len");
    let mut body = vec![0u8; len];
    stream.read_exact(&mut body).await.expect("read body");
    Envelope::from_msgpack(&body).expect("decode envelope")
}

fn args_bool(env: &Envelope, key: &str) -> Option<bool> {
    match &env.args {
        Value::Map(m) => m
            .iter()
            .find(|(k, _)| k.as_str() == Some(key))
            .and_then(|(_, v)| v.as_bool()),
        _ => None,
    }
}

fn args_bytes(env: &Envelope, key: &str) -> Option<Vec<u8>> {
    match &env.args {
        Value::Map(m) => m
            .iter()
            .find(|(k, _)| k.as_str() == Some(key))
            .and_then(|(_, v)| match v {
                Value::Binary(b) => Some(b.clone()),
                _ => None,
            }),
        _ => None,
    }
}

struct Harness {
    issuer: Arc<TokenIssuer>,
    host: Arc<VisionTestHost>,
    path: std::path::PathBuf,
    _accept: tokio::task::JoinHandle<()>,
    _dir: tempfile::TempDir,
}

fn harness() -> Harness {
    let dir = tempfile::tempdir().expect("tempdir");
    let issuer = Arc::new(TokenIssuer::new(b"vision-bridge-secret".to_vec()));
    let bus = Arc::new(EventBus::new());
    let host = Arc::new(VisionTestHost::new());
    let server = PluginIpcServer::new(dir.path(), issuer.clone(), bus, host.clone());
    let (path, accept) = server.serve_plugin(PLUGIN_ID).expect("bind plugin socket");
    Harness {
        issuer,
        host,
        path,
        _accept: accept,
        _dir: dir,
    }
}

async fn connect(path: &std::path::Path) -> UnixStream {
    for _ in 0..50 {
        if let Ok(s) = UnixStream::connect(path).await {
            return s;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("could not connect to {path:?}");
}

fn sample_descriptor() -> FrameDescriptor {
    FrameDescriptor {
        camera_id: "uvc-0".into(),
        frame_id: 7,
        ts_ms: 1_700_000_000_000,
        width: 64,
        height: 48,
        format: FrameFormat::Rgb24,
        shm_name: "ados-vision-uvc-0".into(),
        slot: 1,
        seq: 7,
        byte_len: (64 * 48 * 3) as u32,
    }
}

#[tokio::test]
async fn subscribe_frames_is_denied_without_the_read_cap() {
    let h = harness();
    let token = h.issuer.mint(PLUGIN_ID, &caps(&[]), 600).to_token_string();
    let mut client = connect(&h.path).await;
    send(&mut client, &request("hello", &token, Value::Map(vec![]))).await;
    let _ = recv(&mut client).await; // ready

    send(
        &mut client,
        &request(methods::SUBSCRIBE_FRAMES, &token, Value::Map(vec![])),
    )
    .await;
    let resp = recv(&mut client).await;
    assert_eq!(
        resp.error.as_deref(),
        Some("capability_denied: vision.frame.read")
    );
}

#[tokio::test]
async fn subscribe_frames_then_descriptors_arrive_as_vision_deliver() {
    let h = harness();
    let token = h
        .issuer
        .mint(PLUGIN_ID, &caps(&["vision.frame.read"]), 600)
        .to_token_string();
    let mut client = connect(&h.path).await;
    send(&mut client, &request("hello", &token, Value::Map(vec![]))).await;
    let _ = recv(&mut client).await; // ready

    // Subscribe to every camera (no camera_id).
    send(
        &mut client,
        &request(methods::SUBSCRIBE_FRAMES, &token, Value::Map(vec![])),
    )
    .await;
    let resp = recv(&mut client).await;
    assert_eq!(resp.error, None);
    assert_eq!(args_bool(&resp, "subscribed"), Some(true));

    // The engine pushes a descriptor; let the forwarder arm first.
    tokio::time::sleep(Duration::from_millis(50)).await;
    let descriptor = sample_descriptor();
    h.host.push(&descriptor);

    // The next push from the server must be a vision.deliver carrying the same
    // descriptor bytes.
    let frame = tokio::time::timeout(Duration::from_secs(2), recv(&mut client))
        .await
        .expect("vision.deliver within timeout");
    assert_eq!(frame.kind, "event");
    assert_eq!(frame.method, methods::DELIVER_FRAME);
    let got = args_bytes(&frame, "descriptor").expect("descriptor bytes present");
    assert_eq!(FrameDescriptor::from_msgpack(&got).unwrap(), descriptor);
}

#[tokio::test]
async fn second_subscribe_to_same_camera_reports_already_subscribed() {
    let h = harness();
    let token = h
        .issuer
        .mint(PLUGIN_ID, &caps(&["vision.frame.read"]), 600)
        .to_token_string();
    let mut client = connect(&h.path).await;
    send(&mut client, &request("hello", &token, Value::Map(vec![]))).await;
    let _ = recv(&mut client).await; // ready

    let args = Value::Map(vec![(Value::from("camera_id"), Value::from("uvc-0"))]);
    send(
        &mut client,
        &request(methods::SUBSCRIBE_FRAMES, &token, args.clone()),
    )
    .await;
    let first = recv(&mut client).await;
    assert_eq!(args_bool(&first, "subscribed"), Some(true));

    send(
        &mut client,
        &request(methods::SUBSCRIBE_FRAMES, &token, args),
    )
    .await;
    let second = recv(&mut client).await;
    assert_eq!(args_bool(&second, "already_subscribed"), Some(true));
}

#[tokio::test]
async fn register_model_routes_to_the_host() {
    let h = harness();
    let token = h
        .issuer
        .mint(PLUGIN_ID, &caps(&["vision.model.register"]), 600)
        .to_token_string();
    let mut client = connect(&h.path).await;
    send(&mut client, &request("hello", &token, Value::Map(vec![]))).await;
    let _ = recv(&mut client).await; // ready

    let args = Value::Map(vec![(Value::from("model_id"), Value::from("m1"))]);
    send(
        &mut client,
        &request(methods::REGISTER_MODEL, &token, args),
    )
    .await;
    let resp = recv(&mut client).await;
    assert_eq!(resp.error, None, "granted cap must not produce a gate error");
    assert_eq!(args_bool(&resp, "registered"), Some(true));
}

#[tokio::test]
async fn infer_falls_back_to_not_implemented_default() {
    let h = harness();
    // The test host overrides register_model but not infer, so infer hits the
    // trait default that returns the not_implemented shape.
    let token = h
        .issuer
        .mint(PLUGIN_ID, &caps(&["vision.model.register"]), 600)
        .to_token_string();
    let mut client = connect(&h.path).await;
    send(&mut client, &request("hello", &token, Value::Map(vec![]))).await;
    let _ = recv(&mut client).await; // ready

    send(
        &mut client,
        &request(methods::INFER, &token, Value::Map(vec![])),
    )
    .await;
    let resp = recv(&mut client).await;
    assert_eq!(resp.error, None);
    let method = match &resp.args {
        Value::Map(m) => m
            .iter()
            .find(|(k, _)| k.as_str() == Some("method"))
            .and_then(|(_, v)| v.as_str()),
        _ => None,
    };
    assert_eq!(method, Some("vision.infer"));
}
