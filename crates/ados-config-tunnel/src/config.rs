//! The `radio.tunnel` config slice this service reads, plus the profile gate.
//!
//! ## Two-tier default-off gate (ship inert, opt in after a safety review)
//!
//! - `enabled` (default **false**) is the master opt-in: absent/false ⇒ the
//!   whole config-tunnel path is inert — the drone-side terminator acts on no
//!   config tunnel, and the ground-side injector refuses every request. The
//!   systemd unit additionally gates on the `/etc/ados/tunnel-enabled` marker
//!   that mirrors this field, so an un-opted node never even runs the unit.
//! - `command_enabled` (default **false**) is the second gate on WRITES: with
//!   the channel enabled but this off, a config READ (`GET /api/config`) is
//!   served but a config WRITE (`PUT /api/config`) is refused with an honest
//!   error, mirroring `radio.crsf.mavlink_command_enabled`. It is NOT implied
//!   by opting the channel in; an operator sets it explicitly after a safety
//!   review.
//!
//! This channel carries **config request/response ONLY** — never armed-flight
//! command authority (a separate, gated safety concern). It rides the low-rate
//! `-p1` control-plane bearer; the "gate" it inherits by riding `-p1` is the
//! WFB pairing key (only a paired peer can inject/decode), which is a
//! pairing-scope gate, not a flight-authorization gate.

use std::path::Path;

use serde::Deserialize;

/// Default local UDP ingress port the service binds to receive its inbound
/// TUNNEL frames off the bearer.
pub const DEFAULT_RX_PORT: u16 = 5820;
/// Default local UDP egress port the service sends outbound TUNNEL frames to.
pub const DEFAULT_TX_PORT: u16 = 5821;

/// The `radio.tunnel` block of `/etc/ados/config.yaml`.
///
/// `rx_port` / `tx_port` are the LOCAL UDP ports the service binds/sends on.
/// They are dedicated ports (disjoint from the WFB plane ports 5600/5601/5803/
/// 5810 and the aux plane) that an `ados-radio` bearer bridge connects to the
/// `-p1` control plane in a separate, gated radio-integration step. Until that
/// bridge is wired the service still binds and reports honestly (a received
/// counter that stays 0 means nothing is arriving), never a fabricated green.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TunnelChannelConfig {
    /// Master opt-in. Absent/false ⇒ the service idles harmlessly.
    pub enabled: bool,
    /// Whether config WRITES (`PUT /api/config`) are permitted over the radio.
    /// Off by default; a read-only channel until set for a bench-validated
    /// write lane.
    pub command_enabled: bool,
    /// Local UDP ingress port for inbound TUNNEL frames.
    pub rx_port: u16,
    /// Local UDP egress port for outbound TUNNEL frames.
    pub tx_port: u16,
}

impl Default for TunnelChannelConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            command_enabled: false,
            rx_port: DEFAULT_RX_PORT,
            tx_port: DEFAULT_TX_PORT,
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
    tunnel: TunnelSection,
}

#[derive(Debug, Default, Deserialize)]
struct TunnelSection {
    #[serde(default)]
    enabled: bool,
    #[serde(default)]
    command_enabled: bool,
    #[serde(default)]
    rx_port: Option<u16>,
    #[serde(default)]
    tx_port: Option<u16>,
}

impl TunnelChannelConfig {
    /// Load from the agent config file. A missing file is the fresh-node case
    /// (quiet defaults, channel off); a malformed one is reported loudly
    /// through the shared config loader, then degrades to defaults rather than
    /// crash-looping.
    pub fn load_from(path: &Path) -> Self {
        let (raw, cfg_err) = ados_config::load_yaml_reporting::<RawConfig>(path, "tunnel");
        ados_config::write_config_status("tunnel", cfg_err.as_deref());
        Self::from_raw(raw)
    }

    fn from_raw(raw: RawConfig) -> Self {
        let t = raw.radio.tunnel;
        Self {
            enabled: t.enabled,
            command_enabled: t.command_enabled,
            rx_port: t.rx_port.filter(|p| *p != 0).unwrap_or(DEFAULT_RX_PORT),
            tx_port: t.tx_port.filter(|p| *p != 0).unwrap_or(DEFAULT_TX_PORT),
        }
    }
}

/// Resolve the agent profile as `drone` / `ground-station` / other, reading
/// `agent.profile` from the config file and falling back to the profile marker
/// file. Mirrors the sibling bearer services' profile gate.
pub fn resolve_profile(config_path: &Path, profile_conf: &Path) -> String {
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
        Some("drone") => return "drone".to_string(),
        Some("ground_station") | Some("ground-station") => return "ground-station".to_string(),
        _ => {}
    }
    if let Ok(text) = std::fs::read_to_string(profile_conf) {
        for line in text.lines() {
            let s = line.trim();
            if let Some(v) = s
                .strip_prefix("profile:")
                .or_else(|| s.strip_prefix("profile="))
            {
                let v = v.trim().trim_matches(|c| c == '"' || c == '\'');
                return if v == "drone" {
                    "drone".to_string()
                } else if v == "ground_station" || v == "ground-station" {
                    "ground-station".to_string()
                } else {
                    v.to_string()
                };
            }
        }
    }
    String::new()
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
    fn missing_file_reads_inert_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = TunnelChannelConfig::load_from(&dir.path().join("nope.yaml"));
        assert_eq!(cfg, TunnelChannelConfig::default());
        assert!(!cfg.enabled);
        assert!(!cfg.command_enabled);
        assert_eq!(cfg.rx_port, DEFAULT_RX_PORT);
        assert_eq!(cfg.tx_port, DEFAULT_TX_PORT);
    }

    #[test]
    fn parses_enable_command_and_ports() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_file(
            &dir,
            "config.yaml",
            "radio:\n  tunnel:\n    enabled: true\n    command_enabled: true\n    rx_port: 6001\n    tx_port: 6002\n",
        );
        let cfg = TunnelChannelConfig::load_from(&path);
        assert!(cfg.enabled && cfg.command_enabled);
        assert_eq!(cfg.rx_port, 6001);
        assert_eq!(cfg.tx_port, 6002);
    }

    #[test]
    fn a_zero_port_falls_back_to_the_default() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_file(
            &dir,
            "config.yaml",
            "radio:\n  tunnel:\n    enabled: true\n    rx_port: 0\n",
        );
        let cfg = TunnelChannelConfig::load_from(&path);
        assert!(cfg.enabled);
        assert!(!cfg.command_enabled); // not implied by enabling the channel
        assert_eq!(cfg.rx_port, DEFAULT_RX_PORT);
    }

    #[test]
    fn resolves_profile_from_config_then_marker() {
        let dir = tempfile::tempdir().unwrap();
        let drone = write_file(&dir, "drone.yaml", "agent:\n  profile: drone\n");
        assert_eq!(resolve_profile(&drone, &dir.path().join("none")), "drone");
        let gs = write_file(&dir, "gs.yaml", "agent:\n  profile: ground_station\n");
        assert_eq!(
            resolve_profile(&gs, &dir.path().join("none")),
            "ground-station"
        );
        // No config profile → consult the marker.
        let auto = write_file(&dir, "auto.yaml", "agent:\n  profile: auto\n");
        let marker = write_file(&dir, "profile.conf", "profile: drone\n");
        assert_eq!(resolve_profile(&auto, &marker), "drone");
    }
}
