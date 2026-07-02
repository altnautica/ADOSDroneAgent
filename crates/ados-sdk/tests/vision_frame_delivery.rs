//! Vision frame delivery: Rust plugin <-> Rust host over real Contract C.
//!
//! Stands up a live `ados-plugin-host` server with a host that arms a vision
//! frame-descriptor stream, connects this SDK's client to it over a Unix
//! socket, subscribes for frames, and asserts a descriptor the host pushes
//! reaches the plugin-side callback. This exercises the full path the bug
//! broke: server `vision.deliver` push -> SDK reader loop `vision.deliver`
//! branch -> camera-id routing -> the registered frame callback. The
//! descriptor is byte-checked against what the host pushed so wire parity with
//! the engine's pump holds.

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ados_plugin_host::host::HostServices;
use ados_plugin_host::{EventBus, PluginIpcServer};
use ados_protocol::framebus::{FrameDescriptor, FrameFormat, FRAMEBUS_DESCRIPTOR_VERSION};
use ados_protocol::plugin::TokenIssuer;
use ados_sdk::PluginIpcClient;
use rmpv::Value;
use tokio::sync::broadcast;

const PLUGIN_ID: &str = "com.example.vision";

fn caps(items: &[&str]) -> BTreeSet<String> {
    items.iter().map(|s| s.to_string()).collect()
}

/// A host that arms a vision frame-descriptor stream from a broadcast channel
/// the test pushes descriptors into. Every other host method stays at the
/// trait default, which is all the vision-delivery path needs.
struct VisionStreamHost {
    frames: broadcast::Sender<Vec<u8>>,
}

impl HostServices for VisionStreamHost {
    fn vision_subscribe_stream(
        &self,
        _plugin_id: &str,
        _camera_id: &str,
    ) -> Option<broadcast::Receiver<Vec<u8>>> {
        Some(self.frames.subscribe())
    }
}

struct Harness {
    issuer: Arc<TokenIssuer>,
    path: std::path::PathBuf,
    frames: broadcast::Sender<Vec<u8>>,
    _accept: tokio::task::JoinHandle<()>,
    _dir: tempfile::TempDir,
}

fn harness() -> Harness {
    let dir = tempfile::tempdir().expect("tempdir");
    let issuer = Arc::new(TokenIssuer::new(b"vision-delivery-secret".to_vec()));
    let bus = Arc::new(EventBus::new());
    let (frames, _rx) = broadcast::channel(64);
    let host = Arc::new(VisionStreamHost {
        frames: frames.clone(),
    });
    let server = PluginIpcServer::new(dir.path(), issuer.clone(), bus, host);
    let (path, accept) = server.serve_plugin(PLUGIN_ID).expect("bind plugin socket");
    Harness {
        issuer,
        path,
        frames,
        _accept: accept,
        _dir: dir,
    }
}

async fn connect(h: &Harness, granted: &[&str]) -> Arc<PluginIpcClient> {
    let token = h
        .issuer
        .mint(PLUGIN_ID, &caps(granted), 600)
        .to_token_string();
    let ipc = Arc::new(PluginIpcClient::new(PLUGIN_ID, token, &h.path));
    ipc.connect().await.expect("connect + handshake");
    ipc
}

fn descriptor(camera_id: &str, seq: u64) -> FrameDescriptor {
    FrameDescriptor {
        v: FRAMEBUS_DESCRIPTOR_VERSION,
        camera_id: camera_id.into(),
        frame_id: seq,
        ts_ms: 1_700_000_000_000,
        width: 64,
        height: 48,
        format: FrameFormat::Rgb24,
        shm_name: format!("ados-vision-{camera_id}"),
        slot: 0,
        seq,
        byte_len: (64 * 48 * 3) as u32,
    }
}

