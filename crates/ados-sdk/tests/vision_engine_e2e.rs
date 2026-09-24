//! The vision surface end to end: SDK -> live plugin host -> engine socket.
//!
//! The host is the production [`RealHost`], its vision client dialing a
//! `vision.sock` in a temp dir. The request methods run against the real
//! engine's socket server in-process (over a probe backend), so what the SDK
//! builds is decoded by the engine's own handlers. The engine-restart test uses
//! a fake engine instead: the real one keeps its rings in the heap off Linux,
//! and the restart is about ring files. The fake speaks the same wire, pushes
//! descriptors and batches the way the engine's push tasks do, and writes real
//! ring files laid out and seqlocked through the `framebus` contract, so the
//! SDK resolves them exactly as it resolves `/dev/shm` on a drone.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use ados_plugin_host::realhost::RealHost;
use ados_plugin_host::{EventBus, PluginIpcServer, VisionClient as HostVisionClient};
use ados_protocol::frame::{decode_len, HEADER_SIZE, PLUGIN_MAX_FRAME};
use ados_protocol::framebus::{
    methods, write_slot, BoundingBox, Detection, DetectionBatch, DetectionHead, FrameDescriptor,
    FrameFormat, ModelExecution, ModelKind, ModelMetadata, RingLayout, FRAMEBUS_DESCRIPTOR_VERSION,
    VISION_DETECTION_VERSION,
};
use ados_protocol::plugin::{Envelope, TokenIssuer, PROTOCOL_VERSION};
use ados_protocol::shutdown::Shutdown;
use ados_sdk::{Frame, PluginContext, PluginIpcClient};
use ados_vision::backend::{LoadedModel, VisionBackend};
use ados_vision::engine::VisionEngine;
use parking_lot::Mutex;
use rmpv::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::unix::OwnedReadHalf;
use tokio::net::UnixListener;
use tokio::sync::mpsc;
use tokio::task::{JoinHandle, JoinSet};

const PLUGIN_ID: &str = "com.example.vision";
const CAMERA: &str = "uvc-0";
const WAIT: Duration = Duration::from_secs(10);

/// A stand-in for the engine's `vision.sock` server that serves the two push
/// streams.
struct FakeEngine {
    accept: JoinHandle<()>,
    /// The subscribe methods the host sent, in arrival order.
    armed: mpsc::UnboundedReceiver<String>,
    /// The current connection's outbound frame queue.
    writer: Arc<Mutex<Option<mpsc::UnboundedSender<Vec<u8>>>>>,
}

impl FakeEngine {
    fn start(sock: &Path) -> Self {
        let _ = std::fs::remove_file(sock);
        let listener = UnixListener::bind(sock).expect("bind fake engine socket");
        let (arm_tx, armed) = mpsc::unbounded_channel();
        let writer: Arc<Mutex<Option<mpsc::UnboundedSender<Vec<u8>>>>> = Arc::new(Mutex::new(None));
        let current = writer.clone();
        let accept = tokio::spawn(async move {
            // Owned by this task, so stopping the engine drops every connection
            // with it, the way a process exit does.
            let mut conns = JoinSet::new();
            while let Ok((stream, _)) = listener.accept().await {
                let (mut rd, mut wr) = stream.into_split();
                let (out_tx, mut out_rx) = mpsc::unbounded_channel::<Vec<u8>>();
                *current.lock() = Some(out_tx.clone());
                conns.spawn(async move {
                    while let Some(frame) = out_rx.recv().await {
                        if wr.write_all(&frame).await.is_err() {
                            break;
                        }
                    }
                });
                let arm_tx = arm_tx.clone();
                conns.spawn(async move {
                    while let Some(env) = read_envelope(&mut rd).await {
                        assert!(
                            env.method == methods::SUBSCRIBE_FRAMES
                                || env.method == methods::SUBSCRIBE_DETECTIONS,
                            "unexpected request {}",
                            env.method
                        );
                        let _ = arm_tx.send(env.method.clone());
                        let args = map(vec![("subscribed", Value::Boolean(true))]);
                        let _ = out_tx.send(response(&env.request_id, args));
                    }
                });
            }
        });
        Self {
            accept,
            armed,
            writer,
        }
    }

