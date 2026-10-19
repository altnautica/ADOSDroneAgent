//! Vision engine configuration, read from the `vision:` block of
//! `/etc/ados/config.yaml`, plus the cameras the engine owns (the top-level
//! `cameras[]` entries marked `capture: true`), the video pipeline's frame tap
//! (`video.vision`) and the simulation flag. Every field is `#[serde(default)]`
//! so a partial or malformed config never blocks startup — a missing `vision:`
//! block yields a disabled engine, which is the safe default on a rig that
//! does not run vision.

use ados_config::cameras::{capture_cameras_from, CaptureCamera};
use ados_protocol::framebus::{
    DetectionHead, FrameFormat, ModelExecution, ModelKind, ModelMetadata,
};
use serde::Deserialize;

fn default_socket_dir() -> String {
    "/run/ados".to_string()
}
fn default_downscale_width() -> u32 {
    640
}
fn default_downscale_height() -> u32 {
    480
}
fn default_backend() -> String {
    "auto".to_string()
}
fn default_slot_count() -> u32 {
    4
}
/// Default per-camera ring budget in megabytes, matching
/// [`crate::ring::DEFAULT_RING_BUDGET_BYTES`]. `/dev/shm` is tmpfs, so this is
/// resident RAM taken from the same pool the encoder and the model use.
fn default_shm_budget_mb() -> u32 {
    (crate::ring::DEFAULT_RING_BUDGET_BYTES / (1024 * 1024)) as u32
}

fn default_detector_dim() -> u32 {
    640
}

fn default_reid_width() -> u32 {
    128
}

fn default_reid_height() -> u32 {
    256
}

/// A detector model the engine auto-loads and runs on every captured frame to
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
    /// Target inference rate in Hz. Absent (or `<= 0`) ⇒ unpaced (run on every
    /// captured frame), which is the historical single-detector behaviour. Set it
    /// to pace this detector so it shares the one accelerator with other models
    /// (`person@15Hz` + `depth@5Hz`) instead of monopolising it.
    #[serde(default)]
    pub rate_hz: Option<f32>,
    /// Scheduling priority (higher runs first when several models are due at
    /// once). Absent ⇒ a high default so a lone detector always wins.
    #[serde(default)]
    pub priority: Option<u8>,
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

/// An appearance (re-id) model the engine loads alongside the detector so the
/// tracker can re-identify its locked subject by learned appearance, not motion
/// alone. Required when `reid_enabled` is set; absent ⇒ the tracker stays
/// motion-only even with `reid_enabled` (the engine degrades cleanly).
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

/// The `vision:` config block.
#[derive(Debug, Clone, Deserialize)]
pub struct VisionConfig {
    /// Master enable. The engine exits cleanly when false.
    #[serde(default)]
    pub enabled: bool,
    /// Agent profile, resolved from `agent.profile`. The engine exits early on
    /// a ground-station profile (no air-side cameras).
    #[serde(default)]
    pub profile: Option<String>,
    /// Directory the engine binds `vision.sock` in and resolves default tap /
    /// sidecar socket paths under.
    #[serde(default = "default_socket_dir")]
    pub socket_dir: String,
    /// The cameras this engine owns: the enabled `capture: true` entries of
    /// the top-level `cameras[]` list, trigger channels resolved.
    #[serde(skip)]
    pub cameras: Vec<CaptureCamera>,
    /// The video pipeline's raw frame tap of the primary stream
    /// (`video.vision.sink`) when `video.vision.enabled`; the engine reads it
    /// as camera `main`.
    #[serde(skip)]
    pub tap_sink: Option<String>,
    /// `simulation.enabled`: frames carry simulator time, which the engine
    /// reports to the clock.
    #[serde(skip)]
    pub simulation: bool,
    /// Width frames are downscaled to before publishing.
    #[serde(default = "default_downscale_width")]
    pub downscale_width: u32,
    /// Height frames are downscaled to before publishing.
    #[serde(default = "default_downscale_height")]
    pub downscale_height: u32,
    /// Slots per camera ring (latest-wins recycling depth).
    #[serde(default = "default_slot_count")]
    pub slot_count: u32,
    /// Ceiling, in megabytes, on ONE camera ring's `/dev/shm` footprint. The
    /// depth above is reduced to fit it, because the slot size is the camera's
    /// frame size and is not known until a frame arrives.
    #[serde(default = "default_shm_budget_mb")]
    pub shm_budget_mb: u32,
    /// Backend preference: "auto" (pick by SoC) | "mock" | "onnx" | "rknn" | "hailo".
    #[serde(default = "default_backend")]
    pub backend: String,
    /// Bench aid for the mock backend: emit one synthetic `person` detection
    /// that orbits the frame centre, so detection consumers can be exercised
    /// without a real model. The backend still reports itself as not
    /// inference-capable. Default off.
    #[serde(default)]
    pub mock_target: bool,
    /// Run the engine-side single-object tracker between inference and publish so
    /// every published detection carries a stable `track_id` + `lock_state`.
    /// Default off: the engine publishes raw detections, exactly as before.
    #[serde(default)]
    pub tracker_enabled: bool,
    /// Build the tracker with an appearance (re-id) model so identity survives a
    /// crossing. Requires `tracker_enabled` and a resident re-id model; default
    /// off. When off the tracker is motion-only.
    #[serde(default)]
    pub reid_enabled: bool,
    /// The re-id model id the appearance model loads in the sidecar (when
    /// `reid_enabled`). Resolved through the model registry like any other model.
    #[serde(default)]
    pub reid_model_id: Option<String>,
    /// The camera id an operator-driven designate/follow flow targets by default
    /// (the click-to-follow camera). `None` ⇒ no default; the caller names the
    /// camera per request.
    #[serde(default)]
    pub designate_camera: Option<String>,
    /// A detector the engine auto-loads and runs on every captured frame to
    /// produce the detection stream. Absent ⇒ no engine-driven detections.
    #[serde(default)]
    pub detector: Option<DetectorConfig>,
    /// The appearance (re-id) model the engine loads when `reid_enabled`. Absent
    /// ⇒ the tracker stays motion-only even with `reid_enabled`.
    #[serde(default)]
    pub reid: Option<ReidConfig>,
}

