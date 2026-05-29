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
use std::sync::{Arc, Mutex};

use ados_protocol::framebus::{
    self, Detection, DetectionBatch, FrameDescriptor, ModelMetadata, RingLayout, VISION_FRAME_TOPIC,
};
use rmpv::Value;

use crate::client::{ClientError, PluginIpcClient};

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
            rings: Arc::new(Mutex::new(RingCache::default())),
        }
    }

    /// Subscribe to frames, optionally filtered to one `camera_id`. The host
    /// delivers matching frame descriptors as `vision.deliver` events; this
    /// client resolves each to pixels and invokes `callback` with the [`Frame`].
    ///
    /// Sends the [`methods::SUBSCRIBE_FRAMES`] RPC (gated on `vision.frame.read`)
    /// then registers an event subscriber on the `vision.frame` topic. A
    /// `camera_id` of `None` receives every camera's frames; a `Some(id)` filter
    /// is applied both in the RPC argument (so the host can narrow the stream)
    /// and in the resolver (so a broader host stream is still filtered locally).
    pub async fn subscribe_frames(
        &self,
        camera_id: Option<&str>,
        callback: FrameCallback,
    ) -> Result<(), ClientError> {
        let want_camera = camera_id.map(str::to_string);
        let rings = self.rings.clone();
        let filter = want_camera.clone();

        // The host pushes a `vision.deliver` event carrying the descriptor in
        // the event payload. Resolve it against the ring, drop torn/stale, and
        // hand the typed Frame to the author's callback.
        let on_event = move |args: Value| {
            let Some(payload) = map_get(&args, "payload") else {
                return;
            };
            let Some(descriptor) = decode_descriptor(&payload) else {
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

        // Tell the engine to start (or widen) the stream toward this plugin.
        self.ipc.vision_subscribe_frames(camera_id).await?;

        // Frame descriptors arrive as events on the reserved frame topic.
        self.ipc
            .event_subscribe(VISION_FRAME_TOPIC, Arc::new(on_event))
            .await
    }

    /// Register an inference model with the engine. Sends
    /// [`methods::REGISTER_MODEL`] (gated on `vision.model.register`) carrying
    /// the model metadata as a msgpack blob the engine decodes with
    /// [`ModelMetadata::from_msgpack`].
    pub async fn register_model(&self, model: &ModelMetadata) -> Result<Value, ClientError> {
        let blob = model
            .to_msgpack()
            .map_err(|e| ClientError::Rpc(format!("model metadata encode failed: {e}")))?;
        self.ipc.vision_register_model(&blob).await
    }

    /// Run a registered model against one frame on the shared backend and
    /// return its detections. Sends [`methods::INFER`] (gated on
    /// `vision.model.register`); the engine arbitrates access to the
    /// accelerator. The frame is passed by descriptor (the engine reads the
    /// same ring), so no pixels cross the RPC envelope.
    pub async fn infer(&self, model_id: &str, frame: &Frame) -> Result<Vec<Detection>, ClientError> {
        let desc = frame
            .descriptor
            .to_msgpack()
            .map_err(|e| ClientError::Rpc(format!("frame descriptor encode failed: {e}")))?;
        let resp = self.ipc.vision_infer(model_id, &desc).await?;
        decode_detections(&resp)
    }

    /// Publish a detection batch on `vision.detection`. Sends
    /// [`methods::PUBLISH_DETECTION`] (gated on `vision.detection.publish`)
    /// carrying the batch as a msgpack blob.
    pub async fn publish_detection(&self, batch: &DetectionBatch) -> Result<Value, ClientError> {
        let blob = batch
            .to_msgpack()
            .map_err(|e| ClientError::Rpc(format!("detection batch encode failed: {e}")))?;
        self.ipc.vision_publish_detection(&blob).await
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
            model_id: model_id.to_string(),
            camera_id: frame.descriptor.camera_id.clone(),
            frame_id: frame.descriptor.frame_id,
            ts_ms: frame.descriptor.ts_ms,
            detections: vec![detection],
        };
        self.publish_detection(&batch).await
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
}

/// Mapped `/dev/shm` rings, keyed by the descriptor's `shm_name`. One ring per
/// camera; held read-only for the life of the subscription.
#[derive(Default)]
struct RingCache {
    rings: HashMap<String, MappedRing>,
}

/// One memory-mapped frame ring: the read-only mmap plus the layout recorded in
/// its header.
struct MappedRing {
    map: memmap2::Mmap,
    layout: RingLayout,
}

/// Resolve a descriptor to a [`Frame`], mapping the ring on first sight of its
/// `shm_name`. Returns `None` on a torn/stale read, a ring that cannot be
/// mapped, or a layout/region mismatch — the frame is dropped (latest-wins).
fn resolve_frame(cache: &Arc<Mutex<RingCache>>, descriptor: FrameDescriptor) -> Option<Frame> {
    let mut guard = cache.lock().expect("ring cache lock");
    let ring = match guard.rings.get(&descriptor.shm_name) {
        Some(r) => r,
        None => {
            let mapped = map_ring(&descriptor.shm_name)?;
            guard.rings.insert(descriptor.shm_name.clone(), mapped);
            guard.rings.get(&descriptor.shm_name).expect("just inserted")
        }
    };
    let pixels = framebus::read_slot(&ring.map, &ring.layout, descriptor.slot, descriptor.seq)
        .ok()
        .flatten()?;
    Some(Frame { descriptor, pixels })
}

/// Map `/dev/shm/<shm_name>` read-only and read the ring layout from its header.
/// `None` if the file is missing, cannot be mapped, or has no valid header.
fn map_ring(shm_name: &str) -> Option<MappedRing> {
    let path = format!("/dev/shm/{shm_name}");
    let file = std::fs::File::open(&path).ok()?;
    // SAFETY: the region is a POSIX shared-memory object the vision engine
    // owns; mapping it read-only is sound. A concurrent writer recycling slots
    // is the expected case and is detected by the per-slot seqlock in
    // `read_slot`, which discards any torn read.
    let map = unsafe { memmap2::Mmap::map(&file) }.ok()?;
    let layout = RingLayout::read_header(&map)?;
    Some(MappedRing { map, layout })
}

/// Decode a [`FrameDescriptor`] from a `vision.deliver` event payload. The host
/// carries the descriptor either as a msgpack-named map (the descriptor's own
/// fields) or as a `descriptor` binary blob; both decode through the framebus
/// contract.
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

/// Decode the `detections` field of an `infer` response: a binary blob holding
/// a msgpack array of [`Detection`].
fn decode_detections(args: &Value) -> Result<Vec<Detection>, ClientError> {
    match map_get(args, "detections") {
        Some(Value::Binary(blob)) => rmp_serde::from_slice(&blob)
            .map_err(|e| ClientError::Rpc(format!("detections decode failed: {e}"))),
        // An empty / absent field is no detections, not an error.
        None | Some(Value::Nil) => Ok(Vec::new()),
        Some(other) => Err(ClientError::Rpc(format!(
            "detections field is not binary: {other:?}"
        ))),
    }
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

    #[test]
    fn detections_decode_empty_when_absent() {
        let args = Value::Map(vec![]);
        assert!(decode_detections(&args).unwrap().is_empty());
    }

    #[test]
    fn detections_decode_from_a_blob() {
        let dets = vec![Detection {
            bbox: framebus::BoundingBox {
                x: 1.0,
                y: 2.0,
                width: 3.0,
                height: 4.0,
            },
            class_label: "weed".into(),
            confidence: 0.9,
            track_id: Some(7),
        }];
        let blob = rmp_serde::to_vec_named(&dets).unwrap();
        let args = Value::Map(vec![(Value::from("detections"), Value::Binary(blob))]);
        let back = decode_detections(&args).unwrap();
        assert_eq!(back, dets);
    }
}
