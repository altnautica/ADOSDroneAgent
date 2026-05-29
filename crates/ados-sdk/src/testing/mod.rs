//! Test harness for vision plugins.
//!
//! Mirrors the Python `ados.sdk.testing` ergonomics for the Rust SDK: a plugin
//! author exercises a frame-consuming plugin without a real plugin host, vision
//! engine, shared memory, or socket. [`FakeVisionEngine`] emits synthetic
//! frames (from an in-memory list or a directory of raw frame files) into the
//! same [`FrameCallback`](crate::vision::FrameCallback) a plugin registers with
//! `ctx.vision.subscribe_frames`, and captures the [`DetectionBatch`]es the
//! plugin would publish so a test can assert against them.
//!
//! The fake builds a real frame ring through the
//! [`framebus`](ados_protocol::framebus) contract and resolves each synthetic
//! frame the same way the production client does (per-slot seqlock, latest-wins,
//! torn/stale reads dropped), so a plugin's frame-handling path is exercised
//! end to end, only the host and the OS shared-memory are faked.
//!
//! ```no_run
//! use ados_sdk::testing::FakeVisionEngine;
//! use ados_protocol::framebus::{FrameFormat, Detection, BoundingBox};
//! use std::sync::{Arc, Mutex};
//!
//! # async fn demo() {
//! let mut engine = FakeVisionEngine::new("uvc-0", 64, 48, FrameFormat::Rgb24);
//! let seen = Arc::new(Mutex::new(0usize));
//! let s = seen.clone();
//! engine.on_frame(move |frame| {
//!     // a plugin's per-frame handler runs here
//!     assert_eq!(frame.descriptor.camera_id, "uvc-0");
//!     *s.lock().unwrap() += 1;
//! });
//! engine.push_solid(0x80); // one grey frame
//! engine.deliver_all();
//! assert_eq!(*seen.lock().unwrap(), 1);
//! # }
//! ```

use std::path::Path;
use std::sync::{Arc, Mutex};

use ados_protocol::framebus::{self, DetectionBatch, FrameDescriptor, FrameFormat, RingLayout};

use crate::vision::{Frame, FrameCallback};

/// Default slot count for the fake ring. Large enough that the harness never
/// recycles a slot under a pending read in a single-threaded test.
const DEFAULT_SLOT_COUNT: u32 = 8;

/// One captured published detection, in publish order.
pub type CapturedDetection = DetectionBatch;

/// In-process stand-in for the vision engine + plugin host bridge.
///
/// Owns a synthetic frame ring and a queue of pending frames. A test registers
/// a [`FrameCallback`] with [`on_frame`](Self::on_frame), enqueues synthetic
/// frames, then calls [`deliver_all`](Self::deliver_all) (or
/// [`deliver_one`](Self::deliver_one)) to drive them through the resolver into
/// the callback. Detections the plugin publishes are captured via the sink
/// [`detection_sink`](Self::detection_sink) returns.
pub struct FakeVisionEngine {
    camera_id: String,
    shm_name: String,
    format: FrameFormat,
    layout: RingLayout,
    region: Vec<u8>,
    /// Monotonic frame sequence, also the ring slot via `seq % slot_count`.
    next_seq: u64,
    /// Pixel payloads waiting to be delivered.
    pending: Vec<Vec<u8>>,
    callback: Option<FrameCallback>,
    captured: Arc<Mutex<Vec<CapturedDetection>>>,
}

impl FakeVisionEngine {
    /// A fake engine for one camera at a fixed frame size and format. The ring
    /// is sized to hold full `width` x `height` frames.
    pub fn new(camera_id: &str, width: u32, height: u32, format: FrameFormat) -> Self {
        Self::with_slot_count(camera_id, width, height, format, DEFAULT_SLOT_COUNT)
    }

