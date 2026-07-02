//! Vision frame transport contract.
//!
//! Camera frames are large (a 640x480 RGB24 frame is ~900 KiB, a 1280x720 one
//! ~2.7 MiB), so they do not travel inside the plugin RPC envelope (capped at
//! 4 MiB and copied through msgpack on a drop-on-full broadcast). Instead the
//! vision engine writes normalized frames into a shared-memory ring and
//! publishes only a small [`FrameDescriptor`] on the `vision.frame` topic. A
//! consumer (a Rust or Python plugin) maps the same ring and reads the slot the
//! descriptor names.
//!
//! This module owns two things so the publisher and every consumer agree
//! byte-for-byte:
//!
//! - the descriptor wire shape ([`FrameDescriptor`], msgpack map with the same
//!   field names in Rust and Python), and
//! - the ring memory layout ([`RingLayout`]) plus the per-slot seqlock used to
//!   detect a torn read when the single writer recycles a slot under a reader.
//!
//! The module is deliberately free of any OS mapping code: it operates on plain
//! byte slices, so it builds and unit-tests on any host. The caller maps
//! `/dev/shm/<shm_name>` (for example with `memmap2`) and hands the slice to
//! [`write_slot`] / [`read_slot`].
//!
//! Detections travel the other way as small structured payloads on the
//! `vision.detection` topic and ride the plugin envelope directly; only frames
//! need the ring.

use std::sync::atomic::{compiler_fence, fence, Ordering};

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Topic the vision engine publishes frame descriptors on. Reserved to the
/// host: plugins subscribe (with `vision.frame.read`) but never publish here.
pub const VISION_FRAME_TOPIC: &str = "vision.frame";

/// The largest slot count a ring header can carry. The header records
/// `slot_count` in two little-endian bytes, so a ring is capped here; a writer
/// that asked for more would diverge from a reader that re-derives the layout
/// from the truncated header field, so it is rejected at header-write time and
/// validated at engine startup. Sane ring depths are single digits, so this
/// cap is never approached in practice.
pub const MAX_SLOT_COUNT: u32 = u16::MAX as u32;

/// Write `value` into `region[off..off+8]` as a little-endian u64 through
/// per-byte volatile stores. Volatile prevents the compiler from eliding or
/// reordering the seq-guard stamps relative to the plain data copy, which the
/// surrounding `fence`s then order across CPUs for a cross-process reader. The
/// region is an `mmap` shared with a separate reader process, so a plain
/// `copy_from_slice` of the guard is not sufficient on a weakly-ordered CPU.
#[inline]
fn store_seq_volatile(region: &mut [u8], off: usize, value: u64) {
    let bytes = value.to_le_bytes();
    let base = region.as_mut_ptr();
    for (i, b) in bytes.iter().enumerate() {
        // SAFETY: the caller validated `off + 8 <= region.len()` (every call
        // site sizes the slot via `check_region` first), so `off + i` is in
        // bounds for `i < 8`.
        unsafe { base.add(off + i).write_volatile(*b) };
    }
}

/// Read `region[off..off+8]` as a little-endian u64 through per-byte volatile
/// loads. The counterpart to [`store_seq_volatile`]; volatile keeps the guard
/// loads from being hoisted or fused with the data copy, and the caller pairs
/// them with `fence(Acquire)`.
#[inline]
fn load_seq_volatile(region: &[u8], off: usize) -> u64 {
    let mut bytes = [0u8; 8];
    let base = region.as_ptr();
    for (i, b) in bytes.iter_mut().enumerate() {
        // SAFETY: the caller validated `off + 8 <= region.len()`, so `off + i`
        // is in bounds for `i < 8`.
        *b = unsafe { base.add(off + i).read_volatile() };
    }
    u64::from_le_bytes(bytes)
}

/// Topic detections are published on, labelled by model id.
pub const VISION_DETECTION_TOPIC: &str = "vision.detection";

/// Normalized pixel format of a frame in the ring. The engine downscales and
/// converts the camera's native format to one of these before publishing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FrameFormat {
    /// Packed 24-bit RGB, 3 bytes per pixel.
    Rgb24,
    /// Semi-planar YUV 4:2:0 (Y plane then interleaved UV), 1.5 bytes per pixel.
    Nv12,
    /// Planar YUV 4:2:0 (Y, then U, then V), 1.5 bytes per pixel.
    Yuv420p,
}

impl FrameFormat {
    /// Exact byte length of one `width` x `height` frame in this format.
    ///
    /// The 4:2:0 formats require even dimensions; callers normalize to even
    /// width/height before sizing a ring.
    pub fn frame_bytes(self, width: u32, height: u32) -> usize {
        let px = width as usize * height as usize;
        match self {
            FrameFormat::Rgb24 => px * 3,
            // Y plane (px) + chroma (px / 2) = px * 3 / 2.
            FrameFormat::Nv12 | FrameFormat::Yuv420p => px + px / 2,
        }
    }
}

/// The current wire version of a [`FrameDescriptor`] on the `vision.frame`
/// topic. Bumped whenever the descriptor's on-wire shape changes; a decode of a
/// descriptor stamped with any other version fails loudly rather than silently
/// mis-parsing. Mirrors the `framebus.descriptor` entry in the contract
/// registry (`contracts.toml`).
pub const FRAMEBUS_DESCRIPTOR_VERSION: u16 = 1;

