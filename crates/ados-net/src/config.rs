//! The slice of `/etc/ados/config.yaml` the uplink daemon reads.
//!
//! The daemon owns the share-uplink firewall (the REST write path only persists
//! the flag), so it reads the configured `ground_station.share_uplink` flag off
//! the same on-disk YAML the Python agent writes, typing only that field and
//! tolerating every other section. Mirrors the sibling crates' config readers.
//!
//! The reader is total: a missing or unparseable file yields the all-defaults
//! snapshot (`share_uplink: false`, matching the Python `GroundStationConfig`
//! default) rather than an error, so a fresh, partially-configured ground
//! station reconciles the firewall to the off state rather than failing to boot.

use std::path::Path;

use serde::Deserialize;

/// Canonical config location, overridable via the `ADOS_CONFIG` env var the
/// systemd unit sets — the same convention the sibling crates use.
pub const CONFIG_YAML: &str = "/etc/ados/config.yaml";

/// The `ground_station:` section. Only `share_uplink` is typed; every other
/// field is tolerated. The default mirrors the Python
/// `GroundStationConfig.share_uplink = False`, so an absent section reads as
/// off.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct GroundStationSection {
    #[serde(default)]
    pub share_uplink: bool,
}

/// The slice of the agent config the uplink daemon projects. Every field
/// defaults so a missing section never fails the load.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct UplinkConfig {
    #[serde(default)]
    pub ground_station: GroundStationSection,
}

impl UplinkConfig {
    /// Load from the canonical path (or the `ADOS_CONFIG` override). A missing
    /// or unparseable file yields the all-defaults config (share_uplink off).
    pub fn load() -> Self {
        let path = std::env::var("ADOS_CONFIG").unwrap_or_else(|_| CONFIG_YAML.to_string());
        Self::load_from(Path::new(&path))
    }

    /// Load from an explicit path (testable). All-defaults on absence / parse
    /// error.
    pub fn load_from(path: &Path) -> Self {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(_) => return UplinkConfig::default(),
        };
        let (cfg, cfg_err) = ados_config::yaml_reporting::<UplinkConfig>(&text, "net");
        // Publish the parse result so a malformed config surfaces on the fleet
        // Health view, not just in the log (per-service status sidecar).
        ados_config::write_config_status("net", cfg_err.as_deref());
        cfg
    }

    /// The configured share-uplink flag.
    pub fn share_uplink(&self) -> bool {
        self.ground_station.share_uplink
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_reads_share_uplink_off() {
        let cfg = UplinkConfig::load_from(Path::new("/nonexistent/ados/config.yaml"));
        assert!(!cfg.share_uplink());
    }

    #[test]
    fn reads_share_uplink_true_ignoring_other_sections() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.yaml");
        std::fs::write(
            &path,
            "\
mavlink:
  port: /dev/ttyACM0
ground_station:
  hotspot_ssid: ADOS-GS-1234
  share_uplink: true
video:
  mode: disabled
",
        )
        .unwrap();
        let cfg = UplinkConfig::load_from(&path);
        assert!(cfg.share_uplink());
    }

    #[test]
    fn absent_ground_station_section_reads_off() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.yaml");
        std::fs::write(&path, "mavlink:\n  port: /dev/ttyACM0\n").unwrap();
        let cfg = UplinkConfig::load_from(&path);
        assert!(!cfg.share_uplink());
    }

    #[test]
    fn unparseable_file_reads_off() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.yaml");
        std::fs::write(&path, "this: is: not: valid: yaml: [[[").unwrap();
        let cfg = UplinkConfig::load_from(&path);
        assert!(!cfg.share_uplink());
    }
}
