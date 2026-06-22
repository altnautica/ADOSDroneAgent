//! Vision engine configuration, read from the `vision:` block of
//! `/etc/ados/config.yaml`. Every field is `#[serde(default)]` so a partial or
//! malformed config never blocks startup — a missing `vision:` block yields a
//! disabled engine, which is the safe default on a rig that does not run vision.

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

/// One explicitly-configured camera. When the `cameras` list is empty the
/// engine falls back to HAL discovery (`python -m ados.hal.camera --json`).
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct CameraEntry {
    /// Stable camera id the engine labels frames and rings with.
    pub id: String,
    /// How the engine gets frames for this camera: "tap" reads the video
    /// pipeline tap socket; "capture" spawns a direct V4L2/CSI capture.
    #[serde(default = "default_camera_kind")]
    pub kind: String,
    /// For "tap": the unix-socket path the video pipeline writes raw frames to.
    /// Defaults to `<socket_dir>/vision-tap-<id>.sock` when absent.
    #[serde(default)]
    pub tap_socket: Option<String>,
    /// For "capture": the device path (e.g. `/dev/video0`). Absent ⇒ resolved
    /// by HAL discovery.
    #[serde(default)]
    pub device_path: Option<String>,
}

fn default_camera_kind() -> String {
    "tap".to_string()
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
    /// Explicit camera list. Empty ⇒ HAL discovery picks the engine cameras.
    #[serde(default)]
    pub cameras: Vec<CameraEntry>,
    /// Width frames are downscaled to before publishing.
    #[serde(default = "default_downscale_width")]
    pub downscale_width: u32,
    /// Height frames are downscaled to before publishing.
    #[serde(default = "default_downscale_height")]
    pub downscale_height: u32,
    /// Slots per camera ring (latest-wins recycling depth).
    #[serde(default = "default_slot_count")]
    pub slot_count: u32,
    /// Backend preference: "auto" (pick by SoC) | "mock" | "onnx" | "rknn".
    #[serde(default = "default_backend")]
    pub backend: String,
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
}

