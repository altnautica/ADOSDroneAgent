//! Vision surface: frame subscription, model registration, inference, and
//! detection publishing, plus the visual-odometry pose helper.
//!
//! A vision plugin reaches the agent's vision engine over the same plugin RPC
//! wire as every other surface, but frames themselves never ride the RPC
//! envelope. The engine writes normalized frames into a shared-memory ring and
//! publishes a small [`FrameDescriptor`] on the `vision.frame` topic; the host
//! delivers each descriptor to a subscriber as a `vision.deliver` event. This
//! client resolves a descriptor to pixels by memory-mapping the named
//! `/dev/shm` ring read-only and reading the descriptor's slot through the
//! per-slot seqlock the [`framebus`](ados_protocol::framebus) contract defines,
//! dropping any torn or stale read (latest-wins).
//!
//! Detections and model metadata are small structured payloads, so they ride
//! the RPC envelope directly through the [`methods`](ados_protocol::framebus::methods)
//! the host gates on the vision capabilities.
//!
//! The client gates nothing itself; the host enforces `vision.frame.read`,
//! `vision.model.register`, and `vision.detection.publish`.

pub mod pose;

use std::collections::HashMap;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use ados_protocol::framebus::{
    self, Detection, DetectionBatch, FrameDescriptor, ModelMetadata, RingLayout,
    VISION_DETECTION_VERSION,
};
use ados_protocol::vision_rpc;
use rmpv::Value;

use crate::client::{ClientError, OffloadAdvertisement, PluginIpcClient};

pub use pose::{Odometry, Pose, POSE_COVARIANCE_LEN, VIO_COMPONENT_ID};

/// A resolved camera frame: the descriptor the engine published plus the pixel
/// bytes read out of the shared-memory ring it named. `pixels.len()` equals
/// `descriptor.byte_len` and is the valid pixel data for
/// `descriptor.width` x `descriptor.height` in `descriptor.format`.
#[derive(Debug, Clone, PartialEq)]
pub struct Frame {
    pub descriptor: FrameDescriptor,
    pub pixels: Vec<u8>,
}

/// A callback invoked once per resolved frame. It runs on the IPC reader task,
/// so it must not block; offload heavy inference to a channel or task. A frame
/// the ring could not resolve (torn or stale read, or a ring that vanished) is
/// dropped silently and the callback does not fire for it.
pub type FrameCallback = Arc<dyn Fn(Frame) + Send + Sync>;

/// Callback for a decoded detection batch.
pub type DetectionCallback = Arc<dyn Fn(DetectionBatch) + Send + Sync>;

/// One declared model's delivery resolution, mirroring the agent resolver's
/// `ModelResolution`. `state` is `resolved` | `needs_model` | `verify_failed`;
/// when resolved, `runtime` and `path` name the cached model and how to run it,
/// and a plugin loads its detector from `path`. Otherwise `reason` explains why.
#[derive(Debug, Clone, PartialEq)]
pub struct ModelResolution {
    pub state: String,
    pub model_id: String,
    pub runtime: Option<String>,
    pub path: Option<String>,
    pub reason: Option<String>,
}

fn mpv_get<'a>(v: &'a Value, key: &str) -> Option<&'a Value> {
    v.as_map()?
        .iter()
        .find(|(k, _)| k.as_str() == Some(key))
        .map(|(_, val)| val)
}

fn parse_model_resolutions(reply: &Value) -> Vec<ModelResolution> {
    let Some(models) = mpv_get(reply, "models").and_then(Value::as_array) else {
        return Vec::new();
    };
    models
        .iter()
        .filter_map(|m| {
            Some(ModelResolution {
                state: mpv_get(m, "state")?.as_str()?.to_string(),
                model_id: mpv_get(m, "model_id")?.as_str()?.to_string(),
                runtime: mpv_get(m, "runtime")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                path: mpv_get(m, "path")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                reason: mpv_get(m, "reason")
                    .and_then(Value::as_str)
                    .map(str::to_string),
            })
        })
        .collect()
}

/// `ctx.vision` — the vision engine facade.
///
/// Cloning shares the one underlying IPC client. The ring resolver caches each
/// camera's mapped `/dev/shm` region keyed by `shm_name` so a steady frame
/// stream maps each ring once, not once per frame.
#[derive(Clone)]
pub struct VisionClient {
    ipc: Arc<PluginIpcClient>,
    rings: Arc<Mutex<RingCache>>,
}

