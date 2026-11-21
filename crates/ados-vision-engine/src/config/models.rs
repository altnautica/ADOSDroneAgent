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

/// The detector's pace when `detector.rate_hz` is absent: ten looks a second
/// per camera, so a designated subject is measured every 100 ms and the
/// follow loop never waits a third of a second for its next fix. Where the
/// accelerator cannot keep up, frames are skipped (latest wins) and the
/// detector runs as fast as it finishes.
fn default_detector_rate_hz() -> f32 {
    10.0
}

/// The whole-frame pass's pace while a designated subject is tracked on a
/// crop: two looks a second at the whole scene, for a subject that leaves the
/// crop and for the other people around it.
fn default_full_frame_hz() -> f32 {
    2.0
}

pub(super) fn default_threads() -> u32 {
    1
}

/// The detector's inference threads when `detector.threads` is absent: two,
/// the most that still leaves a quad-core companion's other cores to the
/// flight services, and what a two-big-core board runs fastest on.
fn default_detector_threads() -> u32 {
    2
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
    /// Output-head layout: `yolo8` (default), `yolo5` or `yolox` (the
    /// per-stride raw heads the NPU sidecars decode).
    #[serde(default)]
    pub head: String,
    /// Class labels in output-index order (e.g. the COCO-80 list). Empty ⇒
    /// detections carry no class name (the track id still flows).
    #[serde(default)]
    pub class_labels: Vec<String>,
    /// Target inference rate in Hz, per camera. Absent ⇒ 10 Hz. An explicit
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
    /// is one, and neither spin-waits). Default 2.
    #[serde(default = "default_detector_threads")]
    pub threads: u32,
    /// The model file the crop pass runs, when it is not the detector itself:
    /// the same detector exported at a smaller input (a 320 px crop holds a
    /// designated subject as well as a 640 px one, at a quarter of the cost).
    /// Its head and classes are the detector's; its input size is
    /// `crop_input_size`, or the size a self-describing file states
    /// (`ados.input`, read by the ONNX backend). Absent ⇒ the crop pass runs
    /// the detector model.
    #[serde(default)]
    pub crop_model_path: Option<String>,
    /// The crop model's square input size in pixels. Absent ⇒ the detector's
    /// input size, which a self-describing file overrides.
    #[serde(default)]
    pub crop_input_size: Option<u32>,
    /// While a designated subject is tracked, the batches between whole-frame
    /// passes run only the crop around it; the whole frame runs at this rate
    /// (Hz) to find a subject that left the crop and the people around it.
    /// Absent ⇒ 2 Hz. At or above `rate_hz`, or `0`, every batch runs the
    /// whole frame (and the crop only when it magnifies the subject).
    #[serde(default = "default_full_frame_hz")]
    pub full_frame_hz: f32,
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
                "yolox" => DetectionHead::Yolox,
                _ => DetectionHead::Yolo8,
            },
        }
    }

    /// The crop pass's model, when `crop_model_path` names one: registered
    /// as `<model_id>.crop`, with the detector's head and classes.
    pub fn crop_metadata(&self) -> Option<ModelMetadata> {
        let path = self.crop_model_path.as_ref()?;
        let detector = self.to_metadata();
        let size = self.crop_input_size.filter(|s| *s > 0);
        Some(ModelMetadata {
            id: format!("{}.crop", self.model_id),
            model_path: Some(path.clone()),
            input_width: size.unwrap_or(detector.input_width),
            input_height: size.unwrap_or(detector.input_height),
            ..detector
        })
    }

    /// The whole-frame pass's interval while a designated subject is tracked
    /// on a crop, in ns; `0` when every batch runs the whole frame.
    pub fn full_frame_interval_ns(&self) -> u64 {
        let fhz = self.full_frame_hz;
        if !(fhz.is_finite() && fhz > 0.0) || (self.rate_hz > 0.0 && fhz >= self.rate_hz) {
            return 0;
        }
        (1e9 / f64::from(fhz)).round() as u64
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

/// The depth model's registry id, and its file name in the models directory
/// (`.onnx` for the CPU runtime, `.rknn` for the Rockchip accelerator):
/// Depth Anything V2 Small, relative inverse depth, Apache-2.0.
pub const DEFAULT_DEPTH_MODEL_ID: &str = "depth_anything_v2_small";
pub const DEFAULT_DEPTH_FILE_STEM: &str = "depth_anything_v2_small_252x196";

/// The depth pace on a CPU runtime and on an accelerator, Hz.
pub const DEPTH_RATE_CPU_HZ: f32 = 2.0;
pub const DEPTH_RATE_ACCEL_HZ: f32 = 5.0;

fn default_depth_model_id() -> String {
    DEFAULT_DEPTH_MODEL_ID.to_string()
}

/// The default model's input: its ViT reads 14-pixel patches, so the size is
/// the multiple of 14 nearest 256 x 192.
fn default_depth_width() -> u32 {
    252
}

fn default_depth_height() -> u32 {
    196
}

fn default_depth_threads() -> u32 {
    2
}

fn default_true() -> bool {
    true
}

/// `vision.depth`: the monocular depth model behind the depth and surfaces
/// layers. It runs on a camera only while a depth or surfaces lease is held
/// there. Every key is optional: with no block at all the engine looks for
/// the default model in the models directory, so installing the model from
/// the registry is enough to turn the layers on.
#[derive(Debug, Clone, Deserialize)]
pub struct DepthConfig {
    /// `false` turns the depth layer off even when a model is installed.
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_depth_model_id")]
    pub model_id: String,
    /// The model file; absent ⇒ the default file in the models directory.
    #[serde(default)]
    pub model_path: Option<String>,
    #[serde(default = "default_depth_width")]
    pub input_width: u32,
    #[serde(default = "default_depth_height")]
    pub input_height: u32,
    /// Runs per second per camera; absent ⇒ 2 on the CPU runtime, 5 on an
    /// accelerator.
    #[serde(default)]
    pub rate_hz: Option<f32>,
    /// ONNX Runtime intra-op threads for the depth session. Default 2.
    #[serde(default = "default_depth_threads")]
    pub threads: u32,
}

impl Default for DepthConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            model_id: default_depth_model_id(),
            model_path: None,
            input_width: default_depth_width(),
            input_height: default_depth_height(),
            rate_hz: None,
            threads: default_depth_threads(),
        }
    }
}

