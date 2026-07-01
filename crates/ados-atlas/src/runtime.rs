//! The capture service's runtime configuration: the `atlas:` block of
//! `/etc/ados/config.yaml`, the pose-source tier selector, the camera-intrinsics
//! resolution, and the socket paths the daemon binds and connects to.
//!
//! The capture *core* ([`crate::CaptureConfig`]) declares the rig and the
//! selection thresholds. This runtime layer wraps it with the service-only
//! settings: the enable gate, the socket directory, the pose tier, the field of
//! view used to derive a default pinhole when a camera is uncalibrated, and any
//! per-camera intrinsics override.

use std::collections::HashMap;
use std::path::Path;

use ados_protocol::atlas::{CameraIntrinsics, Distortion};
use serde::Deserialize;

use crate::config::{CameraConfig, CaptureConfig, CaptureProfile, SelectionParams};

/// Canonical agent config file the daemon reads.
pub const CONFIG_YAML: &str = "/etc/ados/config.yaml";

fn default_socket_dir() -> String {
    "/run/ados".to_string()
}
fn default_hfov_deg() -> f64 {
    70.0
}

/// The configured pose-source preference. `Auto` lets the service pick (see
/// [`select_pose_tier`]); the explicit variants pin the choice.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum PoseTierConfig {
    #[default]
    Auto,
    Local,
    Offload,
    Hybrid,
}

/// The resolved pose-source tier the daemon runs with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PoseTier {
    /// On-board pose from the flight controller's fused state (the state socket).
    Local,
    /// Pose returned by a compute node running SLAM on streamed frames.
    Offload,
    /// Local as the primary control-rate pose, corrected by the offloaded pose
    /// when one is fresher.
    Hybrid,
}

/// Resolve the configured preference into a concrete tier.
///
/// The flight controller's fused pose (read from the state socket) is always
/// available, so `Local` is the floor. `Auto` prefers offloaded SLAM only when a
/// compute node is paired AND this board lacks a local accelerator to run good
/// perception itself; otherwise it stays local.
pub fn select_pose_tier(cfg: PoseTierConfig, npu_present: bool, compute_paired: bool) -> PoseTier {
    match cfg {
        PoseTierConfig::Local => PoseTier::Local,
        PoseTierConfig::Offload => PoseTier::Offload,
        PoseTierConfig::Hybrid => PoseTier::Hybrid,
        PoseTierConfig::Auto => {
            if compute_paired && !npu_present {
                PoseTier::Offload
            } else {
                PoseTier::Local
            }
        }
    }
}

/// A per-camera intrinsics override. When a camera has been calibrated the
/// operator supplies these so reconstruction is metric; absent, the service
/// derives an uncalibrated pinhole from the frame size and the field of view.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct IntrinsicsOverride {
    pub fx: f64,
    pub fy: f64,
    pub cx: f64,
    pub cy: f64,
    #[serde(default)]
    pub distortion_model: Option<String>,
    #[serde(default)]
    pub distortion_params: Vec<f64>,
}

impl IntrinsicsOverride {
    fn to_intrinsics(&self) -> CameraIntrinsics {
        CameraIntrinsics {
            k: [self.fx, 0.0, self.cx, 0.0, self.fy, self.cy, 0.0, 0.0, 1.0],
            distortion: Distortion {
                model: self
                    .distortion_model
                    .clone()
                    .unwrap_or_else(|| "radtan".to_string()),
                params: if self.distortion_params.is_empty() {
                    vec![0.0, 0.0, 0.0, 0.0]
                } else {
                    self.distortion_params.clone()
                },
            },
        }
    }
}

/// Derive an uncalibrated pinhole from the frame size and horizontal field of
/// view: `fx = fy = (width/2) / tan(hfov/2)`, principal point at the centre, no
/// distortion. The compute node treats these as an initial guess to refine.
pub fn default_intrinsics(width: u32, height: u32, hfov_deg: f64) -> CameraIntrinsics {
    let w = width.max(1) as f64;
    let h = height.max(1) as f64;
    let hfov = hfov_deg.clamp(1.0, 179.0).to_radians();
    let fx = (w / 2.0) / (hfov / 2.0).tan();
    CameraIntrinsics {
        k: [fx, 0.0, w / 2.0, 0.0, fx, h / 2.0, 0.0, 0.0, 1.0],
        distortion: Distortion {
            model: "radtan".to_string(),
            params: vec![0.0, 0.0, 0.0, 0.0],
        },
    }
}

/// The capture service's full runtime configuration.
#[derive(Debug, Clone, PartialEq)]
pub struct AtlasRuntimeConfig {
    pub enabled: bool,
    pub profile: Option<String>,
    /// The drone's device id (`agent.device_id`), used to mint a globally-unique
    /// capture `session_id` so two drones on one shared compute node never
    /// collide (empty when absent — the session id then falls back to a nonce).
    pub device_id: String,
    pub socket_dir: String,
    pub capture: CaptureConfig,
    pub pose_tier: PoseTierConfig,
    pub hfov_deg: f64,
    pub intrinsics: HashMap<String, IntrinsicsOverride>,
}