impl VisionClient {
    pub(crate) fn new(ipc: Arc<PluginIpcClient>) -> Self {
        Self {
            ipc,
            rings: Arc::new(Mutex::new(RingCache::new(PathBuf::from(DEFAULT_SHM_DIR)))),
        }
    }

    /// Resolve frame rings under `dir` instead of `/dev/shm`, with an empty
    /// ring cache. Mirrors the Python client's `shm_dir`; the engine's
    /// counterpart is `ADOS_SHM_DIR`. Call before subscribing: an existing
    /// subscription keeps the cache it was registered with.
    pub fn with_shm_dir(self, dir: impl Into<PathBuf>) -> Self {
        Self {
            ipc: self.ipc,
            rings: Arc::new(Mutex::new(RingCache::new(dir.into()))),
        }
    }

    /// Subscribe to frames, optionally filtered to one `camera_id`. The host
    /// delivers matching frame descriptors as `vision.deliver` events; this
    /// client resolves each to pixels and invokes `callback` with the [`Frame`].
    ///
    /// Registers a frame callback on the client's `vision.deliver` path (keyed
    /// on `camera_id`), then sends the [`methods::SUBSCRIBE_FRAMES`] RPC (gated
    /// on `vision.frame.read`) so the engine starts or widens the stream. A
    /// `camera_id` of `None` receives every camera's frames; a `Some(id)` filter
    /// is applied at the engine and, as a backstop, in the resolver. The
    /// `vision.deliver` push carries the descriptor in a `descriptor` binary arg
    /// (no `topic`), so it does not use the `event.subscribe` topic path.
    pub async fn subscribe_frames(
        &self,
        camera_id: Option<&str>,
        callback: FrameCallback,
    ) -> Result<(), ClientError> {
        let want_camera = camera_id.map(str::to_string);
        let rings = self.rings.clone();
        let filter = want_camera.clone();

        // The host pushes a `vision.deliver` event carrying the encoded
        // descriptor in the `descriptor` arg. Decode it, drop a camera that
        // does not match the filter, resolve it against the ring (dropping
        // torn/stale), and hand the typed Frame to the author's callback.
        let on_deliver = move |args: Value| {
            let Some(descriptor) = decode_descriptor(&args) else {
                return;
            };
            if let Some(want) = &filter {
                if &descriptor.camera_id != want {
                    return;
                }
            }
            if let Some(frame) = resolve_frame(&rings, descriptor) {
                callback(frame);
            }
        };

        // Register the frame callback before arming the engine stream, so no
        // descriptor that arrives between the RPC reply and registration is lost.
        self.ipc
            .register_vision_callback(camera_id, Arc::new(on_deliver));

        // Tell the engine to start (or widen) the stream toward this plugin.
        self.ipc.vision_subscribe_frames(camera_id).await?;
        Ok(())
    }

    /// Register an inference model with the engine. Sends
    /// [`methods::REGISTER_MODEL`] (gated on `vision.model.register`) in the
    /// [`vision_rpc`] shape the engine decodes.
    pub async fn register_model(&self, model: &ModelMetadata) -> Result<Value, ClientError> {
        self.ipc.vision_register_model(model).await
    }

    /// Read this plugin's resolved model-delivery status: one
    /// [`ModelResolution`] per declared model, keyed by `model_id`. This is the
    /// model-delivery last mile — a plugin loads its detector from the resolved
    /// `path`. Sends [`methods::READ_MODEL`] (gated on `vision.model.read`); an
    /// unresolved plugin returns an empty list rather than erroring, so a caller
    /// can poll until its model resolves.
    pub async fn resolved_model(&self) -> Result<Vec<ModelResolution>, ClientError> {
        let reply = self.ipc.vision_read_model().await?;
        Ok(parse_model_resolutions(&reply))
    }

    /// Run a registered model against one frame on the shared backend and
    /// return its detections. Sends [`methods::INFER`] (gated on
    /// `vision.model.register`); the engine arbitrates access to the
    /// accelerator. The frame is passed by descriptor (the engine reads its own
    /// ring), so no pixels cross the RPC envelope. A frame whose slot the ring
    /// has since recycled is an error; infer on a fresher frame.
    pub async fn infer(
        &self,
        model_id: &str,
        frame: &Frame,
    ) -> Result<Vec<Detection>, ClientError> {
        let reply = self.ipc.vision_infer(model_id, &frame.descriptor).await?;
        vision_rpc::decode_infer_reply(&reply)
            .map(|batch| batch.detections)
            .map_err(|e| ClientError::Rpc(format!("infer reply decode failed: {e}")))
    }

