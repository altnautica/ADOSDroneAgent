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

/// The cloud-relay endpoint under `server.cloud.url`, plus the MQTT broker
/// host/port the relays dial. Mirrors the Python `CloudConfig` fields the relay
/// reads.
#[derive(Debug, Clone, Deserialize)]
pub struct CloudSection {
    #[serde(default)]
    pub url: String,
    #[serde(default = "default_mqtt_broker")]
    pub mqtt_broker: String,
    #[serde(default = "default_mqtt_port")]
    pub mqtt_port: u16,
}

fn default_mqtt_broker() -> String {
    "mqtt.altnautica.com".to_string()
}
fn default_mqtt_port() -> u16 {
    443
}
fn default_server_mode() -> String {
    "local".to_string()
}
fn default_mqtt_transport() -> String {
    "websockets".to_string()
}
fn default_telemetry_rate() -> u32 {
    2
}

impl Default for CloudSection {
    fn default() -> Self {
        CloudSection {
            url: String::new(),
            mqtt_broker: default_mqtt_broker(),
            mqtt_port: default_mqtt_port(),
        }
    }
}

/// The `server.self_hosted:` section: an operator's own Convex deployment. Only
/// `url` is read here (the MQTT coordinates + api_key the relay reads live
/// elsewhere). Used as the convex-URL fallback when `pairing.convex_url` is
/// empty but the operator chose the self_hosted posture, so a self-hosted pair
/// that only wrote `server.self_hosted.url` still beacons. Mirrors the Python
/// `SelfHostedServerConfig`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct SelfHostedSection {
    #[serde(default)]
    pub url: String,
}

/// The `server:` section: the cloud endpoint + the relay mode + transport.
#[derive(Debug, Clone, Deserialize)]
pub struct ServerSection {
    #[serde(default = "default_server_mode")]
    pub mode: String,
    #[serde(default)]
    pub cloud: CloudSection,
    #[serde(default)]
    pub self_hosted: SelfHostedSection,
    #[serde(default = "default_mqtt_transport")]
    pub mqtt_transport: String,
    #[serde(default = "default_telemetry_rate")]
    pub telemetry_rate: u32,
    /// The operator opt-in for explicit log-window cloud export. Default OFF:
    /// the durable on-device store is the source of truth and nothing is exported
    /// to the cloud account unless the operator turns this on. Even when on, an
    /// export only runs on an explicit operator-triggered request — there is no
    /// continuous firehose. Mirrors the Python `server.cloud_logs_enabled`.
    #[serde(default)]
    pub cloud_logs_enabled: bool,
}

impl Default for ServerSection {
    fn default() -> Self {
        ServerSection {
            mode: default_server_mode(),
            cloud: CloudSection::default(),
            self_hosted: SelfHostedSection::default(),
            mqtt_transport: default_mqtt_transport(),
            telemetry_rate: default_telemetry_rate(),
            cloud_logs_enabled: false,
        }
    }
}

/// The `agent:` section. The device identity the relay reports.
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

/// The `pairing:` section: the convex URL the loops POST to + the beacon gate.
#[derive(Debug, Clone, Deserialize)]
pub struct PairingSection {
    #[serde(default)]
    pub convex_url: String,
    #[serde(default = "default_beacon_interval")]
    pub beacon_interval: u32,
    #[serde(default = "default_beacon_enabled")]
    pub beacon_enabled: bool,
}

fn default_beacon_interval() -> u32 {
    30
}
fn default_beacon_enabled() -> bool {
    true
}

impl Default for PairingSection {
    fn default() -> Self {
        PairingSection {
            convex_url: String::new(),
            beacon_interval: default_beacon_interval(),
            beacon_enabled: default_beacon_enabled(),
        }
    }
}

/// The `video.wfb:` slice the auto-pair supervisor reads. Only the arm flag is
/// typed; every other wfb field is tolerated.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct WfbSection {
    /// Whether auto-pair is armed. Default false — the operator arms it via the
    /// GCS / captive portal / REST, and a successful pair disarms it again.
    /// Mirrors the Python `video.wfb.auto_pair_enabled`.
    #[serde(default)]
    pub auto_pair_enabled: bool,
}

/// The `video:` section. Only the nested `wfb` slice is read here.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct VideoSection {
    #[serde(default)]
    pub wfb: WfbSection,
}

/// The `atlas:` section. Only the enable gate is read here; the cameras /
/// selection / intrinsics are the capture service's concern (`ados-atlas`). The
/// Atlas forwarder reads this gate so a non-Atlas agent does no Atlas work.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct AtlasSection {
    /// Whether Atlas world-model capture + forwarding is enabled. Mirrors the
    /// `atlas.enabled` key the capture service reads.
    #[serde(default)]
    pub enabled: bool,
}

