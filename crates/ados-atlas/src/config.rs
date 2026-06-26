//! Capture configuration: which cameras the rig carries, the flight profile,
//! and the keyframe-selection thresholds.
//!
//! Camera count is configurable from one camera up to an all-sides rig; the
//! enabled set drives one flow at any count (the fusion layer keys off the
//! enabled count, it never forks on it).

use crate::AtlasError;
use ados_protocol::atlas::CameraRole;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

/// One camera on the rig. `enabled` gates whether its frames are ingested at
/// all; `reconstruct` is the per-camera hint to the compute node about whether
/// this stream feeds the world-model reconstruction (a camera may be captured
/// for situational video yet excluded from the splat).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CameraConfig {
    pub id: String,
    pub role: CameraRole,
    pub enabled: bool,
    pub reconstruct: bool,
}

/// The flight pattern a capture session is flown in. The profile is metadata
/// for the compute node's reconstructor and for the operator UI; it does not
/// change the selection math, only the operator's intent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CaptureProfile {
    /// Circling a single subject (object / asset capture).
    Orbit,
    /// Back-and-forth survey rows (area / corridor mapping).
    Lawnmower,
    /// No fixed pattern; the operator flies freely.
    #[default]
    Freeform,
    /// Close-in structure inspection (pipeline / tower / facade).
    Inspection,
}

/// Thresholds the [`crate::KeyframeSelector`] uses to decide when the camera has
/// moved enough to be worth a new keyframe. A frame is selected when it crosses
/// ANY one of these (translation, rotation, or elapsed time).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct SelectionParams {
    /// Minimum baseline (metres) from the last keyframe's position.
    pub min_translation_m: f64,
    /// Minimum viewing-angle change (radians) from the last keyframe.
    pub min_rotation_rad: f64,
    /// Maximum gap (milliseconds) between keyframes even while stationary, so a
    /// hovering drone still lays down a heartbeat keyframe.
    pub max_interval_ms: i64,
}

impl Default for SelectionParams {
    fn default() -> Self {
        Self {
            min_translation_m: 0.5,
            // ~15 degrees.
            min_rotation_rad: 0.26,
            max_interval_ms: 2000,
        }
    }
}

/// The full capture configuration for a session.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct CaptureConfig {
    pub cameras: Vec<CameraConfig>,
    pub profile: CaptureProfile,
    pub selection: SelectionParams,
}

impl CaptureConfig {
    /// Iterate the enabled cameras only.
    pub fn enabled_cameras(&self) -> impl Iterator<Item = &CameraConfig> + '_ {
        self.cameras.iter().filter(|c| c.enabled)
    }

    /// Count the enabled cameras (1 to N). The fusion layer keys off this.
    pub fn enabled_camera_count(&self) -> u32 {
        self.enabled_cameras().count() as u32
    }

    /// Whether a given camera id is present AND enabled.
    pub fn is_enabled(&self, camera_id: &str) -> bool {
        self.cameras.iter().any(|c| c.id == camera_id && c.enabled)
    }

    /// Validate the config before a session runs. A capture with no enabled
    /// camera produces nothing, and duplicate camera ids would collapse two
    /// physical streams onto one per-camera selector, so both are rejected.
    pub fn validate(&self) -> Result<(), AtlasError> {
        if self.enabled_camera_count() == 0 {
            return Err(AtlasError::NoEnabledCameras);
        }
        let mut seen = HashSet::new();
        for cam in &self.cameras {
            if !seen.insert(cam.id.as_str()) {
                return Err(AtlasError::DuplicateCameraId(cam.id.clone()));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cam(id: &str, enabled: bool) -> CameraConfig {
        CameraConfig {
            id: id.into(),
            role: CameraRole::Primary,
            enabled,
            reconstruct: enabled,
        }
    }

    #[test]
    fn single_camera_config_has_one_enabled() {
        let cfg = CaptureConfig {
            cameras: vec![cam("front", true)],
            ..Default::default()
        };
        assert_eq!(cfg.enabled_camera_count(), 1);
        assert!(cfg.is_enabled("front"));
        assert!(!cfg.is_enabled("missing"));
        assert_eq!(cfg.profile, CaptureProfile::Freeform);
    }

    #[test]
    fn multi_camera_config_counts_only_enabled() {
        let cfg = CaptureConfig {
            cameras: vec![
                cam("front", true),
                cam("down", false),
                cam("left", true),
                cam("right", false),
            ],
            profile: CaptureProfile::Orbit,
            ..Default::default()
        };
        assert_eq!(cfg.enabled_camera_count(), 2);
        assert!(cfg.is_enabled("front"));
        assert!(!cfg.is_enabled("down"));
        let enabled: Vec<&str> = cfg.enabled_cameras().map(|c| c.id.as_str()).collect();
        assert_eq!(enabled, vec!["front", "left"]);
    }

    #[test]
    fn default_selection_params_are_sane() {
        let p = SelectionParams::default();
        assert!((p.min_translation_m - 0.5).abs() < 1e-12);
        assert!((p.min_rotation_rad - 0.26).abs() < 1e-12);
        assert_eq!(p.max_interval_ms, 2000);
    }

    #[test]
    fn validate_rejects_no_enabled_cameras() {
        let cfg = CaptureConfig {
            cameras: vec![cam("front", false)],
            ..Default::default()
        };
        assert_eq!(cfg.validate(), Err(AtlasError::NoEnabledCameras));
    }

    #[test]
    fn validate_rejects_duplicate_camera_id() {
        let cfg = CaptureConfig {
            cameras: vec![cam("front", true), cam("front", true)],
            ..Default::default()
        };
        assert_eq!(
            cfg.validate(),
            Err(AtlasError::DuplicateCameraId("front".into()))
        );
    }

    #[test]
    fn validate_accepts_a_clean_config() {
        let cfg = CaptureConfig {
            cameras: vec![cam("front", true), cam("down", false)],
            ..Default::default()
        };
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn profile_serializes_snake_case() {
        let json = serde_json::to_string(&CaptureProfile::Lawnmower).unwrap();
        assert_eq!(json, "\"lawnmower\"");
        let back: CaptureProfile = serde_json::from_str("\"inspection\"").unwrap();
        assert_eq!(back, CaptureProfile::Inspection);
    }
}
