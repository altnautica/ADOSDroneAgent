//! Config for camera USB-recovery (`video.usb_recovery` + `video.camera.expected`).
//!
//! Pure parsing + the expected-camera resolution are unit-tested on every host;
//! the canonical-path read is Linux-only.

use std::time::Duration;

#[cfg(target_os = "linux")]
use crate::config::CONFIG_YAML;

pub(super) const DEFAULT_DEBOUNCE_S: u64 = 20;
pub(super) const DEFAULT_MAX_ATTEMPTS: u32 = 3;
pub(super) const DEFAULT_COOLDOWN_SCHEDULE_S: [u64; 3] = [10, 30, 60];
pub(super) const DEFAULT_HEALTHY_RESET_S: u64 = 120;
pub(super) const DEFAULT_TICK_INTERVAL_S: u64 = 5;
pub(super) const DEFAULT_BOOT_RESET_WINDOW_S: u64 = 180;

/// Configuration, read from `video.usb_recovery` (+ `video.camera.expected`).
/// Default-ON (detect + alert); destructive actions stay gated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CameraRecoveryConfig {
    pub enabled: bool,
    /// "auto" | "true" | "false" — whether a camera is expected on this rig.
    pub expected: String,
    pub debounce: Duration,
    pub max_attempts: u32,
    pub cooldown_schedule: Vec<u64>,
    pub healthy_reset: Duration,
    pub tick_interval: Duration,
    /// Opt-in: allow a shared-hub reset (boot-time-only, guard-gated).
    pub allow_hub_reset: bool,
    pub boot_reset_window: Duration,
    /// Allow a clean per-port re-enable on a hub that exposes it.
    pub allow_ppps: bool,
    /// Opt-in (bench/ground): allow an AGGRESSIVE shared-hub reset that
    /// re-enumerates the camera even when it shares the hub with the radio/FC
    /// (the guard would otherwise refuse). Recovers a wedged camera on a board
    /// with no per-port power without a manual replug, at the cost of a brief
    /// radio+FC re-enumeration. Gated additionally on the FC being DISARMED
    /// (fail-closed), so it can never fire in flight. Off by default.
    pub allow_shared_hub_reset: bool,
}

impl Default for CameraRecoveryConfig {
    fn default() -> Self {
        CameraRecoveryConfig {
            enabled: true,
            expected: "auto".to_string(),
            debounce: Duration::from_secs(DEFAULT_DEBOUNCE_S),
            max_attempts: DEFAULT_MAX_ATTEMPTS,
            cooldown_schedule: DEFAULT_COOLDOWN_SCHEDULE_S.to_vec(),
            healthy_reset: Duration::from_secs(DEFAULT_HEALTHY_RESET_S),
            tick_interval: Duration::from_secs(DEFAULT_TICK_INTERVAL_S),
            allow_hub_reset: false,
            boot_reset_window: Duration::from_secs(DEFAULT_BOOT_RESET_WINDOW_S),
            allow_ppps: true,
            allow_shared_hub_reset: false,
        }
    }
}

