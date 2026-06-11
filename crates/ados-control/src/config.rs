//! The slice of `/etc/ados/config.yaml` the pairing-info route projects.
//!
//! The FastAPI pairing-info endpoint reads its identity fields off the loaded
//! agent config: the device id, the human name, the profile, and the radio
//! peer's device id. This module reads the same on-disk YAML directly, typing
//! only the fields the route needs and tolerating every other section, the same
//! pattern the cloud relay's config reader uses. A config the Python agent wrote
//! reads identically here because the field names + defaults are mirrored.
//!
//! The reader is total: a missing or unparseable file yields the all-defaults
//! snapshot rather than an error, so the guaranteed-200 pairing-info contract
//! holds on a fresh, partially-configured agent.

use std::path::Path;

use serde::Deserialize;

/// Canonical config location, overridable via the `ADOS_CONFIG` env var the
/// systemd unit sets — the same convention the sibling crates use.
pub const CONFIG_YAML: &str = "/etc/ados/config.yaml";

/// The `agent:` section: the device identity the pairing-info route reports.
/// Defaults mirror the Python `AgentConfig` field defaults so an absent section
/// reads the same value the loaded Python config would (`name: "my-drone"`,
/// `profile: "auto"`).
#[derive(Debug, Clone, Deserialize)]
pub struct AgentSection {
    #[serde(default)]
    pub device_id: String,
    #[serde(default = "default_agent_name")]
    pub name: String,
    #[serde(default = "default_agent_profile")]
    pub profile: String,
}

fn default_agent_name() -> String {
    "my-drone".to_string()
}

fn default_agent_profile() -> String {
    "auto".to_string()
}

impl Default for AgentSection {
    fn default() -> Self {
        AgentSection {
            device_id: String::new(),
            name: default_agent_name(),
            profile: default_agent_profile(),
        }
    }
}

/// The `video.wfb:` slice the pairing-info route reads. Only the radio peer's
/// device id is typed; every other wfb field is tolerated. Mirrors the Python
/// `video.wfb.paired_with_device_id`, which persists on both profiles once a
/// bind tunnel (or a PresenceBeacon) back-fills it.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct WfbSection {
    #[serde(default)]
    pub paired_with_device_id: Option<String>,
}

/// The `video:` section. Only the nested `wfb` slice is read here.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct VideoSection {
    #[serde(default)]
    pub wfb: WfbSection,
}

/// The slice of the agent config the pairing-info route projects. Every field
/// defaults so a missing section never fails the load.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct PairingConfig {
    #[serde(default)]
    pub agent: AgentSection,
    #[serde(default)]
    pub video: VideoSection,
}

impl PairingConfig {
    /// Load from the canonical path (or the `ADOS_CONFIG` override). A missing or
    /// unparseable file yields the all-defaults config rather than failing — the
    /// pairing-info route must still answer 200.
    pub fn load() -> Self {
        let path = std::env::var("ADOS_CONFIG").unwrap_or_else(|_| CONFIG_YAML.to_string());
        Self::load_from(Path::new(&path))
    }

    /// Load from an explicit path (testable). All-defaults on absence / parse
    /// error.
    pub fn load_from(path: &Path) -> Self {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(_) => return PairingConfig::default(),
        };
        serde_norway::from_str(&text).unwrap_or_default()
    }

    /// The radio peer's device id, or `None` when no peer is recorded or it is
    /// the empty string. Mirrors the Python guard
    /// (`isinstance(peer_raw, str) and peer_raw`): a present-but-empty value is
    /// treated as no peer.
    pub fn radio_peer_device_id(&self) -> Option<String> {
        self.video
            .wfb
            .paired_with_device_id
            .as_ref()
            .filter(|s| !s.is_empty())
            .cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn temp_yaml(name: &str, body: &str) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "ados-control-cfg-{}-{}.yaml",
            std::process::id(),
            name
        ));
        let mut f = std::fs::File::create(&p).unwrap();
        f.write_all(body.as_bytes()).unwrap();
        p
    }

    #[test]
    fn missing_file_yields_the_python_defaults() {
        let cfg = PairingConfig::load_from(Path::new("/nonexistent/ados/config.yaml"));
        assert_eq!(cfg.agent.device_id, "");
        assert_eq!(cfg.agent.name, "my-drone");
        assert_eq!(cfg.agent.profile, "auto");
        assert_eq!(cfg.radio_peer_device_id(), None);
    }

    #[test]
    fn reads_agent_identity_ignoring_the_rest() {
        let yaml = "\
mavlink:
  port: /dev/ttyACM0
agent:
  device_id: abcdef1234567890
  name: test-drone
  profile: drone
video:
  mode: disabled
";
        let path = temp_yaml("agent", yaml);
        let cfg = PairingConfig::load_from(&path);
        assert_eq!(cfg.agent.device_id, "abcdef1234567890");
        assert_eq!(cfg.agent.name, "test-drone");
        assert_eq!(cfg.agent.profile, "drone");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn reads_the_radio_peer_device_id() {
        let yaml = "\
video:
  wfb:
    channel: 149
    paired_with_device_id: peer-1234567890
";
        let path = temp_yaml("wfbpeer", yaml);
        let cfg = PairingConfig::load_from(&path);
        assert_eq!(
            cfg.radio_peer_device_id(),
            Some("peer-1234567890".to_string())
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn an_empty_peer_is_treated_as_no_peer() {
        // Matches the Python `isinstance(peer_raw, str) and peer_raw` guard: a
        // present-but-empty value reads as no peer.
        let yaml = "\
video:
  wfb:
    paired_with_device_id: \"\"
";
        let path = temp_yaml("emptypeer", yaml);
        let cfg = PairingConfig::load_from(&path);
        assert_eq!(cfg.radio_peer_device_id(), None);
        let _ = std::fs::remove_file(&path);
    }
}