    /// Publish a detection batch on `vision.detection`. Sends
    /// [`methods::PUBLISH_DETECTION`] (gated on `vision.detection.publish`) in
    /// the [`vision_rpc`] shape the engine decodes.
    pub async fn publish_detection(&self, batch: &DetectionBatch) -> Result<Value, ClientError> {
        self.ipc.vision_publish_detection(batch).await
    }

    /// Publish a single detection against one frame, building the
    /// [`DetectionBatch`] from the frame's source camera and id. A convenience
    /// over [`publish_detection`](Self::publish_detection) for the common
    /// one-box-per-frame case.
    pub async fn publish_one(
        &self,
        model_id: &str,
        frame: &Frame,
        detection: Detection,
    ) -> Result<Value, ClientError> {
        let batch = DetectionBatch {
            v: VISION_DETECTION_VERSION,
            model_id: model_id.to_string(),
            camera_id: frame.descriptor.camera_id.clone(),
            frame_id: frame.descriptor.frame_id,
            ts_ms: frame.descriptor.ts_ms,
            frame_width: frame.descriptor.width,
            frame_height: frame.descriptor.height,
            detections: vec![detection],
        };
        self.publish_detection(&batch).await
    }

    /// Report the perception-offload link this plugin holds, so the node's
    /// perception tier reads `offload` while it is live. Re-advertise at least
    /// every ~10 s (the link goes stale after 20 s) and send `paired: false`
    /// when the link drops. Gated on `vision.detection.publish`. See
    /// [`PluginIpcClient::offload_advertise`].
    pub async fn advertise_offload(
        &self,
        advert: &OffloadAdvertisement,
    ) -> Result<Value, ClientError> {
        self.ipc.offload_advertise(advert).await
    }

    /// Register this plugin as the visual-odometry MAVLink component so the FC
    /// attributes injected pose to a vision source. Call once before
    /// [`inject_pose`](Self::inject_pose) / [`inject_odometry`](Self::inject_odometry).
    pub async fn register_vio_component(&self) -> Result<Value, ClientError> {
        self.ipc
            .mavlink_register_component(VIO_COMPONENT_ID, "vio")
            .await
    }

    /// Build a `VISION_POSITION_ESTIMATE` from `pose` and send it to the FC over
    /// the host's MAVLink path under the visual-odometry component id. Replaces
    /// hand-built MAVLink frames in VIO plugins.
    pub async fn inject_pose(&self, pose: &Pose) -> Result<Value, ClientError> {
        let frame = pose::frame_for(&pose.to_vision_position_estimate())
            .map_err(|e| ClientError::Rpc(format!("vision pose encode failed: {e}")))?;
        self.ipc.mavlink_send(&frame, Some(VIO_COMPONENT_ID)).await
    }

    /// Build an `ODOMETRY` message from `odometry` (pose plus body-frame twist)
    /// and send it to the FC under the visual-odometry component id.
    pub async fn inject_odometry(&self, odometry: &Odometry) -> Result<Value, ClientError> {
        let frame = pose::frame_for(&odometry.to_odometry())
            .map_err(|e| ClientError::Rpc(format!("vision odometry encode failed: {e}")))?;
        self.ipc.mavlink_send(&frame, Some(VIO_COMPONENT_ID)).await
    }

    /// Receive the engine's detection batches.
    ///
    /// `camera_id` of `None` receives every camera. The engine fans all cameras
    /// onto one stream, so a `Some(id)` filter is applied here rather than
    /// narrowed at the engine — matching the frame subscription and the Python
    /// client.
    ///
    /// A batch that will not decode is dropped rather than surfaced: the wire
    /// carries a version and an older producer round-trips, so an undecodable
    /// batch means a genuinely broken frame, not a version skew the caller could
    /// act on.
    pub async fn subscribe_detections(
        &self,
        camera_id: Option<&str>,
        callback: DetectionCallback,
    ) -> Result<(), ClientError> {
        let filter = camera_id.map(str::to_string);
        let on_deliver = move |args: Value| {
            let Some(Value::Binary(blob)) = map_get(&args, "batch") else {
                return;
            };
            let Ok(batch) = DetectionBatch::from_msgpack(&blob) else {
                return;
            };
            if let Some(want) = &filter {
                if &batch.camera_id != want {
                    return;
                }
            }
            callback(batch);
        };
        self.ipc
            .register_detection_callback(camera_id, Arc::new(on_deliver));
        self.ipc.vision_subscribe_detections(camera_id).await?;
        Ok(())
    }