impl Default for VisionConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            profile: None,
            socket_dir: default_socket_dir(),
            cameras: Vec::new(),
            tap_sink: None,
            simulation: false,
            downscale_width: default_downscale_width(),
            downscale_height: default_downscale_height(),
            slot_count: default_slot_count(),
            shm_budget_mb: default_shm_budget_mb(),
            backend: default_backend(),
            mock_target: false,
            tracker_enabled: false,
            reid_enabled: false,
            reid_model_id: None,
            designate_camera: None,
            detector: None,
            reid: None,
        }
    }
}

impl VisionConfig {
    /// Load from `/etc/ados/config.yaml`. Returns defaults (disabled) when the
    /// file is missing or unparseable so startup never blocks on config.
    pub fn load_from(path: &std::path::Path) -> Self {
        #[derive(Debug, Default, Deserialize)]
        struct RawConfig {
            #[serde(default)]
            agent: AgentSection,
            #[serde(default)]
            vision: Option<VisionSection>,
            #[serde(default)]
            video: VideoSection,
        }
        #[derive(Debug, Default, Deserialize)]
        struct VideoSection {
            #[serde(default)]
            vision: TapSection,
        }
        #[derive(Debug, Default, Deserialize)]
        struct TapSection {
            #[serde(default)]
            enabled: bool,
            #[serde(default)]
            sink: Option<String>,
        }
        #[derive(Debug, Default, Deserialize)]
        struct AgentSection {
            #[serde(default)]
            profile: Option<String>,
        }
        #[derive(Debug, Deserialize)]
        struct VisionSection {
            #[serde(default)]
            enabled: bool,
            #[serde(default)]
            profile: Option<String>,
            #[serde(default = "default_socket_dir")]
            socket_dir: String,
            #[serde(default = "default_downscale_width")]
            downscale_width: u32,
            #[serde(default = "default_downscale_height")]
            downscale_height: u32,
            #[serde(default = "default_slot_count")]
            slot_count: u32,
            #[serde(default = "default_shm_budget_mb")]
            shm_budget_mb: u32,
            #[serde(default = "default_backend")]
            backend: String,
            #[serde(default)]
            mock_target: bool,
            #[serde(default)]
            tracker_enabled: bool,
            #[serde(default)]
            reid_enabled: bool,
            #[serde(default)]
            reid_model_id: Option<String>,
            #[serde(default)]
            designate_camera: Option<String>,
            #[serde(default)]
            detector: Option<DetectorConfig>,
            #[serde(default)]
            reid: Option<ReidConfig>,
        }

        let Ok(text) = std::fs::read_to_string(path) else {
            return VisionConfig::default();
        };
        let raw: RawConfig = ados_config::yaml_or_default(&text, "vision");
        let cameras = capture_cameras_from(&text);
        let simulation = ados_config::simulation::read_gate_from(&text).enabled;
        let tap_sink = raw.video.vision.enabled.then(|| {
            raw.video
                .vision
                .sink
                .clone()
                .unwrap_or_else(|| "/run/ados/vision-tap-main.sock".to_string())
        });
        let Some(v) = raw.vision else {
            // No `vision:` block ⇒ disabled, but still carry the agent profile
            // so the ground-station gate is consistent.
            return VisionConfig {
                profile: raw.agent.profile,
                cameras,
                tap_sink,
                simulation,
                ..VisionConfig::default()
            };
        };
        VisionConfig {
            enabled: v.enabled,
            // `agent.profile` is canonical; a `vision.profile` override pins it.
            profile: v.profile.or(raw.agent.profile),
            socket_dir: v.socket_dir,
            cameras,
            tap_sink,
            simulation,
            downscale_width: v.downscale_width,
            downscale_height: v.downscale_height,
            slot_count: v.slot_count,
            shm_budget_mb: v.shm_budget_mb,
            backend: v.backend,
            mock_target: v.mock_target,
            tracker_enabled: v.tracker_enabled,
            reid_enabled: v.reid_enabled,
            reid_model_id: v.reid_model_id,
            designate_camera: v.designate_camera,
            detector: v.detector,
            reid: v.reid,
        }
    }

