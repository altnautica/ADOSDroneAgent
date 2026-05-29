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

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Topic the vision engine publishes frame descriptors on. Reserved to the
/// host: plugins subscribe (with `vision.frame.read`) but never publish here.
pub const VISION_FRAME_TOPIC: &str = "vision.frame";

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

/// The small message published on `vision.frame`. It names the ring slot a
/// consumer should read and carries the `seq` that the per-slot seqlock must
/// still hold for the read to be valid.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrameDescriptor {
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

    /// Decode from a msgpack map.
    pub fn from_msgpack(bytes: &[u8]) -> Result<Self, rmp_serde::decode::Error> {
        rmp_serde::from_slice(bytes)
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum RingError {
    #[error("slot {slot} out of range (ring has {slot_count} slots)")]
    SlotOutOfRange { slot: u32, slot_count: u32 },
    #[error("payload of {len} bytes exceeds slot capacity {cap}")]
    PayloadTooLarge { len: usize, cap: usize },
    #[error("shared region is {got} bytes, layout needs {need}")]
    RegionTooSmall { got: usize, need: usize },
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

    /// Write the ring header at the front of a freshly created region.
    pub fn write_header(&self, region: &mut [u8]) -> Result<(), RingError> {
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

    // seq_begin first (marks the slot as being written for this seq).
    region[base..base + 8].copy_from_slice(&seq.to_le_bytes());
    region[base + 8..base + 12].copy_from_slice(&(data.len() as u32).to_le_bytes());
    region[base + 12..base + 16].copy_from_slice(&0u32.to_le_bytes());
    region[data_off..data_off + data.len()].copy_from_slice(data);
    // seq_end last (commits the write).
    region[trailer_off..trailer_off + 8].copy_from_slice(&seq.to_le_bytes());
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

    // Load the trailer (committed marker) first.
    let seq_end = u64::from_le_bytes(region[trailer_off..trailer_off + 8].try_into().unwrap());
    if seq_end != expected_seq {
        return Ok(None);
    }
    let byte_len = u32::from_le_bytes(region[base + 8..base + 12].try_into().unwrap()) as usize;
    if byte_len > cap {
        return Ok(None);
    }
    let data = region[data_off..data_off + byte_len].to_vec();
    // Re-check both guards: a writer that recycled this slot mid-copy moves the
    // seq forward, so a stale begin or a changed end means the copy was torn.
    let seq_begin = u64::from_le_bytes(region[base..base + 8].try_into().unwrap());
    let seq_end2 = u64::from_le_bytes(region[trailer_off..trailer_off + 8].try_into().unwrap());
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

/// One detection from a model.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Detection {
    pub bbox: BoundingBox,
    pub class_label: String,
    pub confidence: f32,
    /// Stable track id across frames (tracking models only).
    #[serde(default)]
    pub track_id: Option<u64>,
}

/// The payload on `vision.detection`, labelled by source model and frame so
/// overlays and consumers can align boxes to the frame they came from.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DetectionBatch {
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
    pub fn from_msgpack(bytes: &[u8]) -> Result<Self, rmp_serde::decode::Error> {
        rmp_serde::from_slice(bytes)
    }
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
        };
        let bytes = m.to_msgpack().unwrap();
        assert_eq!(ModelMetadata::from_msgpack(&bytes).unwrap(), m);
    }

    #[test]
    fn detection_batch_round_trips() {
        let b = DetectionBatch {
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
            }],
        };
        let bytes = b.to_msgpack().unwrap();
        assert_eq!(DetectionBatch::from_msgpack(&bytes).unwrap(), b);
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
        assert_eq!(FrameFormat::Yuv420p.frame_bytes(1280, 720), 1280 * 720 * 3 / 2);
    }

    #[test]
    fn descriptor_round_trips_through_msgpack() {
        let d = FrameDescriptor {
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
        assert_eq!(read_slot(&region, &layout, 0, 101).unwrap(), Some(vec![8; 4]));
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
}