/// The small message published on `vision.frame`. It names the ring slot a
/// consumer should read and carries the `seq` that the per-slot seqlock must
/// still hold for the read to be valid.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrameDescriptor {
    /// Wire version, stamped on every descriptor and checked on decode. A
    /// consumer that sees a version it does not speak rejects the frame instead
    /// of mis-parsing it. Deliberately carries no serde default: a payload
    /// missing this field fails to decode.
    #[serde(rename = "v")]
    pub v: u16,
    /// Source camera id (a UVC/CSI device the engine owns, or the FPV camera
    /// tapped from the video pipeline). Lets a consumer filter by camera.
    pub camera_id: String,
    /// Monotonic frame counter for this camera, starting at 1.
    pub frame_id: u64,
    /// Capture time in milliseconds (the same clock the engine timestamps with;
    /// VIO consumers align this to flight-controller time downstream).
    pub ts_ms: i64,
    pub width: u32,
    pub height: u32,
    pub format: FrameFormat,
    /// `/dev/shm` name of the ring this frame lives in (one ring per camera).
    pub shm_name: String,
    /// Slot index within the ring holding this frame's pixels.
    pub slot: u32,
    /// Ring sequence stamped on the slot. A consumer re-checks this against the
    /// slot's seqlock after copying; a mismatch means the writer recycled the
    /// slot mid-read and the frame must be dropped.
    pub seq: u64,
    /// Length of the valid pixel bytes in the slot (`format.frame_bytes`).
    pub byte_len: u32,
}

impl FrameDescriptor {
    /// Encode as a msgpack map with named keys (matches the Python reader).
    pub fn to_msgpack(&self) -> Result<Vec<u8>, rmp_serde::encode::Error> {
        rmp_serde::to_vec_named(self)
    }

    /// Decode from a msgpack map, rejecting a descriptor whose wire version this
    /// build does not speak. A missing `v` field fails the msgpack decode; a
    /// present-but-unknown `v` returns [`DescriptorError::Version`].
    pub fn from_msgpack(bytes: &[u8]) -> Result<Self, DescriptorError> {
        let desc: FrameDescriptor = rmp_serde::from_slice(bytes)?;
        if desc.v != FRAMEBUS_DESCRIPTOR_VERSION {
            return Err(DescriptorError::Version {
                got: desc.v,
                ours: FRAMEBUS_DESCRIPTOR_VERSION,
            });
        }
        Ok(desc)
    }
}

/// Errors decoding a [`FrameDescriptor`] from its msgpack wire form.
#[derive(Debug, Error)]
pub enum DescriptorError {
    #[error("msgpack decode error: {0}")]
    Decode(#[from] rmp_serde::decode::Error),
    #[error("unsupported vision.frame descriptor version {got} (this build speaks {ours})")]
    Version { got: u16, ours: u16 },
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum RingError {
    #[error("slot {slot} out of range (ring has {slot_count} slots)")]
    SlotOutOfRange { slot: u32, slot_count: u32 },
    #[error("payload of {len} bytes exceeds slot capacity {cap}")]
    PayloadTooLarge { len: usize, cap: usize },
    #[error("shared region is {got} bytes, layout needs {need}")]
    RegionTooSmall { got: usize, need: usize },
    #[error("slot_count {slot_count} exceeds the header maximum {max}")]
    SlotCountTooLarge { slot_count: u32, max: u32 },
}

/// Memory layout of a single-writer, many-reader frame ring.
///
/// Layout (all integers little-endian):
///
/// ```text
/// [ ring header  ]  HEADER_LEN bytes: magic, version, slot_count, slot_bytes
/// [ slot 0       ]  SLOT_HEADER_LEN + slot_bytes + SLOT_TRAILER_LEN
/// [ slot 1       ]
///   ...
/// ```
///
/// Each slot is `seq_begin:u64 | byte_len:u32 | _pad:u32 | <slot_bytes data> | seq_end:u64`.
/// The writer stores `seq_begin`, then the data and length, then `seq_end`; a
/// reader loads `seq_end`, copies the data, then re-checks `seq_begin` and
/// `seq_end` against the descriptor's `seq`. If either differs the read was torn
/// by a slot recycle and is discarded. Sizing `slot_count` above the number of
/// concurrent consumers plus their read latency makes torn reads rare.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RingLayout {
    pub slot_count: u32,
    /// Capacity of one slot's pixel region (the largest frame the ring holds).
    pub slot_bytes: u32,
}

impl RingLayout {
    /// "ADV1" — ADOS vision ring, version 1.
    pub const MAGIC: u32 = 0x4144_5631;
    pub const VERSION: u16 = 1;
    pub const HEADER_LEN: usize = 16;
    /// seq_begin (u64) + byte_len (u32) + pad (u32).
    pub const SLOT_HEADER_LEN: usize = 16;
    /// seq_end (u64).
    pub const SLOT_TRAILER_LEN: usize = 8;

    /// Size a ring that holds frames up to `width` x `height` in `format`.
    pub fn for_frame(slot_count: u32, width: u32, height: u32, format: FrameFormat) -> Self {
        RingLayout {
            slot_count,
            slot_bytes: format.frame_bytes(width, height) as u32,
        }
    }

    pub fn slot_stride(&self) -> usize {
        Self::SLOT_HEADER_LEN + self.slot_bytes as usize + Self::SLOT_TRAILER_LEN
    }

    /// Total bytes the shared region must provide.
    pub fn total_len(&self) -> usize {
        Self::HEADER_LEN + self.slot_count as usize * self.slot_stride()
    }

    fn slot_offset(&self, slot: u32) -> usize {
        Self::HEADER_LEN + slot as usize * self.slot_stride()
    }

    /// Validate the layout against the on-disk header's field widths. The
    /// header stores `slot_count` in two bytes, so a layout above
    /// [`MAX_SLOT_COUNT`] would truncate on write and the reader's
    /// header-derived layout would name a different slot than the writer used.
    /// Callers validate at ring-creation / engine startup so a misconfigured
    /// depth is rejected loudly rather than silently truncated.
    pub fn validate(&self) -> Result<(), RingError> {
        if self.slot_count > MAX_SLOT_COUNT {
            return Err(RingError::SlotCountTooLarge {
                slot_count: self.slot_count,
                max: MAX_SLOT_COUNT,
            });
        }
        Ok(())
    }