impl Default for AtlasRuntimeConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            profile: None,
            device_id: String::new(),
            socket_dir: default_socket_dir(),
            capture: CaptureConfig::default(),
            pose_tier: PoseTierConfig::Auto,
            hfov_deg: default_hfov_deg(),
            intrinsics: HashMap::new(),
        }
    }
}

impl AtlasRuntimeConfig {
    /// Load from `/etc/ados/config.yaml`. Returns defaults (disabled) when the
    /// file is missing or unparseable so startup never blocks on config.
    pub fn load_from(path: &Path) -> Self {
        #[derive(Debug, Default, Deserialize)]
        struct RawConfig {
            #[serde(default)]
            agent: AgentSection,
            #[serde(default)]
            atlas: Option<AtlasSection>,
        }
        #[derive(Debug, Default, Deserialize)]
        struct AgentSection {
            #[serde(default)]
            profile: Option<String>,
            #[serde(default)]
            device_id: String,
        }
        #[derive(Debug, Deserialize)]
        struct AtlasSection {
            #[serde(default)]
            enabled: bool,
            #[serde(default = "default_socket_dir")]
            socket_dir: String,
            #[serde(default)]
            cameras: Vec<CameraConfig>,
            #[serde(default)]
            capture_profile: CaptureProfile,
            #[serde(default)]
            selection: SelectionParams,
            #[serde(default)]
            pose_tier: PoseTierConfig,
            #[serde(default = "default_hfov_deg")]
            hfov_deg: f64,
            #[serde(default)]
            intrinsics: HashMap<String, IntrinsicsOverride>,
        }

        let Ok(text) = std::fs::read_to_string(path) else {
            return AtlasRuntimeConfig::default();
        };
        // A parse error must be LOUD, never a silent default-to-disabled: one bad
        // enum (e.g. an unknown camera role) would otherwise swallow the whole
        // block and leave a status surface reporting `enabled: false` with no
        // reason. Log the exact serde error (it names the offending field) so the
        // operator can see why atlas is off, then fall back to defaults.
        let raw: RawConfig = match serde_norway::from_str(&text) {
            Ok(c) => c,
            Err(e) => {
                tracing::error!(
                    error = %e,
                    "atlas config parse failed; atlas stays disabled until the config is valid"
                );
                RawConfig::default()
            }
        };
        let Some(a) = raw.atlas else {
            return AtlasRuntimeConfig {
                profile: raw.agent.profile,
                device_id: raw.agent.device_id,
                ..AtlasRuntimeConfig::default()
            };
        };
        AtlasRuntimeConfig {
            enabled: a.enabled,
            // `agent.profile` is the single canonical source (the capture service
            // is air-side; there is no atlas-specific profile override).
            profile: raw.agent.profile,
            device_id: raw.agent.device_id,
            socket_dir: a.socket_dir,
            capture: CaptureConfig {
                cameras: a.cameras,
                profile: a.capture_profile,
                selection: a.selection,
            },
            pose_tier: a.pose_tier,
            hfov_deg: a.hfov_deg,
            intrinsics: a.intrinsics,
        }
    }

    /// Intrinsics for a camera: the configured override if present, else an
    /// uncalibrated pinhole derived from the frame size and the field of view.
    pub fn intrinsics_for(&self, camera_id: &str, width: u32, height: u32) -> CameraIntrinsics {
        match self.intrinsics.get(camera_id) {
            Some(o) => o.to_intrinsics(),
            None => default_intrinsics(width, height, self.hfov_deg),
        }
    }

    /// The frame-descriptor source socket the vision engine broadcasts on.
    pub fn frames_socket_path(&self) -> String {
        format!(
            "{}/vision-frames.sock",
            self.socket_dir.trim_end_matches('/')
        )
    }

    /// The flight-controller state socket the local-VIO pose is read from.
    pub fn state_socket_path(&self) -> String {
        format!("{}/state.sock", self.socket_dir.trim_end_matches('/'))
    }

    /// The socket a compute node publishes offloaded SLAM poses on.
    pub fn offload_socket_path(&self) -> String {
        format!(
            "{}/atlas-pose-offload.sock",
            self.socket_dir.trim_end_matches('/')
        )
    }

    /// The atlas bus the capture service publishes keyframes, poses, and capture
    /// state on (one socket, every message tagged by topic).
    pub fn atlas_socket_path(&self) -> String {
        format!("{}/atlas.sock", self.socket_dir.trim_end_matches('/'))
    }

    /// The inbound control socket the capture service accepts start/stop/pause/
    /// resume/status commands on (the GCS drives capture through the front, which
    /// forwards to this socket). Resolved under the same `socket_dir` as the
    /// atlas bus so both halves agree on `/run/ados` by default.
    pub fn control_socket_path(&self) -> String {
        format!(
            "{}/atlas-control.sock",
            self.socket_dir.trim_end_matches('/')
        )
    }