    /// Stop serving: the listener and every connection go away at once.
    fn stop(self) {
        self.accept.abort();
    }

    /// Write one pre-encoded frame to the current host connection.
    fn push(&self, frame: Vec<u8>) {
        self.writer
            .lock()
            .as_ref()
            .expect("a host connection")
            .send(frame)
            .expect("connection open");
    }

    /// Wait until the host has asked for both push streams on this engine.
    async fn wait_armed(&mut self) {
        let mut seen = BTreeSet::new();
        while seen.len() < 2 {
            let method = tokio::time::timeout(WAIT, self.armed.recv())
                .await
                .expect("the host never armed the engine's push streams")
                .expect("engine stopped");
            seen.insert(method);
        }
    }
}

/// The real engine's `vision.sock` server, in-process over [`ProbeBackend`].
struct RealEngine {
    engine: Arc<VisionEngine>,
    cancel: Shutdown,
}

impl RealEngine {
    fn start(sock: &Path) -> Self {
        let engine = VisionEngine::new(Box::new(ProbeBackend), 4);
        let cancel = Shutdown::new();
        let path = sock.to_str().expect("utf-8 socket path").to_string();
        let (e, c) = (engine.clone(), cancel.clone());
        tokio::spawn(async move {
            ados_vision::visionsock::serve(e, &path, c)
                .await
                .expect("serve vision.sock");
        });
        Self { engine, cancel }
    }
}

impl Drop for RealEngine {
    fn drop(&mut self) {
        self.cancel.trigger();
    }
}

/// A backend whose model reports the frame it was handed: one detection whose
/// label is the first pixel byte and whose box is the frame, so a test can see
/// the engine ran on the frame the request named.
struct ProbeBackend;

struct ProbeModel;

impl LoadedModel for ProbeModel {
    fn infer(
        &self,
        frame: &[u8],
        width: u32,
        height: u32,
        _format: FrameFormat,
    ) -> anyhow::Result<Vec<Detection>> {
        Ok(vec![Detection {
            bbox: Some(BoundingBox {
                x: 0.0,
                y: 0.0,
                width: width as f32,
                height: height as f32,
            }),
            class_label: format!("pixel-{}", frame.first().copied().unwrap_or(0)),
            ..detection()
        }])
    }
}

impl VisionBackend for ProbeBackend {
    fn load(&self, _meta: &ModelMetadata) -> anyhow::Result<Box<dyn LoadedModel>> {
        Ok(Box::new(ProbeModel))
    }
    fn name(&self) -> &str {
        "probe"
    }
}

async fn read_envelope(rd: &mut OwnedReadHalf) -> Option<Envelope> {
    let mut header = [0u8; HEADER_SIZE];
    rd.read_exact(&mut header).await.ok()?;
    let len = decode_len(header, PLUGIN_MAX_FRAME, true).ok()?;
    let mut body = vec![0u8; len];
    rd.read_exact(&mut body).await.ok()?;
    Envelope::from_msgpack(&body).ok()
}

fn map(pairs: Vec<(&str, Value)>) -> Value {
    Value::Map(
        pairs
            .into_iter()
            .map(|(k, v)| (Value::from(k), v))
            .collect(),
    )
}

fn envelope(kind: &str, method: &str, request_id: &str, args: Value) -> Vec<u8> {
    Envelope {
        version: PROTOCOL_VERSION,
        kind: kind.to_string(),
        method: method.to_string(),
        capability: String::new(),
        args,
        request_id: request_id.to_string(),
        token: String::new(),
        error: None,
    }
    .encode_frame()
    .expect("encode envelope")
}

fn response(request_id: &str, args: Value) -> Vec<u8> {
    envelope("response", "response", request_id, args)
}

/// The engine's `vision.deliver` push for one descriptor.
fn deliver_frame(desc: &FrameDescriptor) -> Vec<u8> {
    let args = map(vec![(
        "descriptor",
        Value::Binary(desc.to_msgpack().expect("encode descriptor")),
    )]);
    envelope("event", methods::DELIVER_FRAME, "vis-frame", args)
}