    /// Write the ring header at the front of a freshly created region. Rejects a
    /// `slot_count` the two-byte header field cannot represent so the writer's
    /// slot math (`seq % slot_count`) can never diverge from a reader that
    /// re-derives the layout from the header.
    pub fn write_header(&self, region: &mut [u8]) -> Result<(), RingError> {
        self.validate()?;
        self.check_region(region.len())?;
        region[0..4].copy_from_slice(&Self::MAGIC.to_le_bytes());
        region[4..6].copy_from_slice(&Self::VERSION.to_le_bytes());
        region[6..8].copy_from_slice(&(self.slot_count as u16).to_le_bytes());
        region[8..12].copy_from_slice(&self.slot_bytes.to_le_bytes());
        region[12..16].copy_from_slice(&0u32.to_le_bytes());
        Ok(())
    }

    /// Read the layout a writer recorded in a region's header.
    pub fn read_header(region: &[u8]) -> Option<RingLayout> {
        if region.len() < Self::HEADER_LEN {
            return None;
        }
        let magic = u32::from_le_bytes(region[0..4].try_into().unwrap());
        let version = u16::from_le_bytes(region[4..6].try_into().unwrap());
        if magic != Self::MAGIC || version != Self::VERSION {
            return None;
        }
        let slot_count = u16::from_le_bytes(region[6..8].try_into().unwrap()) as u32;
        let slot_bytes = u32::from_le_bytes(region[8..12].try_into().unwrap());
        Some(RingLayout {
            slot_count,
            slot_bytes,
        })
    }

    fn check_region(&self, got: usize) -> Result<(), RingError> {
        let need = self.total_len();
        if got < need {
            return Err(RingError::RegionTooSmall { got, need });
        }
        Ok(())
    }
}

/// Write one frame into `slot` of the ring, stamping it with `seq`.
///
/// The single writer is expected to choose `slot = seq % slot_count` and call
/// this once per captured frame, then publish the matching [`FrameDescriptor`].
pub fn write_slot(
    region: &mut [u8],
    layout: &RingLayout,
    slot: u32,
    seq: u64,
    data: &[u8],
) -> Result<(), RingError> {
    if slot >= layout.slot_count {
        return Err(RingError::SlotOutOfRange {
            slot,
            slot_count: layout.slot_count,
        });
    }
    let cap = layout.slot_bytes as usize;
    if data.len() > cap {
        return Err(RingError::PayloadTooLarge {
            len: data.len(),
            cap,
        });
    }
    layout.check_region(region.len())?;

    let base = layout.slot_offset(slot);
    let data_off = base + RingLayout::SLOT_HEADER_LEN;
    let trailer_off = data_off + cap;

    // Seqlock write order (single writer, many cross-process readers):
    //   1. stamp seq_begin
    //   2. Release fence so the begin stamp is visible before the data copy
    //   3. copy the pixel bytes + length
    //   4. Release fence so the data is visible before seq_end commits
    //   5. stamp seq_end
    // The two fences keep a weakly-ordered CPU (the aarch64 SBC target) from
    // reordering the data copy past either guard, so a reader that observes
    // matching begin/end guards is guaranteed to have observed this frame's
    // bytes, not a torn mix of two frames.
    store_seq_volatile(region, base, seq);
    region[base + 8..base + 12].copy_from_slice(&(data.len() as u32).to_le_bytes());
    region[base + 12..base + 16].copy_from_slice(&0u32.to_le_bytes());
    fence(Ordering::Release);
    region[data_off..data_off + data.len()].copy_from_slice(data);
    fence(Ordering::Release);
    store_seq_volatile(region, trailer_off, seq);
    Ok(())
}

/// Read the frame a [`FrameDescriptor`] points at, validating the seqlock.
///
/// Returns `Ok(None)` if the slot no longer holds `expected_seq` (the writer
/// recycled it) — the caller drops that frame and waits for the next descriptor.
pub fn read_slot(
    region: &[u8],
    layout: &RingLayout,
    slot: u32,
    expected_seq: u64,
) -> Result<Option<Vec<u8>>, RingError> {
    if slot >= layout.slot_count {
        return Err(RingError::SlotOutOfRange {
            slot,
            slot_count: layout.slot_count,
        });
    }
    layout.check_region(region.len())?;

    let cap = layout.slot_bytes as usize;
    let base = layout.slot_offset(slot);
    let data_off = base + RingLayout::SLOT_HEADER_LEN;
    let trailer_off = data_off + cap;

    // Seqlock read order (mirrors the write fences in reverse):
    //   1. load seq_end (the committed marker the writer stamps last)
    //   2. Acquire fence so the data copy cannot be hoisted above the load
    //   3. copy the pixel bytes
    //   4. Acquire fence so the guard re-reads cannot be hoisted above the copy
    //   5. re-read seq_begin + seq_end; a mismatch means the writer recycled
    //      the slot mid-copy (the read was torn) and the frame is dropped.
    let seq_end = load_seq_volatile(region, trailer_off);
    if seq_end != expected_seq {
        return Ok(None);
    }
    let byte_len = u32::from_le_bytes(region[base + 8..base + 12].try_into().unwrap()) as usize;
    if byte_len > cap {
        return Ok(None);
    }
    fence(Ordering::Acquire);
    // The copy must not be reordered before the seq_end check above nor after
    // the guard re-reads below; the volatile guard loads plus the fences pin it.
    compiler_fence(Ordering::Acquire);
    let data = region[data_off..data_off + byte_len].to_vec();
    fence(Ordering::Acquire);
    let seq_begin = load_seq_volatile(region, base);
    let seq_end2 = load_seq_volatile(region, trailer_off);
    if seq_begin != expected_seq || seq_end2 != expected_seq {
        return Ok(None);
    }
    Ok(Some(data))
}

// ---------------------------------------------------------------------------
// Detection + model contracts.
//
// Unlike frames, these payloads are small, so they ride the plugin RPC
// envelope and the `vision.detection` event directly (no shared memory). They
// live here so the engine, the plugin host bridge, and the SDK share one shape.
// ---------------------------------------------------------------------------

/// Plugin RPC method names for the vision surface. The plugin host gates each
/// on the matching capability before routing to the vision engine over
/// `/run/ados/vision.sock`.
pub mod methods {
    /// Subscribe to `vision.frame` descriptors. Gated on `vision.frame.read`.
    /// The host then delivers descriptors as `vision.deliver` events.
    pub const SUBSCRIBE_FRAMES: &str = "vision.subscribe_frames";
    /// Register an inference model. Gated on `vision.model.register`.
    pub const REGISTER_MODEL: &str = "vision.register_model";
    /// Run a registered model against one frame on the shared backend, returning
    /// detections. Gated on `vision.model.register`.
    pub const INFER: &str = "vision.infer";
    /// Publish a detection batch on `vision.detection`. Gated on
    /// `vision.detection.publish`.
    pub const PUBLISH_DETECTION: &str = "vision.publish_detection";

