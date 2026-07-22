//! The `radio.crsf` config slice this service reads, plus the profile gate.

use std::path::Path;

use serde::Deserialize;

/// Default RC frame cadence when the config does not pin one. Conservative:
/// well inside what a USB-serial RC module accepts, and a full-duplex bridge
/// owns the half-duplex bus turnaround, so the host only has to hold the
/// frame cadence, not a microsecond turnaround budget.
pub const DEFAULT_PACKET_RATE_HZ: u16 = 50;
/// Ceiling on the configured cadence (the protocol's fastest standard rate).
pub const MAX_PACKET_RATE_HZ: u16 = 500;

/// The `radio.crsf` block of `/etc/ados/config.yaml`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CrsfLaneConfig {
    /// The lane opt-in. Absent/false ⇒ the service idles harmlessly.
    pub enabled: bool,
    /// The serial device the RC transmitter module is pinned to
    /// (e.g. `/dev/ttyUSB0`). Empty ⇒ no pin, the lane is unconfigured.
    pub device: String,
    /// RC frame cadence, frames per second, clamped to 1..=500.
    pub packet_rate_hz: u16,
}

impl Default for CrsfLaneConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            device: String::new(),
            packet_rate_hz: DEFAULT_PACKET_RATE_HZ,
        }
    }
}

#[derive(Debug, Default, Deserialize)]
struct RawConfig {
    #[serde(default)]
    radio: RadioSection,
}

#[derive(Debug, Default, Deserialize)]
struct RadioSection {
    #[serde(default)]
    crsf: CrsfSection,
}

#[derive(Debug, Default, Deserialize)]
struct CrsfSection {
    #[serde(default)]
    enabled: bool,
    #[serde(default)]
    device: String,
    #[serde(default)]
    packet_rate_hz: Option<u16>,
}

impl CrsfLaneConfig {
    /// Load from the agent config file. A missing file is the fresh-node case
    /// (quiet defaults); a malformed one is reported loudly through the
    /// shared config loader and publishes to the config-status sidecar, then
    /// degrades to defaults rather than crash-looping.
    pub fn load_from(path: &Path) -> Self {
        let (raw, cfg_err) = ados_config::load_yaml_reporting::<RawConfig>(path, "crsf");
        ados_config::write_config_status("crsf", cfg_err.as_deref());
        Self::from_raw(raw)
    }

    fn from_raw(raw: RawConfig) -> Self {
        let configured = raw.radio.crsf.packet_rate_hz;
        let packet_rate_hz = match configured {
            None => DEFAULT_PACKET_RATE_HZ,
            Some(r) if (1..=MAX_PACKET_RATE_HZ).contains(&r) => r,
            Some(r) => {
                let clamped = r.clamp(1, MAX_PACKET_RATE_HZ);
                tracing::warn!(
                    configured = r,
                    clamped,
                    "crsf packet rate out of range; clamped"
                );
                clamped
            }
        };
        Self {
            enabled: raw.radio.crsf.enabled,
            device: raw.radio.crsf.device.trim().to_string(),
            packet_rate_hz,
        }
    }
}

/// True when the agent profile resolves to `drone` — the CRSF transmitter
/// lane is ground-side (the ground node drives the RC module), so on a drone
/// this binary idles. Reads `agent.profile` from the config file, falling
/// back to the profile marker file. Defensive: the unit is already
/// profile-gated at install time.
pub fn profile_is_drone(config_path: &Path, profile_conf: &Path) -> bool {
    #[derive(Debug, Default, Deserialize)]
    struct Raw {
        #[serde(default)]
        agent: AgentSection,
    }
    #[derive(Debug, Default, Deserialize)]
    struct AgentSection {
        #[serde(default)]
        profile: Option<String>,
    }
    let cfg_profile = std::fs::read_to_string(config_path)
        .ok()
        .and_then(|t| serde_norway::from_str::<Raw>(&t).ok())
        .and_then(|r| r.agent.profile);
    match cfg_profile.as_deref() {
        Some("drone") => return true,
        Some("ground_station") | Some("ground-station") => return false,
        _ => {} // empty/auto/missing → consult the profile marker
    }
    if let Ok(text) = std::fs::read_to_string(profile_conf) {
        for line in text.lines() {
            let s = line.trim();
            if let Some(v) = s
                .strip_prefix("profile:")
                .or_else(|| s.strip_prefix("profile="))
            {
                let v = v.trim().trim_matches(|c| c == '"' || c == '\'');
                return v == "drone";
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_file(dir: &tempfile::TempDir, name: &str, body: &str) -> std::path::PathBuf {
        let path = dir.path().join(name);
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(body.as_bytes()).unwrap();
        path
    }

    #[test]
    fn missing_file_reads_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = CrsfLaneConfig::load_from(&dir.path().join("nope.yaml"));
        assert_eq!(cfg, CrsfLaneConfig::default());
        assert!(!cfg.enabled);
        assert_eq!(cfg.packet_rate_hz, DEFAULT_PACKET_RATE_HZ);
    }

    #[test]
    fn full_block_reads_through() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_file(
            &dir,
            "config.yaml",
            "radio:\n  crsf:\n    enabled: true\n    device: \" /dev/ttyUSB0 \"\n    packet_rate_hz: 150\n",
        );
        let cfg = CrsfLaneConfig::load_from(&path);
        assert!(cfg.enabled);
        assert_eq!(cfg.device, "/dev/ttyUSB0");
        assert_eq!(cfg.packet_rate_hz, 150);
    }

    #[test]
    fn out_of_range_rate_clamps() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_file(
            &dir,
            "config.yaml",
            "radio:\n  crsf:\n    enabled: true\n    packet_rate_hz: 2000\n",
        );
        assert_eq!(
            CrsfLaneConfig::load_from(&path).packet_rate_hz,
            MAX_PACKET_RATE_HZ
        );
        let path0 = write_file(
            &dir,
            "config0.yaml",
            "radio:\n  crsf:\n    packet_rate_hz: 0\n",
        );
        assert_eq!(CrsfLaneConfig::load_from(&path0).packet_rate_hz, 1);
    }

    #[test]
    fn absent_crsf_block_is_disabled() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_file(&dir, "config.yaml", "video:\n  enabled: true\n");
        let cfg = CrsfLaneConfig::load_from(&path);
        assert!(!cfg.enabled);
        assert!(cfg.device.is_empty());
    }

    #[test]
    fn profile_gate_reads_config_then_marker() {
        let dir = tempfile::tempdir().unwrap();
        let drone = write_file(&dir, "drone.yaml", "agent:\n  profile: drone\n");
        let gs = write_file(&dir, "gs.yaml", "agent:\n  profile: ground_station\n");
        let unset = write_file(&dir, "unset.yaml", "agent: {}\n");
        let marker_drone = write_file(&dir, "profile-drone.conf", "profile=drone\n");
        let marker_gs = write_file(&dir, "profile-gs.conf", "profile: ground_station\n");
        let missing = dir.path().join("missing");

        assert!(profile_is_drone(&drone, &missing));
        assert!(!profile_is_drone(&gs, &marker_drone), "config wins");
        assert!(profile_is_drone(&unset, &marker_drone));
        assert!(!profile_is_drone(&unset, &marker_gs));
        assert!(!profile_is_drone(&missing, &missing), "unknown never gates");
    }
}