    /// True when the resolved profile is the ground station (the service exits:
    /// the air side owns the cameras).
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
        let c = AtlasRuntimeConfig::load_from(Path::new("/nonexistent/config.yaml"));
        assert!(!c.enabled);
        assert!(c.profile.is_none());
        assert_eq!(c.socket_dir, "/run/ados");
        assert_eq!(c.pose_tier, PoseTierConfig::Auto);
        assert_eq!(c.atlas_socket_path(), "/run/ados/atlas.sock");
        assert_eq!(c.control_socket_path(), "/run/ados/atlas-control.sock");
        assert_eq!(c.frames_socket_path(), "/run/ados/vision-frames.sock");
        assert_eq!(c.state_socket_path(), "/run/ados/state.sock");
    }

    #[test]
    fn no_atlas_block_keeps_profile_but_disabled() {
        let (_d, p) = write_tmp("agent:\n  profile: drone\n");
        let c = AtlasRuntimeConfig::load_from(&p);
        assert!(!c.enabled);
        assert_eq!(c.profile.as_deref(), Some("drone"));
        assert!(!c.is_ground_station());
    }

    #[test]
    fn device_id_loads_from_the_agent_block() {
        // The capture service reads `agent.device_id` to scope the session id.
        let (_d, p) =
            write_tmp("agent:\n  profile: drone\n  device_id: drone-42\natlas:\n  enabled: true\n");
        assert_eq!(AtlasRuntimeConfig::load_from(&p).device_id, "drone-42");
        // Absent → empty (the session id then uses a nonce, never a bare ms).
        let (_d2, p2) = write_tmp("atlas:\n  enabled: true\n");
        assert_eq!(AtlasRuntimeConfig::load_from(&p2).device_id, "");
    }

    #[test]
    fn full_atlas_block_loads() {
        let yaml = "\
agent:
  profile: drone
atlas:
  enabled: true
  socket_dir: /tmp/run
  capture_profile: orbit
  pose_tier: hybrid
  hfov_deg: 90
  cameras:
    - id: front
      role: primary
      enabled: true
      reconstruct: true
    - id: down
      role: down
      enabled: false
      reconstruct: false
  selection:
    min_translation_m: 1.0
    min_rotation_rad: 0.3
    max_interval_ms: 1500
  intrinsics:
    front:
      fx: 900.0
      fy: 900.0
      cx: 640.0
      cy: 360.0
";
        let (_d, p) = write_tmp(yaml);
        let c = AtlasRuntimeConfig::load_from(&p);
        assert!(c.enabled);
        assert_eq!(c.profile.as_deref(), Some("drone"));
        assert_eq!(c.socket_dir, "/tmp/run");
        assert_eq!(c.pose_tier, PoseTierConfig::Hybrid);
        assert_eq!(c.capture.profile, CaptureProfile::Orbit);
        assert_eq!(c.capture.enabled_camera_count(), 1);
        assert!((c.capture.selection.min_translation_m - 1.0).abs() < 1e-9);
        assert_eq!(c.atlas_socket_path(), "/tmp/run/atlas.sock");
        assert_eq!(c.control_socket_path(), "/tmp/run/atlas-control.sock");
        // The configured intrinsics override is used for `front`.
        let k = c.intrinsics_for("front", 1280, 720);
        assert!((k.k[0] - 900.0).abs() < 1e-9);
        assert!((k.k[2] - 640.0).abs() < 1e-9);
    }

    #[test]
    fn ground_station_profile_is_detected() {
        let (_d, p) = write_tmp("agent:\n  profile: ground_station\natlas:\n  enabled: true\n");
        assert!(AtlasRuntimeConfig::load_from(&p).is_ground_station());
    }

    #[test]
    fn pose_tier_auto_prefers_offload_only_when_npu_less_and_paired() {
        // The always-available floor is Local.
        assert_eq!(
            select_pose_tier(PoseTierConfig::Auto, false, false),
            PoseTier::Local
        );
        // A paired node + no local accelerator → offload.
        assert_eq!(
            select_pose_tier(PoseTierConfig::Auto, false, true),
            PoseTier::Offload
        );
        // A local accelerator keeps it local even when a node is paired.
        assert_eq!(
            select_pose_tier(PoseTierConfig::Auto, true, true),
            PoseTier::Local
        );
        // Explicit config always wins.
        assert_eq!(
            select_pose_tier(PoseTierConfig::Local, false, true),
            PoseTier::Local
        );
        assert_eq!(
            select_pose_tier(PoseTierConfig::Offload, true, false),
            PoseTier::Offload
        );
    }

    #[test]
    fn derived_intrinsics_centre_the_principal_point() {
        let k = default_intrinsics(1280, 720, 70.0);
        assert!((k.k[2] - 640.0).abs() < 1e-9, "cx at centre");
        assert!((k.k[5] - 360.0).abs() < 1e-9, "cy at centre");
        assert!(k.k[0] > 0.0, "positive focal length");
        // fx == fy (square pixels) and bottom row is [0,0,1].
        assert!((k.k[0] - k.k[4]).abs() < 1e-9);
        assert_eq!(k.k[8], 1.0);
    }
}
