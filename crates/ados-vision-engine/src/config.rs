//! Vision engine configuration, read from the `vision:` block of
//! `/etc/ados/config.yaml`, plus the cameras the engine owns (the top-level
//! `cameras[]` entries marked `capture: true`), the video pipeline's frame tap
//! (`video.vision`) and the simulation flag. Every field is `#[serde(default)]`
//! so a partial or malformed config never blocks startup — a missing `vision:`
//! block yields a disabled engine, which is the safe default on a rig that
//! does not run vision.

use ados_config::cameras::{camera_entries, CaptureCamera, InvalidCamera};
use serde::Deserialize;

use crate::engine::{DetectionDemandMode, DetectionPolicy};

mod models;
use models::default_threads;
pub use models::{
    DepthConfig, DepthPlan, DetectorConfig, ReidConfig, DEFAULT_DEPTH_FILE_STEM,
    DEFAULT_DEPTH_MODEL_ID, DEPTH_RATE_ACCEL_HZ, DEPTH_RATE_CPU_HZ,
};

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

fn default_designate_hold_s() -> f32 {
    30.0
}

fn default_max_box_area_frac() -> f32 {
    0.5
}

fn default_models_dir() -> String {
    "/opt/ados/models/vision".to_string()
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
    /// Ids of the valid `cameras[]` entries the engine does not own (not
    /// `capture: true`, or disabled): plugins may not change their controls.
    #[serde(skip)]
    pub other_camera_ids: Vec<String>,
    /// The primary camera (the `cameras[]` entry with role `primary`, else the
    /// first) is one this engine owns: the video pipeline encodes it from the
    /// frame bus, so its tap carries this engine's own frames back.
    #[serde(skip)]
    pub primary_captured: bool,
    /// The video pipeline's raw frame tap of the primary stream
    /// (`video.vision.sink`) when `video.vision.enabled`; the engine reads it
    /// as camera `main`.
    #[serde(skip)]
    pub tap_sink: Option<String>,
    /// `simulation.enabled`: frames carry simulator time, which the engine
    /// reports to the clock.
    #[serde(skip)]
    pub simulation: bool,
    /// `cameras[]` entries that do not validate: skipped by every consumer,
    /// reported in the status sidecar's `source_errors`.
    #[serde(skip)]
    pub invalid_cameras: Vec<InvalidCamera>,
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
    /// Build the tracker with appearance association so identity survives a
    /// crossing. Requires `tracker_enabled`; uses the `reid` model when it is
    /// loaded, else the colour histogram. Default off: motion-only.
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
    /// A detector the engine auto-loads and runs on the cameras detection is
    /// wanted on, to produce the detection stream. Absent ⇒ no engine-driven
    /// detections.
    #[serde(default)]
    pub detector: Option<DetectorConfig>,
    /// The appearance (re-id) model the engine loads whenever it is set. Absent
    /// ⇒ embeddings come from the colour histogram descriptor.
    #[serde(default)]
    pub reid: Option<ReidConfig>,
    /// When the detector runs: `on_demand` (default) only on a camera some
    /// consumer holds a lease on; `always` on every captured camera.
    #[serde(default)]
    pub detection_demand: DetectionDemandMode,
    /// Seconds of detection an operator designation holds on its camera.
    #[serde(default = "default_designate_hold_s")]
    pub designate_hold_s: f32,
    /// ffmpeg threads each RTSP decoder runs on. Default 1.
    #[serde(default = "default_threads")]
    pub decode_threads: u32,
    /// Detections whose box covers more than this fraction of the frame are
    /// dropped before tracking (a person that close is under a metre from the
    /// lens, never a follow target). Default 0.5; `1` keeps every box.
    #[serde(default = "default_max_box_area_frac")]
    pub max_box_area_frac: f32,
    /// The depth model behind the depth and surfaces layers.
    #[serde(default)]
    pub depth: DepthConfig,
    /// Where installed models live (the registry downloads here).
    #[serde(default = "default_models_dir")]
    pub models_dir: String,
    /// Each camera's mount and field of view, from `cameras[]`.
    #[serde(skip)]
    pub camera_geometry: std::collections::HashMap<String, crate::depth::geometry::CameraGeometry>,
}

