//! Detection and model metadata types carried on the vision bus.

use super::*;

/// What a model produces, so consumers know how to read its output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ModelKind {
    Detection,
    Segmentation,
    Classification,
    Tracking,
    /// Keypoint / pose estimation: the model emits per-detection `keypoints`
    /// (skeleton joints or landmarks) rather than only a box.
    Pose,
    /// Monocular depth estimation: the model emits a per-detection `depth`
    /// (metres) so a consumer can place the object in space.
    Depth,
    /// Appearance re-identification: the model emits an embedding used to
    /// re-associate the same object across frames (feeds the tracker, not a box).
    Reid,
}

/// A registered model's info, for the engine's list-models read-back (the
/// [`methods::LIST_MODELS`] reply). Lets the GCS vision hub show every model
/// loaded on the drone — its task, how it runs, whether the engine loaded a
/// backend for it — not only the models currently publishing detections.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelInfo {
    pub id: String,
    pub kind: ModelKind,
    pub execution: ModelExecution,
    /// True when the engine holds a loaded backend model for it (an engine-run
    /// model whose file loaded); false for a plugin-side or failed-to-load model.
    pub backend_loaded: bool,
    pub output_classes: Vec<String>,
    /// Measured inference rate in Hz (an EWMA over inter-inference intervals).
    /// `0.0` when the model has not run yet — the honest "no data" reading, not a
    /// claim of zero throughput. Additive: an older payload without it decodes to
    /// `0.0`.
    #[serde(default)]
    pub fps: f32,
    /// Measured per-inference latency in ms (an EWMA). `0.0` until the model runs.
    #[serde(default, rename = "latencyMs")]
    pub latency_ms: f32,
    /// Whether this model actually runs a real backend (an engine-run model whose
    /// file loaded on an inference-capable backend). A mock-backend engine reports
    /// `false` so a status surface never presents a placeholder as a working
    /// detector. Additive; defaults `false`.
    #[serde(default, rename = "isInferenceCapable")]
    pub is_inference_capable: bool,
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

/// One keypoint of a detection (a pose joint or a landmark), in the frame's own
/// pixel resolution, with its own detection confidence.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Keypoint {
    pub x: f32,
    pub y: f32,
    /// Confidence this keypoint was located correctly, `0..1`.
    pub confidence: f32,
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

/// One percept from a model: the self-describing per-frame unit a consumer
/// reads. It began as a 2D box and grows by ADDING typed optional fields (each
/// serde-default + skipped when absent), so a producer emits only what it
/// measured and an older producer/reader round-trips unchanged. The 2D box is
/// itself now optional: a box detector always sets it, while a box-less percept
/// (a segmentation mask, a pose, a depth-only reading) may omit it and carry its
/// geometry in the typed fields below.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Detection {
    /// The 2D bounding box in the frame's own pixel resolution. `Some` for the
    /// common box detector (always set before box-less percepts existed, so an
    /// older producer's payload still decodes to `Some`); `None` for a box-less
    /// percept. Skipped on the wire when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bbox: Option<BoundingBox>,
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
    /// Open, self-describing per-detection attributes: the extension point for
    /// richer perception a model may carry beyond the 2D box — a segmentation
    /// mask reference, keypoints, a depth estimate, a world-frame position — as
    /// well as any model-specific metadata, keyed by name. A concrete primitive
    /// promotes to its own typed field once a consumer needs it as first-class;
    /// until then it rides here so the contract extends without a schema change.
    /// Absent (skipped on the wire) when the source reports none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attributes: Option<BTreeMap<String, rmpv::Value>>,
    /// Segmentation mask as a polygon of `(x, y)` vertices tracing the object
    /// outline, in the frame's own pixel resolution. `None` when the source
    /// reports no mask.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mask: Option<Vec<[f32; 2]>>,
    /// Keypoints (pose joints or landmarks) for this detection, in frame pixels.
    /// `None` when the source reports none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keypoints: Option<Vec<Keypoint>>,
    /// Estimated distance from the camera to the detection, in metres. `None`
    /// when the source reports no depth.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub depth: Option<f32>,
    /// The detection's position in a world frame, as a bare 3-array so the frame
    /// convention can evolve without a schema change: local ENU metres
    /// `[x, y, z]`, or geographic `[lat, lon, alt]` once a producer geolocates.
    /// `None` when the source reports no world position.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub world_pos: Option<[f64; 3]>,
}

/// The current wire version of a [`DetectionBatch`] on the `vision.detection`
/// topic. Bumped whenever the batch's on-wire shape changes; a decode of a batch
/// stamped with any other version fails loudly rather than silently mis-parsing.
/// Defined as the `vision.detection` row of the generated contract registry, so
/// `contracts.toml` is the only place it is set.
pub const VISION_DETECTION_VERSION: u16 = crate::contracts::contract_version("vision.detection")
    .expect("vision.detection is registered in contracts.toml");

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
    /// Capture time of the source frame: the frame's host CLOCK_MONOTONIC
    /// timestamp in milliseconds (`FrameDescriptor::ts_ns / 1_000_000`).
    pub ts_ms: i64,
    /// The source frame's pixel size, so a consumer scales the pixel-space boxes
    /// to the rendered frame. `0` when the source did not report it (the consumer
    /// then falls back to its own default). Additive + serde-default, so a batch
    /// from an older producer (no dims) still decodes (mirrors `attributes`).
    #[serde(default)]
    pub frame_width: u32,
    #[serde(default)]
    pub frame_height: u32,
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