/// The engine's `vision.deliver_detection` push for one batch.
fn deliver_detection(batch: &DetectionBatch) -> Vec<u8> {
    let args = map(vec![(
        "batch",
        Value::Binary(batch.to_msgpack().expect("encode batch")),
    )]);
    envelope("event", methods::DELIVER_DETECTION, "vis-det", args)
}

/// A frame ring written the way the engine's ring writer writes one: a file
/// named for the camera, mapped read/write, header stamped, each frame placed
/// in slot `seq % slot_count` under the per-slot seqlock, `seq` from 1.
struct TestRing {
    map: memmap2::MmapMut,
    layout: RingLayout,
    shm_name: String,
    next_seq: u64,
    _file: std::fs::File,
}

impl TestRing {
    /// Create the ring the way an engine start does: whatever file held the
    /// name is unlinked first, so this is a new file under the same name.
    fn create(dir: &Path) -> Self {
        let shm_name = format!("ados-vision-{CAMERA}");
        let path = dir.join(&shm_name);
        let _ = std::fs::remove_file(&path);
        let layout = RingLayout::for_frame(4, 4, 4, FrameFormat::Rgb24);
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)
            .expect("create ring file");
        file.set_len(layout.total_len() as u64).expect("size ring");
        // SAFETY: the file was just sized to the layout and only this test
        // writes it.
        let mut map = unsafe { memmap2::MmapMut::map_mut(&file) }.expect("map ring");
        layout.write_header(&mut map[..]).expect("ring header");
        Self {
            map,
            layout,
            shm_name,
            next_seq: 1,
            _file: file,
        }
    }

    fn write(&mut self, pixels: &[u8]) -> FrameDescriptor {
        let seq = self.next_seq;
        self.next_seq += 1;
        let slot = (seq % self.layout.slot_count as u64) as u32;
        write_slot(&mut self.map[..], &self.layout, slot, seq, pixels).expect("write slot");
        FrameDescriptor {
            v: FRAMEBUS_DESCRIPTOR_VERSION,
            camera_id: CAMERA.to_string(),
            frame_id: seq,
            ts_ms: 1_700_000_000_000 + seq as i64,
            width: 4,
            height: 4,
            format: FrameFormat::Rgb24,
            shm_name: self.shm_name.clone(),
            slot,
            seq,
            byte_len: pixels.len() as u32,
        }
    }
}

fn batch(frame_id: u64) -> DetectionBatch {
    DetectionBatch {
        v: VISION_DETECTION_VERSION,
        model_id: "com.example.detector".to_string(),
        camera_id: CAMERA.to_string(),
        frame_id,
        ts_ms: 1_700_000_000_000,
        frame_width: 4,
        frame_height: 4,
        detections: Vec::new(),
    }
}

struct Harness {
    ctx: PluginContext,
    ipc: Arc<PluginIpcClient>,
    sock: PathBuf,
    dir: PathBuf,
    _accept: JoinHandle<()>,
    _dir: tempfile::TempDir,
}

/// An engine from `start` on `vision.sock` in a temp dir, a live plugin host
/// whose vision client is connected to it, and a plugin connected to the host
/// holding `granted`.
async fn harness<E>(granted: &[&str], start: impl FnOnce(&Path) -> E) -> (Harness, E) {
    let dir = tempfile::tempdir().expect("tempdir");
    let sock = dir.path().join("vision.sock");
    let engine = start(&sock);
    eventually("the engine socket", || sock.exists()).await;
    let host_vision = Arc::new(HostVisionClient::spawn(&sock));
    assert!(
        host_vision.connected_within(WAIT).await,
        "the host never connected to the engine"
    );
    let host = Arc::new(RealHost::new().with_vision(host_vision.clone()));
    let issuer = Arc::new(TokenIssuer::new(b"vision-engine-e2e-secret".to_vec()));
    let server = PluginIpcServer::new(dir.path(), issuer.clone(), Arc::new(EventBus::new()), host);
    let (path, accept) = server.serve_plugin(PLUGIN_ID).expect("bind plugin socket");
    let caps: BTreeSet<String> = granted.iter().map(|s| s.to_string()).collect();
    let token = issuer.mint(PLUGIN_ID, &caps, 600).to_token_string();
    let ipc = Arc::new(PluginIpcClient::new(PLUGIN_ID, token, &path));
    ipc.connect().await.expect("connect + handshake");
    let mut ctx = PluginContext::new(ipc.clone(), "1.0.0", "agent-1", None, BTreeMap::new());
    ctx.vision = ctx.vision.clone().with_shm_dir(dir.path());
    let h = Harness {
        ctx,
        ipc,
        sock,
        dir: dir.path().to_path_buf(),
        _accept: accept,
        _dir: dir,
    };
    (h, engine)
}