    /// The slot count to size camera rings with, clamped to the range the ring
    /// header can represent. The header records `slot_count` in two bytes, so a
    /// configured value above that maximum would truncate and make the writer's
    /// `seq % slot_count` math diverge from a header-deriving reader; clamping
    /// here keeps the writer and every consumer in agreement. A value below 2 is
    /// raised to 2 (the engine needs at least two slots to recycle without
    /// every read racing the single live frame).
    ///
    /// This is only the header-representability bound. What the depth costs in
    /// RAM depends on the camera's frame size, which is not known until a frame
    /// arrives, so the memory bound is [`Self::effective_shm_budget_bytes`] and
    /// the engine reduces the depth to fit it.
    pub fn effective_slot_count(&self) -> u32 {
        self.slot_count.clamp(
            crate::ring::MIN_SLOT_COUNT,
            ados_protocol::framebus::MAX_SLOT_COUNT,
        )
    }

    /// The per-camera ring byte budget, floored so a zero or absurdly small
    /// configured value cannot leave the engine unable to open a ring at all.
    /// One 1080p rgb24 frame is ~6 MB, so the floor holds two of them.
    pub fn effective_shm_budget_bytes(&self) -> usize {
        const FLOOR_MB: u32 = 16;
        self.shm_budget_mb.max(FLOOR_MB) as usize * 1024 * 1024
    }

    /// The `vision.sock` path under the configured socket directory.
    pub fn vision_socket_path(&self) -> String {
        format!("{}/vision.sock", self.socket_dir.trim_end_matches('/'))
    }

    /// The `vision-detections.sock` path: a last-state broadcast socket the
    /// API process subscribes to so the browser can receive live detection
    /// batches over a WebSocket. Distinct from `vision.sock` (the request /
    /// response plugin bridge) — this one is broadcast-only and carries
    /// length-prefixed msgpack [`ados_protocol::framebus::DetectionBatch`].
    pub fn detections_socket_path(&self) -> String {
        format!(
            "{}/vision-detections.sock",
            self.socket_dir.trim_end_matches('/')
        )
    }

    /// The `vision-frames.sock` path: a last-state broadcast socket that
    /// re-publishes every frame descriptor so an on-box service (the world-model
    /// capture service) can subscribe and map the ring the descriptor names.
    /// Carries length-prefixed msgpack [`ados_protocol::framebus::FrameDescriptor`].
    pub fn frames_socket_path(&self) -> String {
        format!(
            "{}/vision-frames.sock",
            self.socket_dir.trim_end_matches('/')
        )
    }

    /// The accelerator sidecar socket path the RKNN backend talks to.
    pub fn rknn_socket_path(&self) -> String {
        format!("{}/vision-rknn.sock", self.socket_dir.trim_end_matches('/'))
    }

    /// The accelerator sidecar socket path the Hailo backend talks to (Pi + AI HAT).
    pub fn hailo_socket_path(&self) -> String {
        format!(
            "{}/vision-hailo.sock",
            self.socket_dir.trim_end_matches('/')
        )
    }

    /// Whether the engine has work: vision is enabled, or a camera is marked
    /// `capture: true` (the engine is its only owner, whatever
    /// `vision.enabled` says).
    pub fn should_run(&self) -> bool {
        self.enabled || !self.cameras.is_empty()
    }

    /// True when the resolved profile is the ground station (the engine exits).
    pub fn is_ground_station(&self) -> bool {
        matches!(
            self.profile.as_deref(),
            Some("ground_station") | Some("ground-station")
        )
    }
}

#[cfg(test)]
#[path = "config_tests.rs"]
mod tests;