/// The slice of the agent config the cloud relay reads. Every field defaults so
/// a missing section never fails the load.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct CloudConfig {
    #[serde(default)]
    pub agent: AgentSection,
    #[serde(default)]
    pub ota: OtaSection,
    #[serde(default)]
    pub server: ServerSection,
    #[serde(default)]
    pub pairing: PairingSection,
    #[serde(default)]
    pub video: VideoSection,
    #[serde(default)]
    pub atlas: AtlasSection,
}

/// The `ADOS_ATLAS_ENABLED` env override (truthy = `1` / `true` / `yes` / `on`,
/// case-insensitive). Lets a bench / a unit flip Atlas on or off without editing
/// the yaml, matching the env-override convention the other crates use.
fn atlas_env_override() -> Option<bool> {
    std::env::var("ADOS_ATLAS_ENABLED").ok().map(|v| {
        matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )
    })
}

impl CloudConfig {
    /// The convex URL the loops POST to, or empty when the relay is disabled.
    /// Cloud relay is on only for an explicit cloud posture; absent, "local",
    /// or an unknown/typo mode all stay local-first and silent. An allowlist
    /// (not a denylist) so a typo'd mode fails CLOSED — it never beacons.
    ///
    /// `pairing.convex_url` is the canonical source. When it is empty but the
    /// operator chose the self_hosted posture, fall back to
    /// `server.self_hosted.url`: the setup writer historically wrote only the
    /// latter, so without this fallback a self-hosted pair would never beacon
    /// even though the relay is enabled (the "pairs but never beacons" bug).
    pub fn effective_convex_url(&self) -> String {
        if !self.cloud_relay_enabled() {
            return String::new();
        }
        let primary = self.pairing.convex_url.trim();
        if !primary.is_empty() {
            return primary.to_string();
        }
        // Fallback: a self_hosted posture whose URL only landed in
        // server.self_hosted.url (the pre-fix setup writer's behaviour).
        if self.server.mode == "self_hosted" {
            let fallback = self.server.self_hosted.url.trim();
            if !fallback.is_empty() {
                return fallback.to_string();
            }
        }
        String::new()
    }

    /// Whether the configured server mode is an explicit cloud-relay posture.
    /// Matches the supervisor's cloud-relay gate: only `cloud` / `self_hosted`
    /// turn the relay on; everything else (absent default, `local`, unknown)
    /// stays local-first.
    pub fn cloud_relay_enabled(&self) -> bool {
        matches!(self.server.mode.as_str(), "cloud" | "self_hosted")
    }

    /// Whether the operator has opted in to explicit log-window cloud export.
    /// Default-off; combined with the cloud-paired check and an explicit
    /// operator-triggered request, this is the full gate on any export.
    pub fn cloud_logs_enabled(&self) -> bool {
        self.server.cloud_logs_enabled
    }