/// Poll `check` until it holds or [`WAIT`] runs out.
async fn eventually(what: &str, mut check: impl FnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + WAIT;
    while !check() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {what}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// An engine restart must not leave a subscribed plugin's frame callback
/// silent until the plugin is disabled and re-enabled, while the host reports
/// the engine reconnected and its push re-armed. Frames and
/// detections must both resume on their own, and a frame must carry the
/// restarted engine's pixels, not a previous run's.
#[tokio::test]
async fn frame_and_detection_subscriptions_survive_an_engine_restart() {
    let (h, mut engine) = harness(
        &["vision.frame.read", "vision.detection.subscribe"],
        FakeEngine::start,
    )
    .await;

    let frames: Arc<Mutex<Vec<Frame>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = frames.clone();
    h.ctx
        .vision
        .subscribe_frames(Some(CAMERA), Arc::new(move |f: Frame| sink.lock().push(f)))
        .await
        .expect("subscribe_frames");
    let batches: Arc<Mutex<Vec<DetectionBatch>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = batches.clone();
    h.ctx
        .vision
        .subscribe_detections(None, Arc::new(move |b: DetectionBatch| sink.lock().push(b)))
        .await
        .expect("subscribe_detections");
    engine.wait_armed().await;

    // The first engine run streams more frames than the ring has slots, so
    // every slot holds a sequence the next run will not reach for a while.
    let mut ring = TestRing::create(&h.dir);
    for seq in 1..=6u8 {
        engine.push(deliver_frame(&ring.write(&[seq; 48])));
    }
    eventually("the first run's frames", || {
        frames.lock().iter().any(|f| f.descriptor.seq == 6)
    })
    .await;
    engine.push(deliver_detection(&batch(6)));
    eventually("the first run's detection batch", || {
        batches.lock().iter().any(|b| b.frame_id == 6)
    })
    .await;

    // Restart: the connection drops, the old ring's name is unlinked and a new
    // ring is created under it, and the sequence starts over.
    engine.stop();
    drop(ring);
    let mut ring = TestRing::create(&h.dir);
    let mut engine = FakeEngine::start(&h.sock);
    engine.wait_armed().await;

    frames.lock().clear();
    batches.lock().clear();
    for seq in 1..=2u8 {
        engine.push(deliver_frame(&ring.write(&[0xB0 + seq; 48])));
    }
    eventually("a frame from the restarted engine", || {
        !frames.lock().is_empty()
    })
    .await;
    for f in frames.lock().iter() {
        assert_eq!(
            f.pixels,
            vec![0xB0 + f.descriptor.seq as u8; 48],
            "a frame after the restart must carry the restarted engine's pixels"
        );
    }
    engine.push(deliver_detection(&batch(1)));
    eventually("a detection batch from the restarted engine", || {
        batches.lock().iter().any(|b| b.frame_id == 1)
    })
    .await;

    engine.stop();
    h.ipc.close().await;
}

fn detection() -> Detection {
    Detection {
        bbox: Some(BoundingBox {
            x: 12.0,
            y: 24.5,
            width: 64.0,
            height: 48.0,
        }),
        class_label: "person".to_string(),
        confidence: 0.875,
        track_id: Some(7),
        assoc_confidence: None,
        lock_state: None,
        attributes: None,
        mask: None,
        keypoints: None,
        depth: None,
        world_pos: None,
    }
}

fn model() -> ModelMetadata {
    ModelMetadata {
        id: "com.example.detector".to_string(),
        kind: ModelKind::Detection,
        execution: ModelExecution::EngineRun,
        input_width: 4,
        input_height: 4,
        input_format: FrameFormat::Rgb24,
        output_classes: vec!["person".to_string(), "car".to_string()],
        model_path: None,
        head: DetectionHead::Yolo8,
    }
}

fn field<'a>(args: &'a Value, key: &str) -> Option<&'a Value> {
    args.as_map()?
        .iter()
        .find(|(k, _)| k.as_str() == Some(key))
        .map(|(_, v)| v)
}