    /// As [`new`](Self::new) but with an explicit slot count, for tests that
    /// want to force slot recycling and exercise the torn/stale-read drop path.
    pub fn with_slot_count(
        camera_id: &str,
        width: u32,
        height: u32,
        format: FrameFormat,
        slot_count: u32,
    ) -> Self {
        let layout = RingLayout::for_frame(slot_count, width, height, format);
        let mut region = vec![0u8; layout.total_len()];
        layout
            .write_header(&mut region)
            .expect("freshly sized region holds its own header");
        Self {
            camera_id: camera_id.to_string(),
            shm_name: format!("ados-vision-{camera_id}"),
            format,
            layout,
            region,
            next_seq: 0,
            pending: Vec::new(),
            callback: None,
            captured: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// The frame size of one full frame in this ring's format.
    pub fn frame_bytes(&self) -> usize {
        self.layout.slot_bytes as usize
    }

    /// Register the per-frame callback the plugin under test would pass to
    /// `ctx.vision.subscribe_frames`. Replaces any prior callback.
    pub fn on_frame<F>(&mut self, callback: F)
    where
        F: Fn(Frame) + Send + Sync + 'static,
    {
        self.callback = Some(Arc::new(callback));
    }

    /// Register a callback as a pre-built [`FrameCallback`] (e.g. the exact
    /// `Arc` a plugin handed to the SDK).
    pub fn on_frame_arc(&mut self, callback: FrameCallback) {
        self.callback = Some(callback);
    }

    /// Enqueue a raw pixel frame. The bytes must be at most one full frame
    /// (`frame_bytes`); a shorter slice is delivered as a partial frame, which
    /// is what a real engine does for a truncated capture.
    pub fn push_frame(&mut self, pixels: Vec<u8>) {
        self.pending.push(pixels);
    }

    /// Enqueue a frame filled with one byte value (a flat colour), sized to a
    /// full frame. Handy for asserting the callback sees the right bytes.
    pub fn push_solid(&mut self, value: u8) {
        self.pending.push(vec![value; self.frame_bytes()]);
    }

    /// Enqueue every `*.bin` / `*.raw` file in `dir` (sorted by name) as a
    /// frame, reading each file's bytes verbatim. Returns the count enqueued.
    pub fn push_dir(&mut self, dir: impl AsRef<Path>) -> std::io::Result<usize> {
        let mut paths: Vec<_> = std::fs::read_dir(dir)?
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| {
                matches!(
                    p.extension().and_then(|s| s.to_str()),
                    Some("bin") | Some("raw")
                )
            })
            .collect();
        paths.sort();
        let mut n = 0;
        for p in paths {
            let bytes = std::fs::read(&p)?;
            self.pending.push(bytes);
            n += 1;
        }
        Ok(n)
    }

    /// Deliver the next pending frame: write it into the ring, build its
    /// descriptor, resolve it through the seqlock, and invoke the callback.
    /// Returns `true` if a frame was delivered (a frame existed and resolved),
    /// `false` if the queue was empty. A frame that fails to resolve (a forced
    /// torn read) returns `true` for "consumed" but does not fire the callback.
    pub fn deliver_one(&mut self) -> bool {
        if self.pending.is_empty() {
            return false;
        }
        let pixels = self.pending.remove(0);
        let seq = self.next_seq + 1;
        self.next_seq = seq;
        let slot = (seq % self.layout.slot_count as u64) as u32;
        framebus::write_slot(&mut self.region, &self.layout, slot, seq, &pixels)
            .expect("synthetic frame fits a full-size slot");

        let descriptor = FrameDescriptor {
            camera_id: self.camera_id.clone(),
            frame_id: seq,
            ts_ms: 1_700_000_000_000 + seq as i64,
            width: 0, // width/height are descriptive; the resolver keys on byte_len.
            height: 0,
            format: self.layout_format(),
            shm_name: self.shm_name.clone(),
            slot,
            seq,
            byte_len: pixels.len() as u32,
        };

        if let Some(frame) = self.resolve(descriptor) {
            if let Some(cb) = &self.callback {
                cb(frame);
            }
        }
        true
    }

    /// Deliver every pending frame in order. Returns the number delivered.
    pub fn deliver_all(&mut self) -> usize {
        let mut n = 0;
        while self.deliver_one() {
            n += 1;
        }
        n
    }

    /// The shared detection sink. Pass this into a plugin (or call
    /// [`capture`](Self::capture) directly) so detections the plugin would
    /// publish via `ctx.vision.publish_detection` are recorded here.
    pub fn detection_sink(&self) -> Arc<Mutex<Vec<CapturedDetection>>> {
        self.captured.clone()
    }

    /// Record a detection batch the plugin under test published, as if the
    /// engine received it. A test that cannot route the real publish call can
    /// invoke this from a stubbed publish path.
    pub fn capture(&self, batch: DetectionBatch) {
        self.captured.lock().expect("capture lock").push(batch);
    }

    /// The captured detections in publish order. Returns a copy.
    pub fn captured_detections(&self) -> Vec<CapturedDetection> {
        self.captured.lock().expect("capture lock").clone()
    }

    /// Clear the captured detections.
    pub fn clear_captured(&self) {
        self.captured.lock().expect("capture lock").clear();
    }

    /// Resolve a descriptor against the in-memory ring (no `/dev/shm`),
    /// validating the seqlock exactly as the production resolver does.
    fn resolve(&self, descriptor: FrameDescriptor) -> Option<Frame> {
        let pixels =
            framebus::read_slot(&self.region, &self.layout, descriptor.slot, descriptor.seq)
                .ok()
                .flatten()?;
        Some(Frame { descriptor, pixels })
    }

    /// The format the ring was sized for. The ring header records only
    /// `slot_bytes`, so the harness keeps the format it was built with.
    fn layout_format(&self) -> FrameFormat {
        self.format
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ados_protocol::framebus::{BoundingBox, Detection};

    #[test]
    fn delivers_a_solid_frame_to_the_callback() {
        let mut engine = FakeVisionEngine::new("uvc-0", 4, 4, FrameFormat::Rgb24);
        let seen: Arc<Mutex<Vec<Frame>>> = Arc::new(Mutex::new(Vec::new()));
        let s = seen.clone();
        engine.on_frame(move |frame| s.lock().unwrap().push(frame));

        engine.push_solid(0x42);
        assert_eq!(engine.deliver_all(), 1);

        let got = seen.lock().unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].descriptor.camera_id, "uvc-0");
        assert_eq!(got[0].descriptor.frame_id, 1);
        assert_eq!(got[0].pixels.len(), engine.frame_bytes());
        assert!(got[0].pixels.iter().all(|&b| b == 0x42));
    }

