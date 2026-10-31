//! The slice of `/etc/ados/config.yaml` the uplink daemon reads.
//!
//! The daemon owns the share-uplink firewall (the REST write path persists the
//! flag and asks the daemon to apply it), so it reads the configured
//! `ground_station.share_uplink` flag off the same on-disk YAML the Python agent
//! writes, typing only that field and tolerating every other section. Mirrors
//! the sibling crates' config readers.
//!
//! The reader is total: a missing or unparseable file yields the all-defaults
//! snapshot (`share_uplink: false`, matching the Python `GroundStationConfig`
//! default) rather than an error, so a fresh, partially-configured ground
//! station reconciles the firewall to the off state rather than failing to boot.

use std::path::Path;

use serde::{Deserialize, Serialize};

/// Canonical config location, overridable via the `ADOS_CONFIG` env var the
/// systemd unit sets — the same convention the sibling crates use.
pub const CONFIG_YAML: &str = "/etc/ados/config.yaml";

/// The `ground_station:` section. Only `share_uplink` is typed; every other
/// field is tolerated. The default must agree with `share_uplink: bool = False`
/// on `GroundStationConfig` in `src/ados/core/config/ground_station.py`, so an
/// absent section reads as off.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
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
    /// error; a malformed file or slice is reported loudly through the shared
    /// slice loader.
    pub fn load_from(path: &Path) -> Self {
        let (ground_station, cfg_err) =
            match ados_config::schema::load_slice(path, "/ground_station") {
                Ok(section) => (section, None),
                Err(e) => (GroundStationSection::default(), Some(e)),
            };
        // Publish the parse result so a malformed config surfaces on the fleet
        // Health view, not just in the log (per-service status sidecar).
        ados_config::write_config_status("net", cfg_err.as_deref());
        Self { ground_station }
    }

    /// The configured share-uplink flag.
    pub fn share_uplink(&self) -> bool {
        self.ground_station.share_uplink
    }
}

#[cfg(test)]
mod tests;