    /// Subscribe to published `DetectionBatch`es. Gated on
    /// `vision.detection.subscribe`. The host then delivers each batch as a
    /// `vision.deliver_detection` event (mirrors `subscribe_frames`).
    pub const SUBSCRIBE_DETECTIONS: &str = "vision.subscribe_detections";

    /// Event method the host uses to push a detection batch to a subscriber.
    pub const DELIVER_DETECTION: &str = "vision.deliver_detection";

    /// Designate the engine's single-object follow target: lock the camera's
    /// tracker onto a specific box (the operator's click-to-follow pick). Served
    /// by the engine to trusted on-box callers; not yet exposed to the plugin
    /// capability dispatch (a plugin-facing gate lands with the follow-me
    /// plugin). Args: `{camera_id, bbox, class_label?, confidence?}`.
    pub const DESIGNATE_TRACK: &str = "vision.designate_track";

    /// Event method the host uses to push a frame descriptor to a subscriber
    /// (mirrors `mavlink.deliver`).
    pub const DELIVER_FRAME: &str = "vision.deliver";
}

/// What a model produces, so consumers know how to read its output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ModelKind {
    Detection,
    Segmentation,
    Classification,
    Tracking,
}

/// How a registered model is executed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelExecution {
    /// The engine loads the model file and runs it on the shared backend, then
    /// publishes detections itself.
    EngineRun,
    /// The plugin runs the model and publishes detections (or calls `infer`).
    PluginSide,
}

/// The output-tensor layout of a detection model's head, so the decoder reads
/// the right axes. `Yolo8` is the transposed `[1, 4+nc, anchors]` head — box
/// (cx, cy, w, h) plus one score per class, no objectness — that ultralytics
/// YOLOv8/v11 export. `Yolo5` is the legacy `[1, anchors, 5+nc]` head — box plus
/// an objectness column plus per-class scores — of YOLOv5/v7. They are decoded
/// differently (a v8 model read as v5 shifts every field by one and mistakes the
/// first class score for objectness), so the layout travels with the model.
/// Defaults to `Yolo8`, the current export path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum DetectionHead {
    #[default]
    Yolo8,
    Yolo5,
}

/// Metadata a plugin supplies when registering a model.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelMetadata {
    /// Reverse-DNS-ish unique model id (e.g. `com.example.weeds`).
    pub id: String,
    pub kind: ModelKind,
    pub execution: ModelExecution,
    /// Input the model expects; the engine downscales/converts frames to match.
    pub input_width: u32,
    pub input_height: u32,
    pub input_format: FrameFormat,
    /// Class labels in output-index order (empty for non-detection kinds).
    #[serde(default)]
    pub output_classes: Vec<String>,
    /// Path to the model file on the agent, for engine-run models.
    #[serde(default)]
    pub model_path: Option<String>,
    /// Output-head layout for decoding (detection models). Defaults to `Yolo8`;
    /// a model that ships the legacy YOLOv5/v7 head pins `Yolo5`. A payload that
    /// predates this field deserializes to the `Yolo8` default.
    #[serde(default)]
    pub head: DetectionHead,
}

impl ModelMetadata {
    pub fn to_msgpack(&self) -> Result<Vec<u8>, rmp_serde::encode::Error> {
        rmp_serde::to_vec_named(self)
    }
    pub fn from_msgpack(bytes: &[u8]) -> Result<Self, rmp_serde::decode::Error> {
        rmp_serde::from_slice(bytes)
    }
}

/// A pixel-space bounding box (origin top-left), in the frame's own resolution.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct BoundingBox {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

/// Confidence that this frame's detection belongs to the same object as the
/// track it was associated with. `Locked` = the tracker is confident the
/// identity held; `Uncertain` = the association is weak (occlusion, a nearby
/// similar object, a low-confidence re-match) so a downstream consumer should
/// treat the identity as provisional; `Lost` = the track could not be
/// re-associated this frame. Carrying this on the wire makes a silent identity
/// swap impossible to hide: the uncertainty travels with the detection instead
/// of being collapsed away by the tracker.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LockState {
    Locked,
    Uncertain,
    Lost,
}