#[tokio::test]
async fn pushed_descriptor_reaches_the_frame_callback() {
    let h = harness();
    let ipc = connect(&h, &["vision.frame.read"]).await;

    // Register a raw vision callback (the resolver path needs a real /dev/shm
    // ring, which is not portable; this asserts the routing fix — the bug was
    // the reader loop never invoking ANY callback for vision.deliver).
    let hits = Arc::new(AtomicUsize::new(0));
    let last: Arc<Mutex<Option<FrameDescriptor>>> = Arc::new(Mutex::new(None));
    let h_hits = hits.clone();
    let h_last = last.clone();
    ipc.register_vision_callback(
        Some("uvc-0"),
        Arc::new(move |args: Value| {
            h_hits.fetch_add(1, Ordering::Relaxed);
            // Decode the descriptor the server delivered in the `descriptor`
            // binary arg, the exact bytes the engine's pump emits.
            if let Value::Map(m) = &args {
                if let Some((_, Value::Binary(blob))) =
                    m.iter().find(|(k, _)| k.as_str() == Some("descriptor"))
                {
                    if let Ok(d) = FrameDescriptor::from_msgpack(blob) {
                        *h_last.lock().unwrap() = Some(d);
                    }
                }
            }
        }),
    );

    // Arm the engine stream for this camera so the server spawns its forwarder.
    ipc.vision_subscribe_frames(Some("uvc-0"))
        .await
        .expect("subscribe_frames");
    // Let the server's forwarder attach to the broadcast before pushing.
    tokio::time::sleep(Duration::from_millis(50)).await;

    let want = descriptor("uvc-0", 7);
    h.frames
        .send(want.to_msgpack().unwrap())
        .expect("push descriptor");

    let mut saw = false;
    for _ in 0..100 {
        if hits.load(Ordering::Relaxed) > 0 {
            saw = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(saw, "the vision.deliver push never reached the callback");
    assert_eq!(
        last.lock().unwrap().as_ref(),
        Some(&want),
        "the delivered descriptor must match the one the host pushed"
    );

    ipc.close().await;
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn subscribe_frames_resolves_a_real_ring_to_a_frame() {
    use ados_protocol::framebus::{write_slot, RingLayout};
    use std::io::Write;

    let h = harness();
    let ipc = connect(&h, &["vision.frame.read"]).await;

    // Build a real /dev/shm ring the resolver maps by name.
    let camera = "uvc-shm";
    let shm_name = format!("ados-vision-{camera}");
    let shm_path = format!("/dev/shm/{shm_name}");
    let _ = std::fs::remove_file(&shm_path);
    let layout = RingLayout::for_frame(4, 4, 4, FrameFormat::Rgb24); // 48-byte slots
    let mut region = vec![0u8; layout.total_len()];
    layout.write_header(&mut region).unwrap();
    let pixels: Vec<u8> = (0..layout.slot_bytes as u8).collect();
    let seq = 3u64;
    let slot = (seq % layout.slot_count as u64) as u32;
    write_slot(&mut region, &layout, slot, seq, &pixels).unwrap();
    {
        let mut f = std::fs::File::create(&shm_path).expect("create shm");
        f.write_all(&region).expect("write shm");
    }

    // The plugin context owns the VisionClient built from the same connected
    // IPC client, exactly as a real plugin reaches `ctx.vision`.
    let ctx = ados_sdk::PluginContext::new(
        ipc.clone(),
        "1.0.0",
        "agent-1",
        std::collections::BTreeMap::new(),
    );

    let got: Arc<Mutex<Option<ados_sdk::Frame>>> = Arc::new(Mutex::new(None));
    let g = got.clone();
    ctx.vision
        .subscribe_frames(
            Some(camera),
            Arc::new(move |frame: ados_sdk::Frame| {
                *g.lock().unwrap() = Some(frame);
            }),
        )
        .await
        .expect("subscribe_frames");
    tokio::time::sleep(Duration::from_millis(50)).await;

    let mut want = descriptor(camera, seq);
    want.width = 4;
    want.height = 4;
    want.shm_name = shm_name.clone();
    want.slot = slot;
    want.byte_len = layout.slot_bytes;
    h.frames
        .send(want.to_msgpack().unwrap())
        .expect("push descriptor");

    let mut resolved = None;
    for _ in 0..100 {
        if let Some(f) = got.lock().unwrap().clone() {
            resolved = Some(f);
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let _ = std::fs::remove_file(&shm_path);
    let frame = resolved.expect("the descriptor must resolve to a Frame from the ring");
    assert_eq!(frame.descriptor.camera_id, camera);
    assert_eq!(frame.pixels, pixels);

    ipc.close().await;
}