impl Default for VisionConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            profile: None,
            socket_dir: default_socket_dir(),
            cameras: Vec::new(),
            downscale_width: default_downscale_width(),
            downscale_height: default_downscale_height(),
            slot_count: default_slot_count(),
            backend: default_backend(),
            tracker_enabled: false,
            reid_enabled: false,
            reid_model_id: None,
            designate_camera: None,
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
            #[serde(default)]
            cameras: Vec<CameraEntry>,
            #[serde(default = "default_downscale_width")]
            downscale_width: u32,
            #[serde(default = "default_downscale_height")]
            downscale_height: u32,
            #[serde(default = "default_slot_count")]
            slot_count: u32,
            #[serde(default = "default_backend")]
            backend: String,
            #[serde(default)]
            tracker_enabled: bool,
            #[serde(default)]
            reid_enabled: bool,
            #[serde(default)]
            reid_model_id: Option<String>,
            #[serde(default)]
            designate_camera: Option<String>,
        }

        let Ok(text) = std::fs::read_to_string(path) else {
            return VisionConfig::default();
        };
        let raw: RawConfig = serde_norway::from_str(&text).unwrap_or_default();
        let Some(v) = raw.vision else {
            // No `vision:` block ⇒ disabled, but still carry the agent profile
            // so the ground-station gate is consistent.
            return VisionConfig {
                profile: raw.agent.profile,
                ..VisionConfig::default()
            };
        };
        VisionConfig {
            enabled: v.enabled,
            // `agent.profile` is canonical; a `vision.profile` override pins it.
            profile: v.profile.or(raw.agent.profile),
            socket_dir: v.socket_dir,
            cameras: v.cameras,
            downscale_width: v.downscale_width,
            downscale_height: v.downscale_height,
            slot_count: v.slot_count,
            backend: v.backend,
            tracker_enabled: v.tracker_enabled,
            reid_enabled: v.reid_enabled,
            reid_model_id: v.reid_model_id,
            designate_camera: v.designate_camera,
        }
    }

    /// The slot count to size camera rings with, clamped to the range the ring
    /// header can represent. The header records `slot_count` in two bytes, so a
    /// configured value above that maximum would truncate and make the writer's
    /// `seq % slot_count` math diverge from a header-deriving reader; clamping
    /// here keeps the writer and every consumer in agreement. A value below 2 is
    /// raised to 2 (the engine needs at least two slots to recycle without
    /// every read racing the single live frame).
    pub fn effective_slot_count(&self) -> u32 {
        self.slot_count
            .clamp(2, ados_protocol::framebus::MAX_SLOT_COUNT)
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

    /// The accelerator sidecar socket path the RKNN backend talks to.
    pub fn rknn_socket_path(&self) -> String {
        format!("{}/vision-rknn.sock", self.socket_dir.trim_end_matches('/'))
    }

    /// The default tap socket path for a camera id.
    pub fn tap_socket_for(&self, camera_id: &str) -> String {
        format!(
            "{}/vision-tap-{}.sock",
            self.socket_dir.trim_end_matches('/'),
            camera_id
        )
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
mod tests {
    use super::*;

    fn write_tmp(yaml: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.yaml");
        std::fs::write(&path, yaml).unwrap();
        (dir, path)
    }

    #[test]
    fn missing_file_is_disabled_default() {
        let c = VisionConfig::load_from(std::path::Path::new("/nonexistent/config.yaml"));
        assert!(!c.enabled);
        assert!(c.profile.is_none());
        assert_eq!(c.downscale_width, 640);
        assert_eq!(c.downscale_height, 480);
        assert_eq!(c.slot_count, 4);
        assert_eq!(c.backend, "auto");
        assert_eq!(c.vision_socket_path(), "/run/ados/vision.sock");
    }

    #[test]
    fn no_vision_block_keeps_profile_but_disabled() {
        let yaml = "agent:\n  profile: drone\n";
        let (_d, p) = write_tmp(yaml);
        let c = VisionConfig::load_from(&p);
        assert!(!c.enabled);
        assert_eq!(c.profile.as_deref(), Some("drone"));
        assert!(!c.is_ground_station());
    }

    #[test]
    fn full_vision_block_loads() {
        let yaml = "\
agent:
  profile: drone
vision:
  enabled: true
  socket_dir: /tmp/run
  downscale_width: 1280
  downscale_height: 720
  slot_count: 6
  backend: rknn
  cameras:
    - id: uvc-0
      kind: capture
      device_path: /dev/video0
    - id: fpv
      kind: tap
";
        let (_d, p) = write_tmp(yaml);
        let c = VisionConfig::load_from(&p);
        assert!(c.enabled);
        assert_eq!(c.profile.as_deref(), Some("drone"));
        assert_eq!(c.socket_dir, "/tmp/run");
        assert_eq!(c.downscale_width, 1280);
        assert_eq!(c.slot_count, 6);
        assert_eq!(c.backend, "rknn");
        assert_eq!(c.cameras.len(), 2);
        assert_eq!(c.cameras[0].id, "uvc-0");
        assert_eq!(c.cameras[0].kind, "capture");
        assert_eq!(c.cameras[0].device_path.as_deref(), Some("/dev/video0"));
        assert_eq!(c.cameras[1].kind, "tap");
        assert_eq!(c.vision_socket_path(), "/tmp/run/vision.sock");
        assert_eq!(c.rknn_socket_path(), "/tmp/run/vision-rknn.sock");
        assert_eq!(c.tap_socket_for("fpv"), "/tmp/run/vision-tap-fpv.sock");
        assert_eq!(
            c.detections_socket_path(),
            "/tmp/run/vision-detections.sock"
        );
    }

    #[test]
    fn tracker_and_reid_default_off_and_parse() {
        // A config with no tracker/reid keys leaves both off and the model ids
        // unset — the engine publishes raw detections exactly as before.
        let c = VisionConfig::default();
        assert!(!c.tracker_enabled);
        assert!(!c.reid_enabled);
        assert!(c.reid_model_id.is_none());
        assert!(c.designate_camera.is_none());

        let yaml = "\
vision:
  enabled: true
  tracker_enabled: true
  reid_enabled: true
  reid_model_id: com.example.reid-osnet
  designate_camera: uvc-0
";
        let (_d, p) = write_tmp(yaml);
        let c = VisionConfig::load_from(&p);
        assert!(c.tracker_enabled);
        assert!(c.reid_enabled);
        assert_eq!(c.reid_model_id.as_deref(), Some("com.example.reid-osnet"));
        assert_eq!(c.designate_camera.as_deref(), Some("uvc-0"));

        // A vision block that omits the new keys keeps them at the safe default.
        let yaml2 = "vision:\n  enabled: true\n";
        let (_d2, p2) = write_tmp(yaml2);
        let c2 = VisionConfig::load_from(&p2);
        assert!(c2.enabled);
        assert!(!c2.tracker_enabled);
        assert!(!c2.reid_enabled);
    }

    #[test]
    fn ground_station_gate() {
        let yaml = "agent:\n  profile: ground_station\nvision:\n  enabled: true\n";
        let (_d, p) = write_tmp(yaml);
        assert!(VisionConfig::load_from(&p).is_ground_station());
        let yaml2 = "agent:\n  profile: ground-station\nvision:\n  enabled: true\n";
        let (_d2, p2) = write_tmp(yaml2);
        assert!(VisionConfig::load_from(&p2).is_ground_station());
    }

    #[test]
    fn vision_profile_overrides_agent_profile() {
        let yaml = "\
agent:
  profile: drone
vision:
  enabled: true
  profile: ground_station
";
        let (_d, p) = write_tmp(yaml);
        assert!(VisionConfig::load_from(&p).is_ground_station());
    }

    #[test]
    fn malformed_yaml_falls_back_to_default() {
        let yaml = "this: : not [valid yaml }}}";
        let (_d, p) = write_tmp(yaml);
        let c = VisionConfig::load_from(&p);
        assert!(!c.enabled);
    }

    #[test]
    fn effective_slot_count_clamps_to_the_header_range() {
        // The default is in range and unchanged.
        let c = VisionConfig::default();
        assert_eq!(c.effective_slot_count(), default_slot_count());

        // A misconfigured huge depth clamps to the header maximum instead of
        // silently truncating the writer/reader slot math.
        let huge = VisionConfig {
            slot_count: 100_000,
            ..VisionConfig::default()
        };
        assert_eq!(
            huge.effective_slot_count(),
            ados_protocol::framebus::MAX_SLOT_COUNT
        );

        // A too-small depth is raised to the two-slot floor.
        let tiny = VisionConfig {
            slot_count: 0,
            ..VisionConfig::default()
        };
        assert_eq!(tiny.effective_slot_count(), 2);
    }
}
