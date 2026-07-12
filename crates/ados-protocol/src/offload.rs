//! The perception-offload wire: the frame reference a node processes, the
//! detection it returns, and the streaming return batch (node -> drone).
//!
//! An NPU-less drone streams camera frames to a paired compute node; the node
//! runs a detector over them and streams detections back. These are the shared
//! types both halves speak so the node (`ados-compute`) and the drone-side
//! return bridge build against one frozen definition, without either crate
//! depending on the other. The pixels ride the frame transport (the RTSP/stream
//! lane); this module carries the small metadata + the returned boxes.
//!
//! The returned box is normalized (`0.0..=1.0`) so it is resolution-independent
//! on the wire; the drone denormalizes to its own frame size when it republishes
//! onto the local `vision.detection` bus.

use serde::{Deserialize, Serialize};

/// A reference to one frame a node was asked to process. Names the frame (its
/// camera, size, and capture time) so a returned detection can be tied back to
/// it; the pixels travel on the frame transport, not here.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FrameRef {
    pub camera_id: String,
    pub width: u32,
    pub height: u32,
    pub ts_ms: i64,
}

/// One detection returned to the drone. The box is normalized `[x, y, w, h]` in
/// `0.0..=1.0` so it is resolution-independent on the wire.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Detection {
    /// Normalized `[x, y, w, h]` in `0.0..=1.0`.
    pub bbox: [f32; 4],
    pub class: String,
    pub confidence: f32,
    /// A stable track id when the backend tracks across frames.
    #[serde(default)]
    pub track_id: Option<u64>,
}

/// The current wire version of an [`OffloadDetectionBatch`]. Bumped whenever the
/// batch's on-wire shape changes; a decode stamped with any other version fails
/// loudly rather than silently mis-parsing (the freeze-and-version discipline,
/// mirroring the versioned `vision.detection` batch).
pub const OFFLOAD_DETECTION_VERSION: u16 = 1;

/// One returned batch of detections for a streaming offload session (node ->
/// drone). Carries the source frame's identity + size so the drone can align the
/// normalized boxes to the frame and denormalize them to the local bus.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OffloadDetectionBatch {
    /// Wire version, stamped on every batch and checked on decode. Deliberately
    /// carries no serde default: a payload missing this field fails to decode.
    #[serde(rename = "v")]
    pub v: u16,
    /// The streaming session this batch belongs to (the drone opened it).
    pub session_id: String,
    pub camera_id: String,
    /// Monotonic per-session frame sequence number (the node stamps it).
    pub seq: u64,
    /// The source frame's capture time (from the frame the node processed).
    pub ts_ms: i64,
    /// The source frame's pixel size, so the drone denormalizes to it.
    pub width: u32,
    pub height: u32,
    pub detections: Vec<Detection>,
}

/// A batch failed to decode: a bad msgpack body, or a version this build does
/// not speak.
#[derive(Debug, thiserror::Error)]
pub enum OffloadBatchError {
    #[error("decode offload batch: {0}")]
    Decode(#[from] rmp_serde::decode::Error),
    #[error("offload batch version {got} != supported {ours}")]
    Version { got: u16, ours: u16 },
}

impl OffloadDetectionBatch {
    /// A batch at the current wire version.
    pub fn new(
        session_id: impl Into<String>,
        camera_id: impl Into<String>,
        seq: u64,
        ts_ms: i64,
        width: u32,
        height: u32,
        detections: Vec<Detection>,
    ) -> Self {
        Self {
            v: OFFLOAD_DETECTION_VERSION,
            session_id: session_id.into(),
            camera_id: camera_id.into(),
            seq,
            ts_ms,
            width,
            height,
            detections,
        }
    }

    /// Encode as a msgpack map with named keys.
    pub fn to_msgpack(&self) -> Result<Vec<u8>, rmp_serde::encode::Error> {
        rmp_serde::to_vec_named(self)
    }

    /// Decode from a msgpack map, rejecting a batch whose wire version this build
    /// does not speak. A missing `v` field fails the msgpack decode; a
    /// present-but-unknown `v` returns [`OffloadBatchError::Version`].
    pub fn from_msgpack(bytes: &[u8]) -> Result<Self, OffloadBatchError> {
        let batch: OffloadDetectionBatch = rmp_serde::from_slice(bytes)?;
        if batch.v != OFFLOAD_DETECTION_VERSION {
            return Err(OffloadBatchError::Version {
                got: batch.v,
                ours: OFFLOAD_DETECTION_VERSION,
            });
        }
        Ok(batch)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> OffloadDetectionBatch {
        OffloadDetectionBatch::new(
            "sess-1",
            "front",
            7,
            1_700_000_000_000,
            1280,
            720,
            vec![Detection {
                bbox: [0.25, 0.25, 0.5, 0.5],
                class: "person".into(),
                confidence: 0.8,
                track_id: Some(3),
            }],
        )
    }

    #[test]
    fn batch_round_trips_through_msgpack() {
        let b = sample();
        let back = OffloadDetectionBatch::from_msgpack(&b.to_msgpack().unwrap()).unwrap();
        assert_eq!(b, back);
        assert_eq!(back.v, OFFLOAD_DETECTION_VERSION);
    }

    #[test]
    fn a_wrong_version_fails_loudly() {
        let mut b = sample();
        b.v = 99;
        let err = OffloadDetectionBatch::from_msgpack(&b.to_msgpack().unwrap()).unwrap_err();
        match err {
            OffloadBatchError::Version { got, ours } => {
                assert_eq!(got, 99);
                assert_eq!(ours, OFFLOAD_DETECTION_VERSION);
            }
            other => panic!("expected a version error, got {other:?}"),
        }
    }

    #[test]
    fn a_frame_ref_round_trips() {
        let f = FrameRef {
            camera_id: "front".into(),
            width: 640,
            height: 480,
            ts_ms: 42,
        };
        let bytes = rmp_serde::to_vec_named(&f).unwrap();
        let back: FrameRef = rmp_serde::from_slice(&bytes).unwrap();
        assert_eq!(f, back);
    }
}
