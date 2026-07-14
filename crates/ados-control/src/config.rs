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

/// The installer-provisioned persistent device identity. Read as the fallback
/// when `agent.device_id` is absent from config.yaml, so the identity survives a
/// config the Python agent / setup webapp / GCS rewrote without echoing it.
pub const DEVICE_ID_FILE: &str = "/etc/ados/device-id";

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
    /// error. When the loaded config carries no `agent.device_id`, the identity
    /// is resolved from the persistent installer files so the pairing route
    /// never reports an empty device id (which the GCS rejects as "missing
    /// device_id") while a real identity is provisioned on disk.
    pub fn load_from(path: &Path) -> Self {
        let mut cfg = match std::fs::read_to_string(path) {
            Ok(text) => ados_config::yaml_or_default(&text, "control"),
            Err(_) => PairingConfig::default(),
        };
        if cfg.agent.device_id.trim().is_empty() {
            cfg.agent.device_id = device_id_from_identity(
                Path::new(DEVICE_ID_FILE),
                std::env::var("ADOS_DEVICE_ID").ok(),
            );
        }
        cfg
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

/// Resolve the persistent device identity when `agent.device_id` is absent from
/// config.yaml. The installer provisions the device-id file (and the matching
/// `ADOS_DEVICE_ID` the unit exports); either is authoritative. Returns `""`
/// only when no identity is provisioned at all, preserving the prior behavior
/// on a truly fresh box.
fn device_id_from_identity(file: &Path, env: Option<String>) -> String {
    if let Ok(contents) = std::fs::read_to_string(file) {
        let id = contents.trim();
        if !id.is_empty() {
            return id.to_string();
        }
    }
    if let Some(env_id) = env {
        let id = env_id.trim();
        if !id.is_empty() {
            return id.to_string();
        }
    }
    String::new()
}

/// The `security.api:` slice the proxied-route auth gate reads. Only the
/// manually-configured key is typed; every other field is tolerated. Mirrors
/// the Python `ApiSecurityConfig.api_key` (default `""`).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ApiSecuritySection {
    #[serde(default)]
    pub api_key: String,
}

/// The `security:` section of `/etc/ados/config.yaml` the proxied-route auth
/// gate reads. Mirrors the Python `SecurityConfig` field names + defaults so a
/// config the Python agent wrote reads identically here:
///
/// - `api.api_key` ⇒ the manually-configured key (default `""`).
/// - `hmac_enabled` / `hmac_secret` ⇒ the HMAC signing gate (default off / `""`).
/// - `setup_token_required` ⇒ the setup-mutation token escalation (default off).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct SecuritySection {
    #[serde(default)]
    pub api: ApiSecuritySection,
    #[serde(default)]
    pub hmac_enabled: bool,
    #[serde(default)]
    pub hmac_secret: String,
    #[serde(default)]
    pub setup_token_required: bool,
}

/// The top-level config slice carrying only the `security:` section. A separate
/// total reader (not folded into `PairingConfig`) so each surface types only
/// the section it needs. Like `PairingConfig`, the reader is total: a missing
/// or unparseable file yields all-defaults, so a fresh or partially-configured
/// agent never fails to load.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ControlSecurityConfig {
    #[serde(default)]
    pub security: SecuritySection,
    #[serde(default)]
    pub mcp: McpSection,
}

/// The `mcp:` section: the AI-control-surface posture the LAN auth edge reads.
/// A control-front-only section (no Python mirror). Default all-off, so the MCP
/// token path is inert until an operator opts in.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct McpSection {
    /// When true, the LAN auth edge accepts a scoped MCP token
    /// (`X-ADOS-MCP-Token`) as an alternative to `X-ADOS-Key` on a would-be-401,
    /// enforcing the token's scope against the route class. Default off.
    #[serde(default)]
    pub token_accept_enabled: bool,
}

impl ControlSecurityConfig {
    /// Load from the canonical path (or the `ADOS_CONFIG` override).
    pub fn load() -> Self {
        let path = std::env::var("ADOS_CONFIG").unwrap_or_else(|_| CONFIG_YAML.to_string());
        Self::load_from(Path::new(&path))
    }

    /// Load from an explicit path (testable). All-defaults on absence / parse
    /// error, so the gate stays off on a config the front cannot read.
    pub fn load_from(path: &Path) -> Self {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(_) => return ControlSecurityConfig::default(),
        };
        let (cfg, cfg_err) = ados_config::yaml_reporting::<ControlSecurityConfig>(&text, "control");
        // Publish the parse result so a malformed config surfaces on the fleet
        // Health view, not just in the log (per-service status sidecar). This is
        // the control service's startup config load (built once at server init);
        // the pairing reader stays on the quiet-default helper (it is on the hot
        // per-request path).
        ados_config::write_config_status("control", cfg_err.as_deref());
        cfg
    }

    /// The `security:` section.
    pub fn security(&self) -> &SecuritySection {
        &self.security
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
    fn device_id_falls_back_to_the_identity_file_then_env() {
        // The provisioned identity file wins (trimmed).
        let f = temp_yaml("device-id", "630e079b69d6\n");
        assert_eq!(
            device_id_from_identity(&f, Some("env-id".to_string())),
            "630e079b69d6"
        );
        let _ = std::fs::remove_file(&f);
        // The env is used when the file is absent.
        let missing = Path::new("/nonexistent/ados/device-id-xyz");
        assert_eq!(
            device_id_from_identity(missing, Some(" env-id \n".to_string())),
            "env-id"
        );
        // Empty only when nothing is provisioned (preserves the fresh-box default).
        assert_eq!(device_id_from_identity(missing, None), "");
        assert_eq!(device_id_from_identity(missing, Some(String::new())), "");
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

    #[test]
    fn security_defaults_are_the_python_defaults() {
        // A missing file → all-defaults → the same defaults the Python agent uses.
        let cfg = ControlSecurityConfig::load_from(Path::new("/nonexistent/ados/config.yaml"));
        let s = cfg.security();
        assert_eq!(s.api.api_key, "");
        assert!(!s.hmac_enabled);
        assert_eq!(s.hmac_secret, "");
        assert!(!s.setup_token_required);
    }

    #[test]
    fn a_python_written_config_reads_identically() {
        // A config the Python agent wrote reads the same field values here.
        let yaml = "\
security:
  api:
    api_key: configured-key
  hmac_enabled: true
  hmac_secret: a-very-long-secret-key
  setup_token_required: true
";
        let path = temp_yaml("py-security", yaml);
        let cfg = ControlSecurityConfig::load_from(&path);
        let s = cfg.security();
        assert_eq!(s.api.api_key, "configured-key");
        assert!(s.hmac_enabled);
        assert_eq!(s.hmac_secret, "a-very-long-secret-key");
        assert!(s.setup_token_required);
        let _ = std::fs::remove_file(&path);
    }
}
