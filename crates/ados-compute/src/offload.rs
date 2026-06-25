//! The perception-offload backend: a detector the compute node runs over frames
//! streamed from an NPU-less drone, returning detections (and, for a SLAM
//! offload, poses) so the drone's autonomous behaviours run on borrowed
//! compute. Real backends run an ONNX / TensorRT / RKNN model; the mock keeps
//! the offload path testable with no model and no camera.
//!
//! The fast control loop stays on the drone; this is the slow perception lane.
//! A consumer treats a stale or link-lost result as lost (the safety gate lives
//! on the drone side).

use serde::{Deserialize, Serialize};

use crate::ComputeError;

/// A reference to one frame the node was asked to process. The real lane
/// carries the pixels over a shared-memory ring or the stream lane; this names
/// the frame so a detection can be tied back to it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FrameRef {
    pub camera_id: String,
    pub width: u32,
    pub height: u32,
    pub ts_ms: i64,
}

/// One detection returned to the drone.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Detection {
    /// Normalized `[x, y, w, h]` in `0.0..=1.0`.
    pub bbox: [f32; 4],
    pub class: String,
    pub confidence: f32,
    /// A stable track id when the backend tracks across frames.
    pub track_id: Option<u64>,
}

/// A detection backend. `Send + Sync` so a worker pool can share one.
pub trait Detector: Send + Sync {
    fn name(&self) -> &str;

    /// Run detection on one frame.
    fn infer(&self, frame: &FrameRef) -> Result<Vec<Detection>, ComputeError>;
}

/// A no-model detector that returns one deterministic detection. Exercises the
/// offload request/response path with no accelerator.
#[derive(Debug, Default, Clone, Copy)]
pub struct MockDetector;

impl Detector for MockDetector {
    fn name(&self) -> &str {
        "mock"
    }

    fn infer(&self, frame: &FrameRef) -> Result<Vec<Detection>, ComputeError> {
        // A single centered box with a stable track id, keyed off the frame so
        // a caller can confirm the result belongs to the frame it sent.
        Ok(vec![Detection {
            bbox: [0.4, 0.4, 0.2, 0.2],
            class: "object".into(),
            confidence: 0.9,
            track_id: Some(frame.ts_ms.unsigned_abs() % 1000),
        }])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mock_returns_one_detection_for_the_frame() {
        let frame = FrameRef {
            camera_id: "front".into(),
            width: 1280,
            height: 720,
            ts_ms: 42,
        };
        let dets = MockDetector.infer(&frame).unwrap();
        assert_eq!(dets.len(), 1);
        assert_eq!(dets[0].class, "object");
        assert_eq!(dets[0].track_id, Some(42));
        assert_eq!(MockDetector.name(), "mock");
    }
}