    #[test]
    fn delivers_frames_in_order() {
        let mut engine = FakeVisionEngine::new("uvc-0", 2, 2, FrameFormat::Rgb24);
        let ids: Arc<Mutex<Vec<u64>>> = Arc::new(Mutex::new(Vec::new()));
        let i = ids.clone();
        engine.on_frame(move |frame| i.lock().unwrap().push(frame.descriptor.frame_id));

        engine.push_solid(1);
        engine.push_solid(2);
        engine.push_solid(3);
        assert_eq!(engine.deliver_all(), 3);
        assert_eq!(*ids.lock().unwrap(), vec![1, 2, 3]);
    }

    #[test]
    fn captures_published_detections() {
        let engine = FakeVisionEngine::new("uvc-0", 2, 2, FrameFormat::Rgb24);
        let batch = DetectionBatch {
            model_id: "com.example.weeds".into(),
            camera_id: "uvc-0".into(),
            frame_id: 1,
            ts_ms: 1,
            detections: vec![Detection {
                bbox: BoundingBox {
                    x: 0.0,
                    y: 0.0,
                    width: 1.0,
                    height: 1.0,
                },
                class_label: "weed".into(),
                confidence: 0.5,
                track_id: None,
            }],
        };
        engine.capture(batch.clone());
        let got = engine.captured_detections();
        assert_eq!(got, vec![batch]);
        engine.clear_captured();
        assert!(engine.captured_detections().is_empty());
    }

    #[test]
    fn push_dir_reads_raw_frames_in_name_order() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("b.bin"), [2u8; 12]).unwrap();
        std::fs::write(dir.path().join("a.raw"), [1u8; 12]).unwrap();
        std::fs::write(dir.path().join("skip.txt"), b"not a frame").unwrap();

        let mut engine = FakeVisionEngine::new("uvc-0", 2, 2, FrameFormat::Rgb24); // 12-byte frames
        let n = engine.push_dir(dir.path()).unwrap();
        assert_eq!(n, 2);

        let first_byte: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
        let f = first_byte.clone();
        engine.on_frame(move |frame| f.lock().unwrap().push(frame.pixels[0]));
        engine.deliver_all();
        // a.raw (value 1) sorts before b.bin (value 2).
        assert_eq!(*first_byte.lock().unwrap(), vec![1, 2]);
    }
}