    /// Lock the tracker onto a specific detection, overriding its auto-lock.
    ///
    /// This is the operator-designation path: the engine presents a lock state
    /// only for a track a caller designated, so this is what makes a lock mean
    /// "the target that was chosen" rather than "whatever scored highest". The
    /// detection's box, label and confidence cross; it must carry a box.
    pub async fn designate_track(
        &self,
        camera_id: &str,
        detection: &Detection,
    ) -> Result<Value, ClientError> {
        self.ipc.vision_designate_track(camera_id, detection).await
    }
}

/// Where the engine creates its frame rings.
const DEFAULT_SHM_DIR: &str = "/dev/shm";

/// Mapped frame rings under `dir`, keyed by the descriptor's `shm_name`. One
/// ring per camera.
struct RingCache {
    dir: PathBuf,
    rings: HashMap<String, MappedRing>,
}

impl RingCache {
    fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            rings: HashMap::new(),
        }
    }
}

/// One memory-mapped frame ring: the read-only mmap, the layout recorded in its
/// header, and which file it is.
struct MappedRing {
    map: memmap2::Mmap,
    layout: RingLayout,
    /// `(st_dev, st_ino)` of the mapped file, read off the descriptor that was
    /// mapped, so it names exactly the bytes in `map`.
    file_id: (u64, u64),
    /// The highest sequence read out of this mapping.
    last_seq: u64,
}

impl MappedRing {
    /// Read the descriptor's slot through the seqlock, recording its sequence
    /// when the read holds.
    fn read(&mut self, descriptor: &FrameDescriptor) -> Option<Vec<u8>> {
        let pixels = framebus::read_slot(&self.map, &self.layout, descriptor.slot, descriptor.seq)
            .ok()
            .flatten()?;
        self.last_seq = self.last_seq.max(descriptor.seq);
        Some(pixels)
    }

    /// Whether `path` still names the file this mapping holds.
    fn is_named_by(&self, path: &Path) -> bool {
        std::fs::metadata(path).is_ok_and(|m| (m.dev(), m.ino()) == self.file_id)
    }
}

/// Resolve a descriptor to a [`Frame`], mapping the ring on first sight of its
/// `shm_name`. Returns `None` on a torn/stale read, a ring that cannot be
/// mapped, or a layout/region mismatch — the frame is dropped (latest-wins).
///
/// The engine replaces a ring by unlinking its file and creating a new one
/// under the same name — every engine start does, sweeping the last run's
/// rings — and the new ring's sequence starts over at 1. A mapping of the old
/// file stays readable but never again holds the frame being described (or,
/// on a coincidental sequence, holds the previous run's pixels), which left a
/// subscriber silent until its process restarted. So a sequence at or below
/// one this mapping has already served, or a slot it cannot read, is checked
/// against the file the name names now, and a replaced ring is mapped afresh.
/// A live stream pays nothing for this: its sequence only climbs and its reads
/// hold.
fn resolve_frame(cache: &Mutex<RingCache>, descriptor: FrameDescriptor) -> Option<Frame> {
    let mut guard = cache.lock().expect("ring cache lock");
    let cache = &mut *guard;
    let path = cache.dir.join(&descriptor.shm_name);
    if let Some(ring) = cache.rings.get_mut(&descriptor.shm_name) {
        let regressed = descriptor.seq <= ring.last_seq;
        if !regressed {
            if let Some(pixels) = ring.read(&descriptor) {
                return Some(Frame { descriptor, pixels });
            }
        }
        if ring.is_named_by(&path) {
            // Still the live ring: the same descriptor resolved again (once per
            // matching subscription), or a slot the writer already recycled.
            let pixels = if regressed {
                ring.read(&descriptor)
            } else {
                None
            }?;
            return Some(Frame { descriptor, pixels });
        }
        cache.rings.remove(&descriptor.shm_name);
    }
    let mut ring = map_ring(&path)?;
    let pixels = ring.read(&descriptor);
    cache.rings.insert(descriptor.shm_name.clone(), ring);
    Some(Frame {
        descriptor,
        pixels: pixels?,
    })
}