/// One detection from a model.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Detection {
    pub bbox: BoundingBox,
    pub class_label: String,
    pub confidence: f32,
    /// Stable track id across frames (tracking models only).
    #[serde(default)]
    pub track_id: Option<u64>,
    /// How confident the tracker is that this detection is the same object as
    /// its `track_id` (0..1). `None` when the source does not score
    /// association (e.g. a stateless detector). Distinct from `confidence`,
    /// which scores the class/object detection itself.
    #[serde(default)]
    pub assoc_confidence: Option<f32>,
    /// Discrete lock state of the track's identity this frame. `None` when the
    /// source does not report a lock state.
    #[serde(default)]
    pub lock_state: Option<LockState>,
}

/// The current wire version of a [`DetectionBatch`] on the `vision.detection`
/// topic. Bumped whenever the batch's on-wire shape changes; a decode of a batch
/// stamped with any other version fails loudly rather than silently mis-parsing.
/// Mirrors the `vision.detection` entry in the contract registry
/// (`contracts.toml`).
pub const VISION_DETECTION_VERSION: u16 = 1;

/// The payload on `vision.detection`, labelled by source model and frame so
/// overlays and consumers can align boxes to the frame they came from.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DetectionBatch {
    /// Wire version, stamped on every batch and checked on decode. A consumer
    /// that sees a version it does not speak rejects the batch instead of
    /// mis-parsing it. Deliberately carries no serde default: a payload missing
    /// this field fails to decode.
    #[serde(rename = "v")]
    pub v: u16,
    pub model_id: String,
    pub camera_id: String,
    pub frame_id: u64,
    pub ts_ms: i64,
    pub detections: Vec<Detection>,
}

impl DetectionBatch {
    pub fn to_msgpack(&self) -> Result<Vec<u8>, rmp_serde::encode::Error> {
        rmp_serde::to_vec_named(self)
    }

    /// Decode from a msgpack map, rejecting a batch whose wire version this build
    /// does not speak. A missing `v` field fails the msgpack decode; a
    /// present-but-unknown `v` returns [`DetectionBatchError::Version`].
    pub fn from_msgpack(bytes: &[u8]) -> Result<Self, DetectionBatchError> {
        let batch: DetectionBatch = rmp_serde::from_slice(bytes)?;
        if batch.v != VISION_DETECTION_VERSION {
            return Err(DetectionBatchError::Version {
                got: batch.v,
                ours: VISION_DETECTION_VERSION,
            });
        }
        Ok(batch)
    }
}

/// Errors decoding a [`DetectionBatch`] from its msgpack wire form.
#[derive(Debug, Error)]
pub enum DetectionBatchError {
    #[error("msgpack decode error: {0}")]
    Decode(#[from] rmp_serde::decode::Error),
    #[error("unsupported vision.detection version {got} (this build speaks {ours})")]
    Version { got: u16, ours: u16 },
}

#[cfg(test)]
mod contract_tests {
    use super::*;

    #[test]
    fn model_metadata_round_trips() {
        let m = ModelMetadata {
            id: "com.example.weeds".into(),
            kind: ModelKind::Detection,
            execution: ModelExecution::EngineRun,
            input_width: 640,
            input_height: 480,
            input_format: FrameFormat::Rgb24,
            output_classes: vec!["weed".into(), "crop".into()],
            model_path: Some("/opt/ados/models/vision/weeds.onnx".into()),
            head: DetectionHead::Yolo8,
        };
        let bytes = m.to_msgpack().unwrap();
        assert_eq!(ModelMetadata::from_msgpack(&bytes).unwrap(), m);
    }

    #[test]
    fn model_metadata_without_head_defaults_to_yolo8() {
        // A payload serialized before the `head` field existed (no `head` key)
        // must still decode, defaulting to the YOLOv8 head.
        let legacy = ModelMetadataLegacy {
            id: "com.example.old".into(),
            kind: ModelKind::Detection,
            execution: ModelExecution::EngineRun,
            input_width: 640,
            input_height: 640,
            input_format: FrameFormat::Rgb24,
            output_classes: vec!["uav".into()],
            model_path: None,
        };
        let bytes = rmp_serde::to_vec_named(&legacy).unwrap();
        let decoded = ModelMetadata::from_msgpack(&bytes).unwrap();
        assert_eq!(decoded.head, DetectionHead::Yolo8);
        assert_eq!(decoded.id, "com.example.old");
    }

    // The pre-`head` shape, to prove forward-compatibility of the new field.
    #[derive(Serialize)]
    struct ModelMetadataLegacy {
        id: String,
        kind: ModelKind,
        execution: ModelExecution,
        input_width: u32,
        input_height: u32,
        input_format: FrameFormat,
        output_classes: Vec<String>,
        model_path: Option<String>,
    }

    #[test]
    fn detection_head_serializes_lowercase() {
        assert_eq!(
            serde_json::to_string(&DetectionHead::Yolo8).unwrap(),
            "\"yolo8\""
        );
        assert_eq!(
            serde_json::to_string(&DetectionHead::Yolo5).unwrap(),
            "\"yolo5\""
        );
    }

    #[test]
    fn detection_batch_round_trips() {
        let b = DetectionBatch {
            v: VISION_DETECTION_VERSION,
            model_id: "com.example.weeds".into(),
            camera_id: "uvc-0".into(),
            frame_id: 7,
            ts_ms: 1_700_000_000_000,
            detections: vec![Detection {
                bbox: BoundingBox {
                    x: 12.0,
                    y: 20.0,
                    width: 64.0,
                    height: 32.0,
                },
                class_label: "weed".into(),
                confidence: 0.87,
                track_id: None,
                assoc_confidence: None,
                lock_state: None,
            }],
        };
        let bytes = b.to_msgpack().unwrap();
        assert_eq!(DetectionBatch::from_msgpack(&bytes).unwrap(), b);
    }

