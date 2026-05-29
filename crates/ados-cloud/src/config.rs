//! Cloud relay configuration.
//!
//! Reads the sections of `/etc/ados/config.yaml` the relay needs. Mirrors the
//! Python config field names + defaults so a config written by the Python agent
//! is read identically here; serde ignores every other section, so the large
//! operator config is untouched. The long-running relay tasks read additional
//! keys; this carries the foundation (OTA + the convex relay URL) the chunk-1
//! skeleton resolves at startup.

use std::path::Path;

use serde::Deserialize;

/// Canonical config location, overridable via the `ADOS_CONFIG` env var the
/// systemd unit sets (same convention as the other crates).
pub const CONFIG_YAML: &str = "/etc/ados/config.yaml";

fn default_ota_channel() -> String {
    "stable".to_string()
}
fn default_github_repo() -> String {
    "altnautica/ADOSDroneAgent".to_string()
}
fn default_check_interval() -> u32 {
    24
}

/// The `ota:` section. Mirrors the Python `OtaConfig` fields the checker reads.
#[derive(Debug, Clone, Deserialize)]
pub struct OtaSection {
    #[serde(default = "default_ota_channel")]
    pub channel: String,
    #[serde(default = "default_github_repo")]
    pub github_repo: String,
    #[serde(default = "default_check_interval")]
    pub check_interval: u32,
}

impl Default for OtaSection {
    fn default() -> Self {
        OtaSection {
            channel: default_ota_channel(),
            github_repo: default_github_repo(),
            check_interval: default_check_interval(),
        }
    }
}

/// The cloud-relay endpoint under `server.cloud.url`. The relay POSTs the
/// heartbeat to `{url}/agent/status`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct CloudSection {
    #[serde(default)]
    pub url: String,
}

/// The `server:` section (only the cloud endpoint is read here).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ServerSection {
    #[serde(default)]
    pub cloud: CloudSection,
}

/// The slice of the agent config the cloud relay reads. Every field defaults so
/// a missing section never fails the load.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct CloudConfig {
    #[serde(default)]
    pub ota: OtaSection,
    #[serde(default)]
    pub server: ServerSection,
}

impl CloudConfig {
    /// Load from the canonical path (or the `ADOS_CONFIG` override). A missing or
    /// unparseable file yields the all-defaults config rather than failing — the
    /// relay must still start to report its own degraded state.
    pub fn load() -> Self {
        let path = std::env::var("ADOS_CONFIG").unwrap_or_else(|_| CONFIG_YAML.to_string());
        Self::load_from(Path::new(&path))
    }

    /// Load from an explicit path (testable). All-defaults on absence / parse
    /// error.
    pub fn load_from(path: &Path) -> Self {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(_) => return CloudConfig::default(),
        };
        serde_norway::from_str(&text).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn temp_yaml(name: &str, body: &str) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "ados-cloud-cfg-{}-{}.yaml",
            std::process::id(),
            name
        ));
        let mut f = std::fs::File::create(&p).unwrap();
        f.write_all(body.as_bytes()).unwrap();
        p
    }

    #[test]
    fn missing_file_yields_defaults() {
        let cfg = CloudConfig::load_from(Path::new("/nonexistent/ados/config.yaml"));
        assert_eq!(cfg.ota.channel, "stable");
        assert_eq!(cfg.ota.github_repo, "altnautica/ADOSDroneAgent");
        assert_eq!(cfg.ota.check_interval, 24);
        assert_eq!(cfg.server.cloud.url, "");
    }

    #[test]
    fn reads_ota_and_cloud_sections_ignoring_the_rest() {
        let yaml = "\
mavlink:
  port: /dev/ttyACM0
ota:
  channel: beta
  github_repo: altnautica/ADOSDroneAgent
  check_interval: 12
server:
  mode: cloud
  cloud:
    url: https://relay.example/convex
video:
  mode: disabled
";
        let path = temp_yaml("full", yaml);
        let cfg = CloudConfig::load_from(&path);
        assert_eq!(cfg.ota.channel, "beta");
        assert_eq!(cfg.ota.check_interval, 12);
        assert_eq!(cfg.server.cloud.url, "https://relay.example/convex");
        let _ = std::fs::remove_file(&path);
    }
}
