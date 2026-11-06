//! The model blocks of the `vision:` config: the detector the engine runs and
//! the appearance (re-id) model it loads beside it.

use ados_protocol::framebus::{
    DetectionHead, FrameFormat, ModelExecution, ModelKind, ModelMetadata,
};
use serde::Deserialize;

fn default_detector_dim() -> u32 {
    640
}

fn default_reid_width() -> u32 {
    128
}

fn default_reid_height() -> u32 {
    256
}

/// The detector's pace when `detector.rate_hz` is absent: a few looks a second
/// per camera, enough to acquire and hand a target to the tracker.
fn default_detector_rate_hz() -> f32 {
    3.0
}

pub(super) fn default_threads() -> u32 {
    1
}

/// A detector model the engine auto-loads and runs, at `rate_hz`, on each
/// captured camera detection is wanted on (see `detection_demand`), to
/// produce the `vision.detection` stream that follow / designate plugins
/// subscribe to. Absent ⇒ the engine produces no detections on its own (a
/// plugin must drive inference through the `vision.sock` register/infer RPCs).
#[derive(Debug, Clone, Deserialize)]
pub struct DetectorConfig {
    /// Model id stamped on every published `DetectionBatch`.
    pub model_id: String,
    /// Resolved path to the model file (`.rknn` on an NPU board, `.onnx` on a
    /// CPU build). Provisioning resolves a registry id to this path; a sideload
    /// sets it directly.
    pub model_path: String,
    #[serde(default = "default_detector_dim")]
    pub input_width: u32,
    #[serde(default = "default_detector_dim")]
    pub input_height: u32,
    /// Output-head layout: `yolo8` (default) or `yolo5`.
    #[serde(default)]
    pub head: String,
    /// Class labels in output-index order (e.g. the COCO-80 list). Empty ⇒
    /// detections carry no class name (the track id still flows).
    #[serde(default)]
    pub class_labels: Vec<String>,
    /// Target inference rate in Hz, per camera. Absent ⇒ 3 Hz. An explicit
    /// `0` (or less) is unpaced: run on every captured frame the accelerator
    /// is free for. Pacing lets this detector share the one accelerator with
    /// other models (`person@15Hz` + `depth@5Hz`) instead of monopolising it.
    #[serde(default = "default_detector_rate_hz")]
    pub rate_hz: f32,
    /// Scheduling priority (higher runs first when several models are due at
    /// once). Absent ⇒ a high default so a lone detector always wins.
    #[serde(default)]
    pub priority: Option<u8>,
    /// Inference threads the detector runs on (ONNX Runtime intra-op; inter-op
    /// is one, and neither spin-waits). Default 1.
    #[serde(default = "default_threads")]
    pub threads: u32,
}

impl DetectorConfig {
    /// Build the engine-run [`ModelMetadata`] for this detector. Frames are fed
    /// as RGB24; the head defaults to YOLOv8 (the current export).
    pub fn to_metadata(&self) -> ModelMetadata {
        ModelMetadata {
            id: self.model_id.clone(),
            kind: ModelKind::Detection,
            execution: ModelExecution::EngineRun,
            input_width: if self.input_width == 0 {
                default_detector_dim()
            } else {
                self.input_width
            },
            input_height: if self.input_height == 0 {
                default_detector_dim()
            } else {
                self.input_height
            },
            input_format: FrameFormat::Rgb24,
            output_classes: self.class_labels.clone(),
            model_path: Some(self.model_path.clone()),
            head: match self.head.to_ascii_lowercase().as_str() {
                "yolo5" | "yolov5" => DetectionHead::Yolo5,
                _ => DetectionHead::Yolo8,
            },
        }
    }
}

/// An appearance (re-id) model the engine loads alongside the detector. Its
/// embeddings feed the re-id tracker (`reid_enabled`) and any subscriber that
/// asks for embeddings on the detection wire. Absent (or failing to load) ⇒
/// both fall back to the model-free colour histogram descriptor.
#[derive(Debug, Clone, Deserialize)]
pub struct ReidConfig {
    /// Model id the engine registers the re-id model under; it must match the
    /// `reid_model_id` the tracker looks up.
    pub model_id: String,
    /// Resolved path to the re-id model file (`.rknn` on an NPU board, `.onnx`
    /// on a CPU build). Provisioning resolves a registry id to this path; a
    /// sideload sets it directly.
    pub model_path: String,
    /// Model input width. OSNet-class person re-id is portrait: 128 wide.
    #[serde(default = "default_reid_width")]
    pub input_width: u32,
    /// Model input height. OSNet-class person re-id is portrait: 256 tall.
    #[serde(default = "default_reid_height")]
    pub input_height: u32,
}

impl ReidConfig {
    /// Build the engine-run [`ModelMetadata`] for this re-id model. Crops are fed
    /// as RGB24 at the model's input size; it carries no class labels (an
    /// embedder, not a detector). The head is unused for embedding but the
    /// metadata requires one, so it defaults to YOLOv8.
    pub fn to_metadata(&self) -> ModelMetadata {
        ModelMetadata {
            id: self.model_id.clone(),
            kind: ModelKind::Tracking,
            execution: ModelExecution::EngineRun,
            input_width: if self.input_width == 0 {
                default_reid_width()
            } else {
                self.input_width
            },
            input_height: if self.input_height == 0 {
                default_reid_height()
            } else {
                self.input_height
            },
            input_format: FrameFormat::Rgb24,
            output_classes: Vec::new(),
            model_path: Some(self.model_path.clone()),
            head: DetectionHead::Yolo8,
        }
    }
}