/// A batch a plugin publishes (an offloaded detection, say) must reach the
/// drone's detection bus, not be refused by the engine as an undecodable
/// batch (`decode args: missing field 'v'`).
#[tokio::test]
async fn a_published_batch_reaches_the_engines_detection_bus() {
    let (h, engine) = harness(&["vision.detection.publish"], RealEngine::start).await;
    let mut bus = engine.engine.subscribe_detections();
    let sent = DetectionBatch {
        detections: vec![detection()],
        ..batch(42)
    };

    let reply = h
        .ctx
        .vision
        .publish_detection(&sent)
        .await
        .expect("publish");

    let got = tokio::time::timeout(WAIT, bus.recv())
        .await
        .expect("the batch never reached the engine's detection bus")
        .expect("detection bus open");
    assert_eq!(got, sent);
    assert_eq!(field(&reply, "subscribers"), Some(&Value::from(1u64)));
    h.ipc.close().await;
}

#[tokio::test]
async fn a_registered_model_lands_in_the_engines_registry() {
    let (h, engine) = harness(&["vision.model.register"], RealEngine::start).await;

    let reply = h
        .ctx
        .vision
        .register_model(&model())
        .await
        .expect("register");

    assert_eq!(field(&reply, "registered"), Some(&Value::Boolean(true)));
    let models = engine.engine.list_models().await;
    assert_eq!(models.len(), 1);
    assert_eq!(models[0].id, model().id);
    assert_eq!(models[0].output_classes, model().output_classes);
    assert!(models[0].backend_loaded);
    h.ipc.close().await;
}

#[tokio::test]
async fn infer_runs_the_engines_model_on_the_named_frame() {
    let (h, engine) = harness(&["vision.model.register"], RealEngine::start).await;
    engine
        .engine
        .register_model(model())
        .await
        .expect("register");
    let pixels = [0x5Au8; 48];
    let descriptor = engine
        .engine
        .publish_frame(
            CAMERA,
            3,
            1_700_000_000_000,
            4,
            4,
            FrameFormat::Rgb24,
            &pixels,
        )
        .await
        .expect("publish frame");
    let frame = Frame {
        descriptor,
        pixels: pixels.to_vec(),
    };

    let detections = h
        .ctx
        .vision
        .infer(&model().id, &frame)
        .await
        .expect("infer");

    assert_eq!(detections.len(), 1);
    assert_eq!(detections[0].class_label, "pixel-90");
    assert_eq!(
        detections[0].bbox,
        Some(BoundingBox {
            x: 0.0,
            y: 0.0,
            width: 4.0,
            height: 4.0
        })
    );
    h.ipc.close().await;
}

#[tokio::test]
async fn a_designated_target_locks_the_engines_tracker() {
    let (h, _engine) = harness(&["vision.track.designate"], RealEngine::start).await;

    let reply = h
        .ctx
        .vision
        .designate_track(CAMERA, &detection())
        .await
        .expect("designate");

    assert_eq!(field(&reply, "designated"), Some(&Value::Boolean(true)));
    assert_eq!(field(&reply, "camera_id"), Some(&Value::from(CAMERA)));
    assert!(
        matches!(field(&reply, "track_id"), Some(Value::Integer(_))),
        "the tracker assigned the designated box a track id: {reply:?}"
    );
    h.ipc.close().await;
}
