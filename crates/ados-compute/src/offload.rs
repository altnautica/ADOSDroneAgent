//! The perception-offload backend: a detector the compute node runs over frames
//! streamed from an NPU-less drone, returning detections (and, for a SLAM
//! offload, poses) so the drone's autonomous behaviours run on borrowed
//! compute. Real backends run an ONNX / TensorRT / RKNN model; the mock keeps
//! the offload path testable with no model and no camera.
//!
//! The fast control loop stays on the drone; this is the slow perception lane.
//! A consumer treats a stale or link-lost result as lost (the safety gate lives
//! on the drone side).

use crate::ComputeError;

// The offload wire (the frame reference + the returned detection) lives in
// `ados_protocol::offload` so this node crate and the drone-side return bridge
// build against one frozen definition without depending on each other.
pub use ados_protocol::offload::{Detection, FrameRef};

/// A detection backend. `Send + Sync` so a worker pool can share one.
pub trait Detector: Send + Sync {
    fn name(&self) -> &str;

    /// Whether this backend actually runs a model. The mock returns a fixed box
    /// with no accelerator, so a node wired to it produces placeholder
    /// detections; a status surface flags that rather than presenting it as a
    /// working offload (Rule 44). A real backend overrides to `true`.
    fn is_inference_capable(&self) -> bool {
        false
    }

    /// Run detection on one frame. `pixels` carries the decoded RGB24 image
    /// (`width * height * 3` bytes, row-major) when a live frame is available:
    /// the streaming transport feeds it (W2) and a direct caller passes it. It
    /// is `None` on the metadata-only path (the mock, and the job-params path
    /// before the transport lands). A real backend requires pixels and errors
    /// without them, rather than fabricating a box.
    fn infer(
        &self,
        frame: &FrameRef,
        pixels: Option<&[u8]>,
    ) -> Result<Vec<Detection>, ComputeError>;
}

/// A no-model detector that returns one deterministic detection. Exercises the
/// offload request/response path with no accelerator.
#[derive(Debug, Default, Clone, Copy)]
pub struct MockDetector;

impl Detector for MockDetector {
    fn name(&self) -> &str {
        "mock"
    }

    fn infer(
        &self,
        frame: &FrameRef,
        _pixels: Option<&[u8]>,
    ) -> Result<Vec<Detection>, ComputeError> {
        // A single centered box with a stable track id, keyed off the frame so
        // a caller can confirm the result belongs to the frame it sent. The mock
        // ignores the pixels: it exercises the path without a model.
        Ok(vec![Detection {
            bbox: [0.4, 0.4, 0.2, 0.2],
            class: "object".into(),
            confidence: 0.9,
            track_id: Some(frame.ts_ms.unsigned_abs() % 1000),
        }])
    }
}

/// Normalize a pixel-space `ados-vision` detection (box in the frame's own
/// resolution) into the offload wire shape (box in `0.0..=1.0`). Pulled out so
/// it is unit-testable without a model. Returns `None` for a box-less percept
/// (a mask/pose/depth-only reading): the offload wire is box-based, so a percept
/// with no 2D box is dropped from the offload result rather than forced to zero.
#[cfg(feature = "onnx")]
fn to_offload_detection(
    d: ados_protocol::framebus::Detection,
    frame_w: u32,
    frame_h: u32,
) -> Option<Detection> {
    let bbox = d.bbox?;
    let fw = (frame_w.max(1)) as f32;
    let fh = (frame_h.max(1)) as f32;
    Some(Detection {
        bbox: [bbox.x / fw, bbox.y / fh, bbox.width / fw, bbox.height / fh],
        class: d.class_label,
        confidence: d.confidence,
        track_id: d.track_id,
    })
}

/// A real ONNX detector: the compute node hosts `ados-vision`'s ONNX backend
/// (the same YOLO decode the drone runs on its NPU) and runs it over frames an
/// NPU-less drone streams in, so the offloaded result is identical to a local
/// detection. On Apple Silicon the ONNX session registers the CoreML execution
/// provider so inference runs on the Mac GPU. Built only under the `onnx`
/// feature; the mock keeps the path testable in CI where no model is present.
#[cfg(feature = "onnx")]
pub struct OnnxDetector {
    model: Box<dyn ados_vision::backend::LoadedModel>,
    name: String,
}

#[cfg(feature = "onnx")]
impl OnnxDetector {
    /// Load `meta` (model path + input dims + head + class labels) into an ONNX
    /// session. Fails if the model file is missing or will not load.
    pub fn from_model(meta: &ados_protocol::framebus::ModelMetadata) -> Result<Self, ComputeError> {
        use ados_vision::backend::VisionBackend;
        let backend = ados_vision::backend::OnnxBackend::new();
        let model = backend.load(meta).map_err(|e| ComputeError::Backend {
            backend: "onnx".into(),
            message: format!("load {}: {e}", meta.id),
        })?;
        Ok(Self {
            model,
            name: format!("onnx:{}", meta.id),
        })
    }
}

#[cfg(feature = "onnx")]
impl Detector for OnnxDetector {
    fn name(&self) -> &str {
        &self.name
    }

    fn is_inference_capable(&self) -> bool {
        true
    }

    fn infer(
        &self,
        frame: &FrameRef,
        pixels: Option<&[u8]>,
    ) -> Result<Vec<Detection>, ComputeError> {
        let px = pixels.ok_or_else(|| ComputeError::Backend {
            backend: "onnx".into(),
            message: "perception offload needs frame pixels (rgb24); none supplied".into(),
        })?;
        let dets = self
            .model
            .infer(
                px,
                frame.width,
                frame.height,
                ados_protocol::framebus::FrameFormat::Rgb24,
            )
            .map_err(|e| ComputeError::Backend {
                backend: "onnx".into(),
                message: e.to_string(),
            })?;
        Ok(dets
            .into_iter()
            .filter_map(|d| to_offload_detection(d, frame.width, frame.height))
            .collect())
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
        let dets = MockDetector.infer(&frame, None).unwrap();
        assert_eq!(dets.len(), 1);
        assert_eq!(dets[0].class, "object");
        assert_eq!(dets[0].track_id, Some(42));
        assert_eq!(MockDetector.name(), "mock");
        assert!(!MockDetector.is_inference_capable());
    }

    #[cfg(feature = "onnx")]
    #[test]
    fn to_offload_detection_normalizes_the_pixel_box() {
        use ados_protocol::framebus::{BoundingBox, Detection as VDet};
        let v = VDet {
            bbox: Some(BoundingBox {
                x: 320.0,
                y: 180.0,
                width: 640.0,
                height: 360.0,
            }),
            class_label: "person".into(),
            confidence: 0.8,
            track_id: Some(3),
            assoc_confidence: None,
            lock_state: None,
            attributes: None,
            mask: None,
            keypoints: None,
            depth: None,
            world_pos: None,
        };
        let out = to_offload_detection(v, 1280, 720).expect("a boxed detection normalizes");
        assert_eq!(out.bbox, [0.25, 0.25, 0.5, 0.5]);
        assert_eq!(out.class, "person");
        assert_eq!(out.confidence, 0.8);
        assert_eq!(out.track_id, Some(3));
    }
}