    /// Whether the Atlas world-model forwarder should run. The
    /// `ADOS_ATLAS_ENABLED` env var, when set, wins over the yaml `atlas.enabled`
    /// key; absent both, Atlas is OFF so a non-Atlas agent does no Atlas work
    /// (its loop early-returns and the process is byte-unchanged).
    pub fn atlas_enabled(&self) -> bool {
        atlas_env_override().unwrap_or(self.atlas.enabled)
    }
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
        ados_config::yaml_or_default(&text, "cloud")
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
        // Default arm flag is off when no wfb section is present.
        assert!(!cfg.video.wfb.auto_pair_enabled);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn cloud_logs_enabled_defaults_off_and_reads_the_opt_in() {
        // Absent → off.
        let cfg = CloudConfig::default();
        assert!(!cfg.cloud_logs_enabled());

        // Present and true → on.
        let yaml = "\
server:
  mode: cloud
  cloud_logs_enabled: true
";
        let path = temp_yaml("cloudlogs", yaml);
        let cfg = CloudConfig::load_from(&path);
        assert!(cfg.cloud_logs_enabled());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn atlas_enabled_reads_the_yaml_gate_and_the_env_override() {
        // The env var is process-global; keep every assertion in one test so the
        // set/remove is serial and no parallel test sees a stale override.
        let prev = std::env::var("ADOS_ATLAS_ENABLED").ok();
        std::env::remove_var("ADOS_ATLAS_ENABLED");

        // Absent atlas section → off.
        assert!(!CloudConfig::default().atlas_enabled());

        // yaml `atlas.enabled: true` → on.
        let on = temp_yaml("atlas-on", "atlas:\n  enabled: true\n");
        let cfg_on = CloudConfig::load_from(&on);
        assert!(cfg_on.atlas_enabled());
        let _ = std::fs::remove_file(&on);

        // yaml `atlas.enabled: false` → off.
        let off = temp_yaml("atlas-off", "atlas:\n  enabled: false\n");
        let cfg_off = CloudConfig::load_from(&off);
        assert!(!cfg_off.atlas_enabled());
        let _ = std::fs::remove_file(&off);

        // The env override wins over the yaml in both directions.
        std::env::set_var("ADOS_ATLAS_ENABLED", "1");
        assert!(cfg_off.atlas_enabled(), "env=1 forces on over yaml=false");
        std::env::set_var("ADOS_ATLAS_ENABLED", "false");
        assert!(
            !cfg_on.atlas_enabled(),
            "env=false forces off over yaml=true"
        );

        // Restore the prior environment for the rest of the suite.
        match prev {
            Some(v) => std::env::set_var("ADOS_ATLAS_ENABLED", v),
            None => std::env::remove_var("ADOS_ATLAS_ENABLED"),
        }
    }

    #[test]
    fn reads_the_wfb_auto_pair_arm_flag() {
        let yaml = "\
video:
  wfb:
    channel: 149
    auto_pair_enabled: true
";
        let path = temp_yaml("wfb", yaml);
        let cfg = CloudConfig::load_from(&path);
        assert!(cfg.video.wfb.auto_pair_enabled);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn effective_convex_url_is_an_allowlist_failing_closed() {
        // The convex URL is loaded from `pairing.convex_url`; the mode decides
        // whether it is surfaced. Build a config body that carries a URL and a
        // chosen mode, then assert the gate per mode.
        let with_mode = |mode: &str| {
            let yaml = format!(
                "\
server:
  mode: {mode}
pairing:
  convex_url: https://relay.example/convex
"
            );
            let path = temp_yaml(&format!("mode-{mode}"), &yaml);
            let cfg = CloudConfig::load_from(&path);
            let url = cfg.effective_convex_url();
            let enabled = cfg.cloud_relay_enabled();
            let _ = std::fs::remove_file(&path);
            (url, enabled)
        };

        // Explicit cloud posture → the URL is surfaced and the relay is on.
        let (url, enabled) = with_mode("cloud");
        assert_eq!(url, "https://relay.example/convex");
        assert!(enabled);

        let (url, enabled) = with_mode("self_hosted");
        assert_eq!(url, "https://relay.example/convex");
        assert!(enabled);

        // Local mode → empty, relay off.
        let (url, enabled) = with_mode("local");
        assert!(url.is_empty());
        assert!(!enabled);

        // An unknown / typo'd mode fails CLOSED — empty, relay off (the
        // allowlist's whole point: a typo never beacons to the cloud).
        let (url, enabled) = with_mode("weird");
        assert!(url.is_empty());
        assert!(!enabled);

        // Absent mode (the all-defaults config defaults the mode to "local")
        // → empty, relay off.
        let cfg = CloudConfig::default();
        assert!(cfg.effective_convex_url().is_empty());
        assert!(!cfg.cloud_relay_enabled());

        // A config with NO server section at all also defaults to off.
        let path = temp_yaml(
            "no-server",
            "pairing:\n  convex_url: https://relay.example/convex\n",
        );
        let cfg = CloudConfig::load_from(&path);
        assert!(cfg.effective_convex_url().is_empty());
        assert!(!cfg.cloud_relay_enabled());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn self_hosted_url_is_the_fallback_when_pairing_convex_url_is_empty() {
        // The "pairs but never beacons" case: a self_hosted posture whose URL
        // only landed in server.self_hosted.url (no pairing.convex_url). The
        // relay must still resolve a URL and beacon.
        let yaml = "\
server:
  mode: self_hosted
  self_hosted:
    url: https://convex-site.operator.example
";
        let path = temp_yaml("selfhosted-fallback", yaml);
        let cfg = CloudConfig::load_from(&path);
        assert!(cfg.cloud_relay_enabled());
        assert_eq!(
            cfg.effective_convex_url(),
            "https://convex-site.operator.example"
        );
        let _ = std::fs::remove_file(&path);

        // pairing.convex_url still wins when both are present.
        let yaml2 = "\
server:
  mode: self_hosted
  self_hosted:
    url: https://fallback.example
pairing:
  convex_url: https://primary.example
";
        let path2 = temp_yaml("selfhosted-primary-wins", yaml2);
        let cfg2 = CloudConfig::load_from(&path2);
        assert_eq!(cfg2.effective_convex_url(), "https://primary.example");
        let _ = std::fs::remove_file(&path2);

        // A cloud posture does NOT borrow the self_hosted URL (the fallback is
        // self_hosted-only): empty pairing.convex_url under cloud → empty.
        let yaml3 = "\
server:
  mode: cloud
  self_hosted:
    url: https://should-not-be-used.example
";
        let path3 = temp_yaml("cloud-no-selfhosted-borrow", yaml3);
        let cfg3 = CloudConfig::load_from(&path3);
        assert!(cfg3.effective_convex_url().is_empty());
        let _ = std::fs::remove_file(&path3);
    }
}