/// Parse `video.usb_recovery` + `video.camera.expected`. Absent / malformed →
/// enabled defaults.
pub fn read_config_from(text: &str) -> CameraRecoveryConfig {
    #[derive(serde::Deserialize, Default)]
    struct Raw {
        #[serde(default)]
        video: Video,
    }
    #[derive(serde::Deserialize, Default)]
    struct Video {
        #[serde(default)]
        camera: Camera,
        #[serde(default)]
        usb_recovery: Option<Rec>,
    }
    #[derive(serde::Deserialize, Default)]
    struct Camera {
        #[serde(default)]
        expected: Option<String>,
    }
    #[derive(serde::Deserialize)]
    struct Rec {
        #[serde(default = "default_true")]
        enabled: bool,
        #[serde(default)]
        debounce_s: Option<u64>,
        #[serde(default)]
        max_attempts: Option<u32>,
        #[serde(default)]
        cooldown_schedule_s: Option<Vec<u64>>,
        #[serde(default)]
        healthy_reset_s: Option<u64>,
        #[serde(default)]
        tick_interval_s: Option<u64>,
        #[serde(default)]
        allow_hub_reset: Option<bool>,
        #[serde(default)]
        boot_reset_window_s: Option<u64>,
        #[serde(default)]
        allow_ppps: Option<bool>,
        #[serde(default)]
        allow_shared_hub_reset: Option<bool>,
    }
    fn default_true() -> bool {
        true
    }
    let mut cfg = CameraRecoveryConfig::default();
    if let Ok(raw) = serde_norway::from_str::<Raw>(text) {
        if let Some(e) = raw.video.camera.expected {
            let e = e.trim().to_lowercase();
            if e == "true" || e == "false" || e == "auto" {
                cfg.expected = e;
            }
        }
        if let Some(r) = raw.video.usb_recovery {
            cfg.enabled = r.enabled;
            cfg.debounce = Duration::from_secs(r.debounce_s.unwrap_or(DEFAULT_DEBOUNCE_S).max(1));
            cfg.max_attempts = r.max_attempts.unwrap_or(DEFAULT_MAX_ATTEMPTS).max(1);
            cfg.cooldown_schedule = r
                .cooldown_schedule_s
                .filter(|v| !v.is_empty())
                .unwrap_or_else(|| DEFAULT_COOLDOWN_SCHEDULE_S.to_vec());
            cfg.healthy_reset =
                Duration::from_secs(r.healthy_reset_s.unwrap_or(DEFAULT_HEALTHY_RESET_S).max(1));
            cfg.tick_interval =
                Duration::from_secs(r.tick_interval_s.unwrap_or(DEFAULT_TICK_INTERVAL_S).max(1));
            cfg.allow_hub_reset = r.allow_hub_reset.unwrap_or(false);
            cfg.boot_reset_window = Duration::from_secs(
                r.boot_reset_window_s
                    .unwrap_or(DEFAULT_BOOT_RESET_WINDOW_S)
                    .max(1),
            );
            cfg.allow_ppps = r.allow_ppps.unwrap_or(true);
            cfg.allow_shared_hub_reset = r.allow_shared_hub_reset.unwrap_or(false);
        }
    }
    cfg
}

/// Resolve whether a camera is expected. `true`/`false` are explicit; `auto`
/// (default) expects a camera iff one enumerated successfully at least once on
/// this rig (the persisted last-known-good record exists — survives reboot, so
/// the boot case still arms). Pure.
pub fn camera_expected(expected_cfg: &str, last_good_exists: bool) -> bool {
    match expected_cfg {
        "true" => true,
        "false" => false,
        _ => last_good_exists,
    }
}

#[cfg(target_os = "linux")]
pub(super) fn read_config() -> CameraRecoveryConfig {
    match std::fs::read_to_string(CONFIG_YAML) {
        Ok(t) => read_config_from(&t),
        Err(_) => CameraRecoveryConfig::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absent_section_is_enabled_with_defaults() {
        let cfg = read_config_from("agent:\n  name: x\n");
        assert!(cfg.enabled);
        assert_eq!(cfg.expected, "auto");
        assert_eq!(cfg.max_attempts, DEFAULT_MAX_ATTEMPTS);
        assert_eq!(cfg.debounce, Duration::from_secs(DEFAULT_DEBOUNCE_S));
        assert!(!cfg.allow_hub_reset);
        assert!(cfg.allow_ppps);
        assert!(!cfg.allow_shared_hub_reset);
    }

    #[test]
    fn explicit_tunables_and_expected() {
        let cfg = read_config_from(
            "video:\n  camera:\n    expected: \"true\"\n  usb_recovery:\n    enabled: false\n    debounce_s: 5\n    max_attempts: 2\n    allow_hub_reset: true\n    allow_ppps: false\n    allow_shared_hub_reset: true\n",
        );
        assert!(!cfg.enabled);
        assert_eq!(cfg.expected, "true");
        assert_eq!(cfg.debounce, Duration::from_secs(5));
        assert_eq!(cfg.max_attempts, 2);
        assert!(cfg.allow_hub_reset);
        assert!(!cfg.allow_ppps);
        assert!(cfg.allow_shared_hub_reset);
    }

    #[test]
    fn malformed_config_defaults_enabled() {
        assert!(read_config_from(": : : not yaml").enabled);
    }

    #[test]
    fn expected_resolution_matrix() {
        // auto = expected iff a last-good record exists.
        assert!(!camera_expected("auto", false));
        assert!(camera_expected("auto", true));
        // Explicit wins regardless of last-good. This is the key anti-false-
        // positive guarantee: a camera-less drone with no last-good stays idle.
        assert!(camera_expected("true", false));
        assert!(!camera_expected("false", true));
    }
}
