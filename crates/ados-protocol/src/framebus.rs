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
//! [`write_slot`] / [`read_slot_into`].
//!
//! Detections travel the other way as small structured payloads on the
//! `vision.detection` topic and ride the plugin envelope directly; only frames
//! need the ring.

use std::collections::BTreeMap;
use std::sync::atomic::{compiler_fence, fence, Ordering};

use serde::{Deserialize, Serialize};
use thiserror::Error;

mod seqlock;
use seqlock::{load_seq_volatile, store_seq_volatile};

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
    /// 8-bit grayscale, 1 byte per pixel.
    Gray8,
    /// 16-bit grayscale, little-endian, 2 bytes per pixel.
    Gray16,
    /// 8-bit Bayer mosaic, RGGB pattern, 1 byte per pixel.
    BayerRggb8,
    /// 16-bit Bayer mosaic, RGGB pattern, little-endian, 2 bytes per pixel.
    BayerRggb16,
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
            FrameFormat::Gray8 | FrameFormat::BayerRggb8 => px,
            FrameFormat::Gray16 | FrameFormat::BayerRggb16 => px * 2,
        }
    }

    /// The ffmpeg `-pix_fmt` name of this layout.
    pub fn ffmpeg_pix_fmt(self) -> &'static str {
        match self {
            FrameFormat::Rgb24 => "rgb24",
            FrameFormat::Nv12 => "nv12",
            FrameFormat::Yuv420p => "yuv420p",
            FrameFormat::Gray8 => "gray",
            FrameFormat::Gray16 => "gray16le",
            FrameFormat::BayerRggb8 => "bayer_rggb8",
            FrameFormat::BayerRggb16 => "bayer_rggb16le",
        }
    }

    /// A raw Bayer mosaic (not yet demosaiced).
    pub fn is_bayer(self) -> bool {
        matches!(self, FrameFormat::BayerRggb8 | FrameFormat::BayerRggb16)
    }
}

/// The current wire version of a [`FrameDescriptor`] on the `vision.frame`
/// topic. Bumped whenever the descriptor's on-wire shape changes; a decode of a
/// descriptor stamped with any other version fails loudly rather than silently
/// mis-parsing. Defined as the `framebus.descriptor` row of the generated
/// contract registry, so `contracts.toml` is the only place it is set.
pub const FRAMEBUS_DESCRIPTOR_VERSION: u16 =
    crate::contracts::contract_version("framebus.descriptor")
        .expect("framebus.descriptor is registered in contracts.toml");

/// `ts_sensor_ns` is absent or on the host monotonic clock.
pub const TIMEBASE_HOST: u8 = 0;
/// `ts_sensor_ns` is on the device/driver clock.
pub const TIMEBASE_SENSOR: u8 = 1;
/// `ts_sensor_ns` is simulation time in ns.
pub const TIMEBASE_SIM: u8 = 2;

mod clock;
pub use clock::mono_ns;

mod ring;
mod vision;
pub use ring::*;
pub use vision::*;

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
    /// Host CLOCK_MONOTONIC ns: exposure midpoint when known, else arrival.
    pub ts_ns: u64,
    /// Source-clock timestamp in ns, interpreted per `timebase`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ts_sensor_ns: Option<u64>,
    /// Exposure duration in ns, when the source reports it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exposure_ns: Option<u32>,
    /// Hardware trigger sequence that produced this frame, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trigger_seq: Option<u64>,
    /// Clock domain of `ts_sensor_ns` (`TIMEBASE_*`).
    pub timebase: u8,
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

/// Plugin RPC method names for the vision surface. The plugin host gates each
/// on the matching capability before routing to the vision engine over
/// `/run/ados/vision.sock`. The request args of `REGISTER_MODEL`, `INFER`,
/// `PUBLISH_DETECTION` and `DESIGNATE_TRACK` are defined in
/// [`crate::vision_rpc`].
pub mod methods;

#[cfg(test)]
mod contract_tests;

#[cfg(test)]
mod tests;
