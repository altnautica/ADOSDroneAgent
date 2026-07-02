//! Vision-engine → atlas frame seam: a loud regression net for the hop where a
//! frame the vision engine publishes must reach the atlas world-model capture
//! reader byte for byte.
//!
//! The failure this guards against: two services in one build silently
//! disagreeing on a wire format, so frames never cross and the world model
//! captures nothing with no error surfaced. Each hop below asserts the real
//! bytes/fields that crossed; a broken hop fails loudly, and every read is
//! bounded by a timeout so a broken hop never hangs the suite.
//!
//! Cross-platform split: the frame *descriptor* crossing the engine's
//! `vision-frames.sock` is wire-only and runs everywhere. The atlas reader
//! (`VisionFrameSource`) additionally copies the pixels out of a `/dev/shm`
//! ring the descriptor names — a Linux-only mechanism (off Linux the writer's
//! ring is a process-local heap buffer and the reader's hard-coded `/dev/shm`
//! path does not exist) — so the pixel-readback and camera-filter assertions
//! are gated with #[cfg(target_os = "linux")]: they compile and run only on
//! Linux (cfg'd out on the dev host), and Linux CI is what covers them.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use ados_protocol::framebus::{FrameDescriptor, FrameFormat};
use ados_protocol::ipc::{connect_with_retry, read_length_prefixed};
use ados_protocol::state::STATE_V2_MAX_FRAME;
use ados_vision::backend::MockBackend;
use ados_vision::engine::VisionEngine;
use tempfile::TempDir;
use tokio::net::UnixStream;
use tokio::sync::Notify;
use tokio::task::JoinHandle;

/// Unique per-call ids so concurrent test binaries never collide on a shared
/// `/dev/shm/ados-vision-<camera_id>` ring name in CI.
static SEQ: AtomicU64 = AtomicU64::new(0);

fn unique_camera(tag: &str) -> String {
    format!(
        "atlasflow-{tag}-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    )
}

/// An 8x8 RGB24 frame (192 bytes) with a non-uniform pattern, so a wrong ring,
/// a wrong slot, or a truncated read is caught instead of being masked by
/// all-equal bytes. `seed` distinguishes one frame's pixels from another's.
fn rgb24_pattern(seed: u8) -> Vec<u8> {
    let n = FrameFormat::Rgb24.frame_bytes(8, 8);
    (0..n)
        .map(|i| ((i * 7 + seed as usize) % 251) as u8)
        .collect()
}

/// The running frame bus: the engine, the bound `vision-frames.sock`, and the
/// `serve` task, kept together so a test can publish and then cleanly stop.
struct Bus {
    engine: Arc<VisionEngine>,
    sock: String,
    cancel: Arc<Notify>,
    server: JoinHandle<()>,
    _dir: TempDir,
}

impl Bus {
    /// Bind the engine's frame-descriptor broadcast on a private tempdir socket.
    fn start(slot_count: u32) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir
            .path()
            .join("vision-frames.sock")
            .to_string_lossy()
            .to_string();
        let engine = VisionEngine::new(Box::new(MockBackend), slot_count);
        let cancel = Arc::new(Notify::new());
        let server = {
            let engine = engine.clone();
            let cancel = cancel.clone();
            let sock = sock.clone();
            tokio::spawn(async move {
                ados_vision::frame_bus::serve(engine, &sock, cancel)
                    .await
                    .expect("frame bus serve");
            })
        };
        Self {
            engine,
            sock,
            cancel,
            server,
            _dir: dir,
        }
    }

    async fn shutdown(self) {
        self.cancel.notify_waiters();
        let _ = self.server.await;
    }
}

/// Read one length-prefixed descriptor frame off `stream`, bounded so a broken
/// hop fails loudly instead of hanging.
async fn read_descriptor(stream: &mut UnixStream) -> FrameDescriptor {
    let payload = tokio::time::timeout(
        Duration::from_secs(2),
        read_length_prefixed(stream, STATE_V2_MAX_FRAME, true),
    )
    .await
    .expect("frame bus descriptor timed out")
    .expect("frame bus read error")
    .expect("frame bus closed with no descriptor");
    FrameDescriptor::from_msgpack(&payload).expect("decode frame descriptor")
}