    #[test]
    fn detection_version_matches_registry() {
        assert_eq!(
            VISION_DETECTION_VERSION,
            crate::contracts::contract_version("vision.detection").unwrap()
        );
    }

    #[test]
    fn detection_rejects_a_future_version() {
        // A batch stamped with a version this build does not speak must fail the
        // decode loudly rather than silently mis-parse.
        let batch = DetectionBatch {
            v: VISION_DETECTION_VERSION + 1,
            model_id: "m".into(),
            camera_id: "c".into(),
            frame_id: 1,
            ts_ms: 0,
            detections: vec![],
        };
        let bytes = batch.to_msgpack().unwrap();
        assert!(matches!(
            DetectionBatch::from_msgpack(&bytes),
            Err(DetectionBatchError::Version { .. })
        ));
    }

    #[test]
    fn detection_missing_version_fails_decode() {
        // A payload that predates the version field (no `v` key) must fail to
        // decode: the field is required, with no serde default.
        #[derive(Serialize)]
        struct NoVersionBatch {
            model_id: String,
            camera_id: String,
            frame_id: u64,
            ts_ms: i64,
            detections: Vec<Detection>,
        }
        let old = NoVersionBatch {
            model_id: "m".into(),
            camera_id: "c".into(),
            frame_id: 1,
            ts_ms: 0,
            detections: vec![],
        };
        let bytes = rmp_serde::to_vec_named(&old).unwrap();
        assert!(matches!(
            DetectionBatch::from_msgpack(&bytes),
            Err(DetectionBatchError::Decode(_))
        ));
    }

    #[test]
    fn detection_lock_fields_round_trip_present() {
        let d = Detection {
            bbox: BoundingBox {
                x: 1.0,
                y: 2.0,
                width: 10.0,
                height: 20.0,
            },
            class_label: "target".into(),
            confidence: 0.91,
            track_id: Some(42),
            assoc_confidence: Some(0.73),
            lock_state: Some(LockState::Uncertain),
        };
        let bytes = rmp_serde::to_vec_named(&d).unwrap();
        let back: Detection = rmp_serde::from_slice(&bytes).unwrap();
        assert_eq!(back, d);
    }

    #[test]
    fn detection_lock_state_serializes_lowercase() {
        let s = rmp_serde::to_vec_named(&LockState::Locked).unwrap();
        assert_eq!(rmp_serde::from_slice::<String>(&s).unwrap(), "locked");
        let s = rmp_serde::to_vec_named(&LockState::Lost).unwrap();
        assert_eq!(rmp_serde::from_slice::<String>(&s).unwrap(), "lost");
    }

    #[test]
    fn detection_new_fields_default_when_absent() {
        // A msgpack named-map written by an old producer that predates the
        // lock fields must still decode (the new fields default to None).
        #[derive(Serialize)]
        struct OldDetection {
            bbox: BoundingBox,
            class_label: String,
            confidence: f32,
            track_id: Option<u64>,
        }
        let old = OldDetection {
            bbox: BoundingBox {
                x: 0.0,
                y: 0.0,
                width: 5.0,
                height: 5.0,
            },
            class_label: "weed".into(),
            confidence: 0.5,
            track_id: Some(7),
        };
        let bytes = rmp_serde::to_vec_named(&old).unwrap();
        let back: Detection = rmp_serde::from_slice(&bytes).unwrap();
        assert_eq!(back.track_id, Some(7));
        assert_eq!(back.assoc_confidence, None);
        assert_eq!(back.lock_state, None);
    }

    #[test]
    fn detection_new_fields_skipped_for_old_readers() {
        // A new producer's bytes must still decode for a reader that only
        // knows the old fields (forward compatibility).
        #[derive(Deserialize)]
        #[allow(dead_code)]
        struct OldReader {
            bbox: BoundingBox,
            class_label: String,
            confidence: f32,
            #[serde(default)]
            track_id: Option<u64>,
        }
        let d = Detection {
            bbox: BoundingBox {
                x: 1.0,
                y: 1.0,
                width: 2.0,
                height: 2.0,
            },
            class_label: "target".into(),
            confidence: 0.8,
            track_id: Some(3),
            assoc_confidence: Some(0.4),
            lock_state: Some(LockState::Lost),
        };
        let bytes = rmp_serde::to_vec_named(&d).unwrap();
        let back: OldReader = rmp_serde::from_slice(&bytes).unwrap();
        assert_eq!(back.track_id, Some(3));
        assert_eq!(back.class_label, "target");
    }

