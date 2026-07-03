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

fn default_camera_role() -> CameraRole {
    CameraRole::Primary
}
fn default_true() -> bool {
    true
}

/// One camera on the rig. `enabled` gates whether its frames are ingested at
/// all; `reconstruct` is the per-camera hint to the compute node about whether
/// this stream feeds the world-model reconstruction (a camera may be captured
/// for situational video yet excluded from the splat).
///
/// Every field but `id` carries a serde default so a minimal config entry
/// (`- id: front`) deserializes — matching the Python `AtlasCameraConfig`
/// optionality so a config valid on one half is valid on the other.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CameraConfig {
    pub id: String,
    #[serde(default = "default_camera_role")]
    pub role: CameraRole,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_true")]
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

fn default_min_translation_m() -> f64 {
    0.5
}
/// ~15 degrees.
fn default_min_rotation_rad() -> f64 {
    0.26
}
fn default_max_interval_ms() -> i64 {
    2000
}
/// 0 = unlimited (the historical behaviour — no session-wide cap).
fn default_max_keyframes() -> u64 {
    0
}

/// Thresholds the [`crate::KeyframeSelector`] uses to decide when the camera has
/// moved enough to be worth a new keyframe. A frame is selected when it crosses
/// ANY one of these (translation, rotation, or elapsed time).
///
/// Each field carries a serde default (the same sane values as [`Default`]) so a
/// partial `selection:` block (the operator tunes only one threshold) keeps the
/// others sane instead of zeroing them — and so it matches the Python
/// `AtlasSelectionParams` optionality.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct SelectionParams {
    /// Minimum baseline (metres) from the last keyframe's position.
    #[serde(default = "default_min_translation_m")]
    pub min_translation_m: f64,
    /// Minimum viewing-angle change (radians) from the last keyframe.
    #[serde(default = "default_min_rotation_rad")]
    pub min_rotation_rad: f64,
    /// Maximum gap (milliseconds) between keyframes even while stationary, so a
    /// hovering drone still lays down a heartbeat keyframe.
    #[serde(default = "default_max_interval_ms")]
    pub max_interval_ms: i64,
    /// Session-wide upper bound on the total keyframes a single capture session
    /// records, across all cameras. Enforced by [`crate::CaptureSession`], not by
    /// the per-camera selector (which is count-blind and would re-cross its
    /// motion/time thresholds forever on a repeating flight path, e.g. an orbit,
    /// bloating a dataset past what a reconstructor can chew). `0` means unlimited
    /// so an unset config keeps today's behaviour.
    #[serde(default = "default_max_keyframes")]
    pub max_keyframes: u64,
}

impl Default for SelectionParams {
    fn default() -> Self {
        Self {
            min_translation_m: default_min_translation_m(),
            min_rotation_rad: default_min_rotation_rad(),
            max_interval_ms: default_max_interval_ms(),
            max_keyframes: default_max_keyframes(),
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
        // 0 = unlimited: an unset config never caps the session.
        assert_eq!(p.max_keyframes, 0);
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

    #[test]
    fn camera_config_fills_defaults_for_a_minimal_entry() {
        // The minimal entry the Python model invites (`- id: front`) must
        // deserialize on the Rust side too, or the whole atlas block is dropped:
        // role->Primary, enabled->true, reconstruct->true.
        let c: CameraConfig = serde_json::from_str(r#"{"id":"front"}"#).unwrap();
        assert_eq!(c.id, "front");
        assert_eq!(c.role, CameraRole::Primary);
        assert!(c.enabled);
        assert!(c.reconstruct);
    }

    #[test]
    fn selection_params_fill_defaults_for_a_partial_block() {
        // Tuning one threshold keeps the others at their sane defaults.
        let p: SelectionParams = serde_json::from_str(r#"{"min_translation_m":1.0}"#).unwrap();
        assert!((p.min_translation_m - 1.0).abs() < 1e-9);
        assert!((p.min_rotation_rad - 0.26).abs() < 1e-9);
        assert_eq!(p.max_interval_ms, 2000);
    }
}