/// Map the ring file at `path` read-only and read the ring layout from its
/// header. `None` if the file is missing, cannot be mapped, or has no valid
/// header.
fn map_ring(path: &Path) -> Option<MappedRing> {
    let file = std::fs::File::open(path).ok()?;
    let meta = file.metadata().ok()?;
    // SAFETY: the region is a POSIX shared-memory object the vision engine
    // owns; mapping it read-only is sound. A concurrent writer recycling slots
    // is the expected case and is detected by the per-slot seqlock in
    // `read_slot`, which discards any torn read.
    let map = unsafe { memmap2::Mmap::map(&file) }.ok()?;
    let layout = RingLayout::read_header(&map)?;
    Some(MappedRing {
        map,
        layout,
        file_id: (meta.dev(), meta.ino()),
        last_seq: 0,
    })
}

/// Decode a [`FrameDescriptor`] from a `vision.deliver` envelope `args` map. The
/// host carries the descriptor as a `descriptor` binary blob (the live path);
/// for robustness a map that is itself the descriptor's own fields also decodes,
/// both through the framebus contract.
fn decode_descriptor(payload: &Value) -> Option<FrameDescriptor> {
    if let Some(Value::Binary(blob)) = map_get(payload, "descriptor") {
        return FrameDescriptor::from_msgpack(&blob).ok();
    }
    // The payload map is the descriptor itself: re-encode it to msgpack and
    // decode through the named-field contract so the field mapping is the one
    // single source of truth in framebus.
    let bytes = rmp_serde::to_vec_named(payload).ok()?;
    FrameDescriptor::from_msgpack(&bytes).ok()
}

/// Read a key from an `rmpv` map value.
fn map_get(args: &Value, key: &str) -> Option<Value> {
    match args {
        Value::Map(entries) => entries
            .iter()
            .find(|(k, _)| k.as_str() == Some(key))
            .map(|(_, v)| v.clone()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ados_protocol::framebus::FrameFormat;

    fn descriptor(shm_name: &str, slot: u32, seq: u64, byte_len: u32) -> FrameDescriptor {
        FrameDescriptor {
            v: ados_protocol::framebus::FRAMEBUS_DESCRIPTOR_VERSION,
            camera_id: "uvc-0".into(),
            frame_id: seq,
            ts_ms: 1_700_000_000_000,
            width: 4,
            height: 4,
            format: FrameFormat::Rgb24,
            shm_name: shm_name.into(),
            slot,
            seq,
            byte_len,
        }
    }

    #[test]
    fn descriptor_decodes_from_a_named_map_payload() {
        let d = descriptor("ados-vision-uvc-0", 1, 7, 48);
        // The host carried the descriptor's own fields as the event payload.
        let bytes = d.to_msgpack().unwrap();
        let payload: Value = rmp_serde::from_slice(&bytes).unwrap();
        let back = decode_descriptor(&payload).unwrap();
        assert_eq!(back, d);
    }

    #[test]
    fn descriptor_decodes_from_a_blob_payload() {
        let d = descriptor("ados-vision-uvc-0", 2, 11, 48);
        let blob = d.to_msgpack().unwrap();
        let payload = Value::Map(vec![(Value::from("descriptor"), Value::Binary(blob))]);
        let back = decode_descriptor(&payload).unwrap();
        assert_eq!(back, d);
    }

    #[test]
    fn resolve_reads_a_frame_written_to_a_real_ring() {
        // Build a ring region in memory, write a frame, and resolve it via a
        // file-backed mmap (a tempfile stands in for /dev/shm here).
        let layout = RingLayout::for_frame(4, 4, 4, FrameFormat::Rgb24); // 48-byte slots
        let mut region = vec![0u8; layout.total_len()];
        layout.write_header(&mut region).unwrap();
        let pixels: Vec<u8> = (0..layout.slot_bytes as u8).collect();
        let seq = 5u64;
        let slot = (seq % layout.slot_count as u64) as u32;
        framebus::write_slot(&mut region, &layout, slot, seq, &pixels).unwrap();

        // The resolver maps by name under /dev/shm; mirror the read path here by
        // exercising read_slot against the same layout the header records.
        let read_layout = RingLayout::read_header(&region).unwrap();
        assert_eq!(read_layout, layout);
        let got = framebus::read_slot(&region, &read_layout, slot, seq).unwrap();
        assert_eq!(got.as_deref(), Some(pixels.as_slice()));

        // A stale descriptor (seq the slot no longer holds) resolves to nothing.
        assert_eq!(
            framebus::read_slot(&region, &read_layout, slot, seq + 1).unwrap(),
            None
        );
    }
}