    #[test]
    fn enums_serialize_to_expected_strings() {
        let k = rmp_serde::to_vec_named(&ModelKind::Tracking).unwrap();
        assert_eq!(rmp_serde::from_slice::<String>(&k).unwrap(), "tracking");
        let e = rmp_serde::to_vec_named(&ModelExecution::PluginSide).unwrap();
        assert_eq!(rmp_serde::from_slice::<String>(&e).unwrap(), "plugin_side");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_bytes_matches_format() {
        assert_eq!(FrameFormat::Rgb24.frame_bytes(640, 480), 640 * 480 * 3);
        assert_eq!(FrameFormat::Nv12.frame_bytes(640, 480), 640 * 480 * 3 / 2);
        assert_eq!(
            FrameFormat::Yuv420p.frame_bytes(1280, 720),
            1280 * 720 * 3 / 2
        );
    }

    #[test]
    fn descriptor_round_trips_through_msgpack() {
        let d = FrameDescriptor {
            v: FRAMEBUS_DESCRIPTOR_VERSION,
            camera_id: "uvc-0".into(),
            frame_id: 42,
            ts_ms: 1_700_000_000_000,
            width: 640,
            height: 480,
            format: FrameFormat::Rgb24,
            shm_name: "ados-vision-uvc-0".into(),
            slot: 3,
            seq: 1234,
            byte_len: (640 * 480 * 3) as u32,
        };
        let bytes = d.to_msgpack().unwrap();
        assert_eq!(FrameDescriptor::from_msgpack(&bytes).unwrap(), d);
    }

    #[test]
    fn descriptor_version_matches_registry() {
        assert_eq!(
            FRAMEBUS_DESCRIPTOR_VERSION,
            crate::contracts::contract_version("framebus.descriptor").unwrap()
        );
    }

    #[test]
    fn ring_version_matches_registry() {
        assert_eq!(
            RingLayout::VERSION,
            crate::contracts::contract_version("framebus.ring").unwrap()
        );
    }

    #[test]
    fn descriptor_rejects_a_future_version() {
        // A descriptor stamped with a version this build does not speak must
        // fail the decode loudly rather than silently mis-parse.
        let d = FrameDescriptor {
            v: FRAMEBUS_DESCRIPTOR_VERSION + 1,
            camera_id: "uvc-0".into(),
            frame_id: 1,
            ts_ms: 0,
            width: 8,
            height: 8,
            format: FrameFormat::Rgb24,
            shm_name: "ados-vision-uvc-0".into(),
            slot: 0,
            seq: 1,
            byte_len: 192,
        };
        let bytes = d.to_msgpack().unwrap();
        assert!(matches!(
            FrameDescriptor::from_msgpack(&bytes),
            Err(DescriptorError::Version { .. })
        ));
    }

    #[test]
    fn descriptor_missing_version_fails_decode() {
        // A payload that predates the version field (no `v` key) must fail to
        // decode: the field is required, with no serde default.
        #[derive(Serialize)]
        struct NoVersionDescriptor {
            camera_id: String,
            frame_id: u64,
            ts_ms: i64,
            width: u32,
            height: u32,
            format: FrameFormat,
            shm_name: String,
            slot: u32,
            seq: u64,
            byte_len: u32,
        }
        let old = NoVersionDescriptor {
            camera_id: "uvc-0".into(),
            frame_id: 1,
            ts_ms: 0,
            width: 8,
            height: 8,
            format: FrameFormat::Rgb24,
            shm_name: "ados-vision-uvc-0".into(),
            slot: 0,
            seq: 1,
            byte_len: 192,
        };
        let bytes = rmp_serde::to_vec_named(&old).unwrap();
        assert!(matches!(
            FrameDescriptor::from_msgpack(&bytes),
            Err(DescriptorError::Decode(_))
        ));
    }

    #[test]
    fn format_serializes_lowercase() {
        // The Python reader matches on the lowercase string.
        let bytes = rmp_serde::to_vec_named(&FrameFormat::Yuv420p).unwrap();
        let back: String = rmp_serde::from_slice(&bytes).unwrap();
        assert_eq!(back, "yuv420p");
    }

    #[test]
    fn header_round_trips() {
        let layout = RingLayout::for_frame(4, 64, 48, FrameFormat::Rgb24);
        let mut region = vec![0u8; layout.total_len()];
        layout.write_header(&mut region).unwrap();
        let read = RingLayout::read_header(&region).unwrap();
        assert_eq!(read, layout);
    }

    #[test]
    fn write_then_read_returns_the_frame() {
        let layout = RingLayout::for_frame(4, 8, 8, FrameFormat::Rgb24); // 192-byte slots
        let mut region = vec![0u8; layout.total_len()];
        layout.write_header(&mut region).unwrap();

        let frame: Vec<u8> = (0..layout.slot_bytes as u8).collect();
        let seq = 9u64;
        let slot = (seq % layout.slot_count as u64) as u32;
        write_slot(&mut region, &layout, slot, seq, &frame).unwrap();

        let got = read_slot(&region, &layout, slot, seq).unwrap();
        assert_eq!(got.as_deref(), Some(frame.as_slice()));
    }

    #[test]
    fn stale_seq_reads_none() {
        let layout = RingLayout::for_frame(2, 4, 4, FrameFormat::Rgb24);
        let mut region = vec![0u8; layout.total_len()];
        layout.write_header(&mut region).unwrap();
        write_slot(&mut region, &layout, 0, 10, &[1, 2, 3]).unwrap();
        // The consumer asks for a sequence the slot never held.
        assert_eq!(read_slot(&region, &layout, 0, 11).unwrap(), None);
    }

    #[test]
    fn recycled_slot_is_detected_as_torn() {
        let layout = RingLayout::for_frame(1, 4, 4, FrameFormat::Rgb24);
        let mut region = vec![0u8; layout.total_len()];
        layout.write_header(&mut region).unwrap();
        write_slot(&mut region, &layout, 0, 100, &[7, 7, 7, 7]).unwrap();
        // Writer recycles the single slot for the next frame.
        write_slot(&mut region, &layout, 0, 101, &[8, 8, 8, 8]).unwrap();
        // A consumer still holding the old descriptor sees the new seq and drops.
        assert_eq!(read_slot(&region, &layout, 0, 100).unwrap(), None);
        assert_eq!(
            read_slot(&region, &layout, 0, 101).unwrap(),
            Some(vec![8; 4])
        );
    }

    #[test]
    fn oversized_payload_rejected() {
        let layout = RingLayout::for_frame(2, 2, 2, FrameFormat::Rgb24); // 12-byte slots
        let mut region = vec![0u8; layout.total_len()];
        layout.write_header(&mut region).unwrap();
        let too_big = vec![0u8; layout.slot_bytes as usize + 1];
        assert!(matches!(
            write_slot(&mut region, &layout, 0, 1, &too_big),
            Err(RingError::PayloadTooLarge { .. })
        ));
    }

    #[test]
    fn slot_out_of_range_rejected() {
        let layout = RingLayout::for_frame(2, 2, 2, FrameFormat::Rgb24);
        let mut region = vec![0u8; layout.total_len()];
        layout.write_header(&mut region).unwrap();
        assert!(matches!(
            write_slot(&mut region, &layout, 2, 1, &[0]),
            Err(RingError::SlotOutOfRange { .. })
        ));
    }

    #[test]
    fn small_region_rejected() {
        let layout = RingLayout::for_frame(4, 64, 64, FrameFormat::Rgb24);
        let mut tiny = vec![0u8; 8];
        assert!(matches!(
            layout.write_header(&mut tiny),
            Err(RingError::RegionTooSmall { .. })
        ));
    }

    #[test]
    fn slot_count_at_the_header_maximum_round_trips() {
        // u16::MAX is the largest count the two-byte header field carries; it
        // must validate and round-trip exactly (no truncation).
        let layout = RingLayout {
            slot_count: MAX_SLOT_COUNT,
            slot_bytes: 4,
        };
        assert!(layout.validate().is_ok());
        // The header is only HEADER_LEN bytes; write it into a region sized to
        // just the header so the round-trip exercises the field, not the slots.
        let mut header_region = vec![0u8; layout.total_len()];
        layout.write_header(&mut header_region).unwrap();
        let read = RingLayout::read_header(&header_region).unwrap();
        assert_eq!(read.slot_count, MAX_SLOT_COUNT);
    }

    #[test]
    fn slot_count_above_the_header_maximum_is_rejected() {
        // A slot_count that the u16 header field would truncate is refused at
        // header-write time rather than silently wrapping to a smaller ring.
        let layout = RingLayout {
            slot_count: MAX_SLOT_COUNT + 1,
            slot_bytes: 4,
        };
        assert!(matches!(
            layout.validate(),
            Err(RingError::SlotCountTooLarge { .. })
        ));
        let mut region = vec![0u8; 64];
        assert!(matches!(
            layout.write_header(&mut region),
            Err(RingError::SlotCountTooLarge { .. })
        ));
    }

    #[test]
    fn concurrent_writer_and_reader_never_yield_a_torn_frame() {
        // A genuine cross-thread seqlock stress: one writer recycles a small
        // ring as fast as it can while a reader copies the slot the latest
        // descriptor named. Every successful read must be an internally
        // consistent frame (all bytes equal the frame's marker), never a torn
        // mix of two frames. The volatile guards + Acquire/Release fences are
        // what make this hold on a weakly-ordered CPU.
        use std::sync::atomic::{AtomicBool, AtomicU64, Ordering as O};
        use std::sync::Arc;

        const SLOTS: u32 = 3;
        const FRAME: usize = 256;
        let layout = RingLayout {
            slot_count: SLOTS,
            slot_bytes: FRAME as u32,
        };
        let total = layout.total_len();

        // The shared region behind an UnsafeCell-equivalent: a raw buffer the
        // writer mutates and the reader reads, exactly the /dev/shm aliasing the
        // seqlock is designed for. A Mutex would serialize the two and defeat
        // the test, so a raw pointer wrapper carries the aliasing explicitly.
        struct Shared(*mut u8, usize);
        unsafe impl Send for Shared {}
        unsafe impl Sync for Shared {}
        let mut backing = vec![0u8; total];
        layout.write_header(&mut backing).unwrap();
        let ptr = backing.as_mut_ptr();
        let shared = Arc::new(Shared(ptr, total));

        // The latest committed seq the reader chases.
        let latest = Arc::new(AtomicU64::new(0));
        let stop = Arc::new(AtomicBool::new(false));

        let w_shared = shared.clone();
        let w_latest = latest.clone();
        let w_stop = stop.clone();
        let writer = std::thread::spawn(move || {
            // SAFETY: the writer is the single mutator of the region for the
            // test's lifetime; the reader only reads. This mirrors the
            // single-writer / many-reader ring contract.
            let region = unsafe { std::slice::from_raw_parts_mut(w_shared.0, w_shared.1) };
            let mut seq = 1u64;
            while !w_stop.load(O::Relaxed) {
                let marker = (seq & 0xFF) as u8;
                let frame = vec![marker; FRAME];
                let slot = (seq % SLOTS as u64) as u32;
                write_slot(region, &layout, slot, seq, &frame).unwrap();
                w_latest.store(seq, O::Release);
                seq += 1;
            }
        });

        let r_shared = shared.clone();
        let r_latest = latest.clone();
        let mut reads = 0u64;
        // Read until a healthy number of committed frames have been observed,
        // bounded so a wedged writer cannot spin forever. A fixed iteration
        // budget is flaky under loaded CI scheduling: a fast reader can drain
        // its whole budget before a starved writer commits its first frame,
        // a test-timing artifact rather than a seqlock failure.
        let mut iters = 0u64;
        while reads < 10_000 && iters < 50_000_000 {
            iters += 1;
            let seq = r_latest.load(O::Acquire);
            if seq == 0 {
                std::hint::spin_loop();
                continue;
            }
            let slot = (seq % SLOTS as u64) as u32;
            // SAFETY: read-only view of the same region the writer mutates; the
            // seqlock discards any read torn by a concurrent recycle.
            let region = unsafe { std::slice::from_raw_parts(r_shared.0, r_shared.1) };
            if let Some(data) = read_slot(region, &layout, slot, seq).unwrap() {
                reads += 1;
                let marker = (seq & 0xFF) as u8;
                assert!(
                    data.iter().all(|&b| b == marker),
                    "torn read at seq {seq}: expected all {marker:#x}"
                );
            }
        }
        stop.store(true, O::Relaxed);
        writer.join().unwrap();
        // The reader saw at least some committed frames (not a vacuous pass).
        assert!(reads > 0, "reader observed no committed frames");
    }
}