#[tokio::test]
async fn descriptor_crosses_the_frame_bus() {
    // Cross-platform: the engine → vision-frames.sock descriptor hop. Only the
    // small descriptor crosses the socket (the pixels stay in the ring), so this
    // asserts the wire fields, not the pixels.
    let bus = Bus::start(4);
    let mut probe = connect_with_retry(&bus.sock, 100, Duration::from_millis(20))
        .await
        .expect("connect to frame bus");
    // Let serve bind, register the subscriber, and subscribe to the engine
    // broadcast before the publish (the subscribe is synchronous right after the
    // bind); the readback below is the real proof the descriptor arrived.
    tokio::time::sleep(Duration::from_millis(50)).await;

    let cam = unique_camera("wire");
    let pixels = rgb24_pattern(11);
    let published = bus
        .engine
        .publish_frame(&cam, 7, 1234, 8, 8, FrameFormat::Rgb24, &pixels)
        .await
        .expect("publish frame");

    let got = read_descriptor(&mut probe).await;
    assert_eq!(got.camera_id, cam, "camera id crosses the bus");
    assert_eq!(got.frame_id, 7, "frame id crosses the bus");
    assert_eq!(got.seq, published.seq, "ring seq matches what was written");
    assert_eq!(got.ts_ms, 1234);
    assert_eq!(got.width, 8);
    assert_eq!(got.height, 8);
    assert_eq!(got.format, FrameFormat::Rgb24);
    assert_eq!(
        got.byte_len as usize,
        pixels.len(),
        "declared byte length matches the published pixels"
    );
    assert_eq!(got.byte_len, published.byte_len);

    bus.shutdown().await;
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn atlas_reader_surfaces_the_published_pixels() {
    // Linux-only: the full atlas ingest hop. VisionFrameSource subscribes to the
    // descriptor bus AND copies the pixels out of the /dev/shm ring the
    // descriptor names — the exact seam that silently breaks when the two sides
    // disagree on the contract. The byte-exact pixel assertion is the
    // regression net.
    use std::collections::HashSet;

    use ados_atlas::VisionFrameSource;

    let bus = Bus::start(4);
    let mut probe = connect_with_retry(&bus.sock, 100, Duration::from_millis(20))
        .await
        .expect("connect to frame bus");
    tokio::time::sleep(Duration::from_millis(50)).await;

    let cam = unique_camera("pixels");
    let pixels = rgb24_pattern(37);
    let published = bus
        .engine
        .publish_frame(&cam, 3, 5000, 8, 8, FrameFormat::Rgb24, &pixels)
        .await
        .expect("publish frame");
    // Real readback: the descriptor was broadcast and is retained as the bus's
    // last value, so the reader that connects next receives it deterministically.
    let seen = read_descriptor(&mut probe).await;
    assert_eq!(seen.camera_id, cam);

    let mut src = VisionFrameSource::new(bus.sock.clone(), HashSet::from([cam.clone()]));
    let frame = tokio::time::timeout(Duration::from_secs(2), src.next())
        .await
        .expect("atlas frame source timed out")
        .expect("atlas surfaced no frame");

    assert_eq!(frame.camera_id, cam);
    assert_eq!(frame.ts_ms, 5000);
    assert_eq!(frame.width, 8);
    assert_eq!(frame.height, 8);
    assert_eq!(frame.format, FrameFormat::Rgb24);
    assert_eq!(frame.bytes.len(), published.byte_len as usize);
    assert_eq!(
        frame.bytes, pixels,
        "atlas must read the exact pixels vision published, byte for byte"
    );

    bus.shutdown().await;
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn atlas_reader_filters_a_disabled_camera() {
    // Linux-only loud negative: a descriptor whose camera id is not in the
    // enabled set must be skipped, and the enabled camera's frame surfaced with
    // ITS pixels — never the disabled camera's. If the filter regresses, the
    // reader surfaces the disabled frame first and every assertion below fails.
    use std::collections::HashSet;

    use ados_atlas::VisionFrameSource;

    let bus = Bus::start(4);
    let mut probe = connect_with_retry(&bus.sock, 100, Duration::from_millis(20))
        .await
        .expect("connect to frame bus");
    tokio::time::sleep(Duration::from_millis(50)).await;

    let enabled_cam = unique_camera("enabled");
    let disabled_cam = unique_camera("disabled");
    let mut src = VisionFrameSource::new(bus.sock.clone(), HashSet::from([enabled_cam.clone()]));

    // Prime: publish + surface one enabled-camera frame. Its successful return is
    // the real readback proving the reader is connected and streaming, so the
    // ordered publishes below reach it live and in order (no sleep-as-sync).
    let prime = rgb24_pattern(1);
    bus.engine
        .publish_frame(&enabled_cam, 1, 100, 8, 8, FrameFormat::Rgb24, &prime)
        .await
        .expect("publish prime");
    // Confirm the prime is broadcast + retained before the reader connects.
    let seen = read_descriptor(&mut probe).await;
    assert_eq!(seen.camera_id, enabled_cam);
    let primed = tokio::time::timeout(Duration::from_secs(2), src.next())
        .await
        .expect("prime frame timed out")
        .expect("no prime frame");
    assert_eq!(primed.camera_id, enabled_cam);
    assert_eq!(primed.bytes, prime);

    // Now the reader is live. Publish a DISABLED-camera frame, then the wanted
    // ENABLED-camera frame, in that order. The reader must skip the first.
    let disabled_px = vec![200u8; FrameFormat::Rgb24.frame_bytes(8, 8)];
    let wanted_px = rgb24_pattern(83);
    bus.engine
        .publish_frame(
            &disabled_cam,
            2,
            200,
            8,
            8,
            FrameFormat::Rgb24,
            &disabled_px,
        )
        .await
        .expect("publish disabled");
    bus.engine
        .publish_frame(&enabled_cam, 3, 300, 8, 8, FrameFormat::Rgb24, &wanted_px)
        .await
        .expect("publish enabled");

    let got = tokio::time::timeout(Duration::from_secs(2), src.next())
        .await
        .expect("enabled frame timed out")
        .expect("no enabled frame after the disabled one");
    assert_eq!(
        got.camera_id, enabled_cam,
        "the disabled camera's descriptor must be filtered out"
    );
    assert_eq!(got.ts_ms, 300);
    assert_eq!(
        got.bytes, wanted_px,
        "the enabled camera's pixels surface, never the disabled camera's"
    );
    assert_ne!(
        got.bytes, disabled_px,
        "the disabled camera's pixels must never surface"
    );

    bus.shutdown().await;
}