impl Default for VisionConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            profile: None,
            socket_dir: default_socket_dir(),
            cameras: Vec::new(),
            other_camera_ids: Vec::new(),
            primary_captured: false,
            tap_sink: None,
            simulation: false,
            invalid_cameras: Vec::new(),
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
            detection_demand: DetectionDemandMode::default(),
            designate_hold_s: default_designate_hold_s(),
            decode_threads: default_threads(),
            max_box_area_frac: default_max_box_area_frac(),
            depth: DepthConfig::default(),
            models_dir: default_models_dir(),
            camera_geometry: std::collections::HashMap::new(),
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
            #[serde(default)]
            detection_demand: DetectionDemandMode,
            #[serde(default = "default_designate_hold_s")]
            designate_hold_s: f32,
            #[serde(default = "default_threads")]
            decode_threads: u32,
            #[serde(default = "default_max_box_area_frac")]
            max_box_area_frac: f32,
            #[serde(default)]
            depth: DepthConfig,
            #[serde(default = "default_models_dir")]
            models_dir: String,
        }

        let Ok(text) = std::fs::read_to_string(path) else {
            return VisionConfig::default();
        };
        let raw: RawConfig = ados_config::yaml_or_default(&text, "vision");
        let entries = camera_entries(&text);
        let primary_captured = entries
            .valid
            .iter()
            .map(|(_, _, c)| c)
            .find(|c| c.role.as_deref() == Some("primary"))
            .or_else(|| entries.valid.first().map(|(_, _, c)| c))
            .is_some_and(|c| c.capture && c.enabled);
        let (cameras, others): (Vec<CaptureCamera>, Vec<CaptureCamera>) = entries
            .valid
            .into_iter()
            .map(|(_, _, c)| c)
            .partition(|c| c.capture && c.enabled);
        let other_camera_ids: Vec<String> = others.into_iter().map(|c| c.id).collect();
        let invalid_cameras = entries.invalid;
        let camera_geometry = crate::depth::geometry::geometries_from_config(&text);
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
                other_camera_ids,
                primary_captured,
                tap_sink,
                simulation,
                invalid_cameras,
                camera_geometry,
                ..VisionConfig::default()
            };
        };
        VisionConfig {
            enabled: v.enabled,
            // `agent.profile` is canonical; a `vision.profile` override pins it.
            profile: v.profile.or(raw.agent.profile),
            socket_dir: v.socket_dir,
            cameras,
            other_camera_ids,
            primary_captured,
            tap_sink,
            simulation,
            invalid_cameras,
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
            detection_demand: v.detection_demand,
            designate_hold_s: v.designate_hold_s,
            decode_threads: v.decode_threads,
            max_box_area_frac: v.max_box_area_frac,
            depth: v.depth,
            models_dir: v.models_dir,
            camera_geometry,
        }
    }

    /// The engine's detection policy from the demand keys. A negative or
    /// non-finite designation hold grants nothing; a non-finite box cutoff
    /// keeps every box.
    pub fn detection_policy(&self) -> DetectionPolicy {
        let hold_s = if self.designate_hold_s.is_finite() {
            self.designate_hold_s.max(0.0)
        } else {
            0.0
        };
        DetectionPolicy {
            mode: self.detection_demand,
            designate_hold_ms: (hold_s as f64 * 1000.0).round() as u64,
            max_box_area_frac: if self.max_box_area_frac.is_finite() {
                self.max_box_area_frac
            } else {
                1.0
            },
        }
    }

    /// The detector's inference threads (at least one), one when no detector
    /// is configured.
    pub fn detector_threads(&self) -> u32 {
        self.detector.as_ref().map_or(1, |d| d.threads.max(1))
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

    /// The `vision-depth.sock` path: the broadcast of every depth grid and
    /// landing-zone frame ([`ados_protocol::vision_depth::DepthRecord`]).
    pub fn depth_socket_path(&self) -> String {
        format!(
            "{}/vision-depth.sock",
            self.socket_dir.trim_end_matches('/')
        )
    }

    /// The MAVLink broadcast socket the depth fit reads attitude and the
    /// downward rangefinder from.
    pub fn mavlink_socket_path(&self) -> String {
        format!(
            "{}/{}",
            self.socket_dir.trim_end_matches('/'),
            ados_protocol::paths::MAVLINK_SOCK
        )
    }

    /// The depth model's ONNX intra-op threads (at least one).
    pub fn depth_threads(&self) -> u32 {
        self.depth.threads.max(1)
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