/// The depth model the engine should load, or why there is none.
#[derive(Debug, Clone, PartialEq)]
pub enum DepthPlan {
    /// Load this model and run it at most once per `interval_ms` per camera.
    Load {
        meta: ModelMetadata,
        interval_ms: u64,
    },
    /// No depth model: the reason says what is missing.
    None(String),
}

impl DepthConfig {
    /// Resolve the model for `backend` (`onnx`, `rknn`, `hailo`, `mock`, ...)
    /// with files under `models_dir`. `exists` checks a path (injected for
    /// tests).
    pub fn plan(
        &self,
        backend: &str,
        models_dir: &str,
        exists: impl Fn(&str) -> bool,
    ) -> DepthPlan {
        if !self.enabled {
            return DepthPlan::None("vision.depth.enabled is false".into());
        }
        let (ext, accel) = match backend {
            "rknn" => ("rknn", true),
            "hailo" => {
                return DepthPlan::None(
                    "the hailo accelerator sidecar does not run depth models".into(),
                )
            }
            _ => ("onnx", false),
        };
        let path = self.model_path.clone().unwrap_or_else(|| {
            format!(
                "{}/{DEFAULT_DEPTH_FILE_STEM}.{ext}",
                models_dir.trim_end_matches('/')
            )
        });
        // The mock runs a synthetic scene and needs no file.
        if backend != "mock" && !exists(&path) {
            return DepthPlan::None(format!(
                "no depth model at {path}; install {} from the model registry",
                self.model_id
            ));
        }
        let rate = self
            .rate_hz
            .filter(|r| r.is_finite() && *r > 0.0)
            .unwrap_or(if accel {
                DEPTH_RATE_ACCEL_HZ
            } else {
                DEPTH_RATE_CPU_HZ
            });
        DepthPlan::Load {
            meta: ModelMetadata {
                id: self.model_id.clone(),
                kind: ModelKind::Depth,
                execution: ModelExecution::EngineRun,
                input_width: if self.input_width == 0 {
                    default_depth_width()
                } else {
                    self.input_width
                },
                input_height: if self.input_height == 0 {
                    default_depth_height()
                } else {
                    self.input_height
                },
                input_format: FrameFormat::Rgb24,
                output_classes: Vec::new(),
                model_path: Some(path),
                head: DetectionHead::Yolo8,
            },
            interval_ms: (1000.0 / rate).round() as u64,
        }
    }
}
