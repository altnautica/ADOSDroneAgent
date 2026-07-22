//! MAVLink service configuration.
//!
//! Reads the `mavlink:` section of `/etc/ados/config.yaml`. Mirrors the Python
//! `MavlinkConfig` (core/config/mavlink.py): the same field names and the same
//! defaults so a config written by the Python agent is read identically here.
//! serde ignores every other section, so the large operator config is
//! untouched.

use std::path::Path;

use serde::Deserialize;

/// Canonical config location (overridable via the `ADOS_CONFIG` env var, which
/// the systemd unit sets).
pub const CONFIG_YAML: &str = "/etc/ados/config.yaml";

fn default_baud_rate() -> u32 {
    57600
}
fn default_source() -> String {
    "auto".to_string()
}
fn default_system_id() -> u8 {
    1
}
fn default_component_id() -> u8 {
    191
}
fn default_endpoint_type() -> String {
    "websocket".to_string()
}
fn default_endpoint_host() -> String {
    "0.0.0.0".to_string()
}
fn default_endpoint_port() -> u16 {
    8765
}
fn default_endpoint_enabled() -> bool {
    true
}
fn default_endpoints() -> Vec<EndpointConfig> {
    vec![EndpointConfig::default()]
}
fn default_ws_proxy_enforce_auth() -> bool {
    false
}

/// A network entry point. v1 ships only the `websocket` type.
#[derive(Debug, Clone, Deserialize)]
pub struct EndpointConfig {
    #[serde(rename = "type", default = "default_endpoint_type")]
    pub kind: String,
    #[serde(default = "default_endpoint_host")]
    pub host: String,
    #[serde(default = "default_endpoint_port")]
    pub port: u16,
    #[serde(default = "default_endpoint_enabled")]
    pub enabled: bool,
}

impl Default for EndpointConfig {
    fn default() -> Self {
        Self {
            kind: default_endpoint_type(),
            host: default_endpoint_host(),
            port: default_endpoint_port(),
            enabled: default_endpoint_enabled(),
        }
    }
}

/// The `mavlink:` config section.
#[derive(Debug, Clone, Deserialize)]
pub struct MavlinkConfig {
    /// The FC transport class the operator picked: `auto` (discover + baud-probe
    /// any candidate serial port), `serial` (the configured `serial_port` +
    /// `baud_rate`), or `udp`/`tcp` (a network transport, with the host:port in
    /// `serial_port` as `udp:host:port` / `tcp:host:port`). Default `auto` so an
    /// un-upgraded config behaves exactly as before. Surfaced on the state
    /// snapshot as `fc_source` so the GCS picker reflects the live choice.
    #[serde(default = "default_source")]
    pub source: String,
    #[serde(default)]
    pub serial_port: String,
    #[serde(default = "default_baud_rate")]
    pub baud_rate: u32,
    #[serde(default = "default_system_id")]
    pub system_id: u8,
    #[serde(default = "default_component_id")]
    pub component_id: u8,
    #[serde(default = "default_endpoints")]
    pub endpoints: Vec<EndpointConfig>,
    /// When true, the direct-GCS WebSocket proxy rejects a paired-agent
    /// connection from an off-box peer that does not present the stored pairing
    /// key. When false (the default), an unauthorized connection is logged but
    /// still admitted, so the data-path behaviour is unchanged until a bench
    /// session enables enforcement.
    #[serde(default = "default_ws_proxy_enforce_auth")]
    pub ws_proxy_enforce_auth: bool,
    /// The serial device an ExpressLRS / CRSF RC module is pinned to
    /// (`radio.crsf.device` in the config — the top-level `radio:` section, NOT
    /// the `mavlink:` block, so it is skipped by the section deserializer and
    /// filled in by the loader). Empty = no pin. A pinned device is excluded
    /// from FC candidacy entirely: the router must never open or baud-sweep the
    /// RC module's port, or it contends with the CRSF lane for the same tty.
    #[serde(skip)]
    pub crsf_device: String,
}

impl Default for MavlinkConfig {
    fn default() -> Self {
        Self {
            source: default_source(),
            serial_port: String::new(),
            baud_rate: default_baud_rate(),
            system_id: default_system_id(),
            component_id: default_component_id(),
            endpoints: default_endpoints(),
            ws_proxy_enforce_auth: default_ws_proxy_enforce_auth(),
            crsf_device: String::new(),
        }
    }
}

impl MavlinkConfig {
    /// Load from the canonical path (or `ADOS_CONFIG` when set). A missing or
    /// unreadable file yields the defaults, matching the Python loader. This is the
    /// real startup entry, so it also publishes the config-status sidecar: a
    /// malformed config surfaces on the remote Health view, not just in the log.
    pub fn load() -> Self {
        let path = std::env::var("ADOS_CONFIG").unwrap_or_else(|_| CONFIG_YAML.to_string());
        let (config, error) = Self::load_reporting(Path::new(&path));
        ados_config::write_config_status("mavlink", error.as_deref());
        config
    }

    /// Load from an explicit path (testable). Does NOT publish the config-status
    /// sidecar — only the real [`load`](Self::load) startup path does, so tests
    /// never write to the run dir.
    pub fn load_from(path: &Path) -> Self {
        Self::load_reporting(path).0
    }

    /// Load from an explicit path, also returning the parse-error message so the
    /// startup path can publish it. `None` on success or a missing/unreadable
    /// file; `Some(msg)` on a present-but-malformed file.
    ///
    /// Reads the `mavlink:` section plus the one foreign field the router
    /// gates on: `radio.crsf.device`, the pinned CRSF/ELRS serial device that
    /// must be excluded from FC port candidacy.
    fn load_reporting(path: &Path) -> (Self, Option<String>) {
        #[derive(Debug, Default, Deserialize)]
        struct RawConfig {
            #[serde(default)]
            mavlink: MavlinkConfig,
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
            device: String,
        }
        let Ok(text) = std::fs::read_to_string(path) else {
            return (MavlinkConfig::default(), None);
        };
        let (raw, error): (RawConfig, _) = ados_config::yaml_reporting(&text, "mavlink");
        let mut cfg = raw.mavlink;
        cfg.crsf_device = raw.radio.crsf.device.trim().to_string();
        (cfg, error)
    }

    /// The first enabled WebSocket endpoint port, if any (the proxy bind port).
    pub fn websocket_port(&self) -> Option<u16> {
        self.endpoints
            .iter()
            .find(|e| e.enabled && e.kind == "websocket")
            .map(|e| e.port)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write(path: &Path, contents: &str) {
        let mut f = std::fs::File::create(path).unwrap();
        f.write_all(contents.as_bytes()).unwrap();
    }

    #[test]
    fn missing_file_yields_python_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let c = MavlinkConfig::load_from(&dir.path().join("nope.yaml"));
        assert_eq!(c.serial_port, "");
        assert_eq!(c.baud_rate, 57600);
        assert_eq!(c.system_id, 1);
        assert_eq!(c.component_id, 191);
        assert_eq!(c.websocket_port(), Some(8765));
    }

    #[test]
    fn reads_explicit_mavlink_section_and_ignores_others() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.yaml");
        write(
            &cfg,
            "agent:\n  profile: drone\nmavlink:\n  serial_port: /dev/ttyACM0\n  baud_rate: 921600\n  system_id: 1\n  component_id: 191\nvideo:\n  mode: auto\n",
        );
        let c = MavlinkConfig::load_from(&cfg);
        assert_eq!(c.serial_port, "/dev/ttyACM0");
        assert_eq!(c.baud_rate, 921600);
        // endpoints omitted -> default websocket 8765
        assert_eq!(c.websocket_port(), Some(8765));
    }

    #[test]
    fn partial_mavlink_section_fills_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.yaml");
        write(&cfg, "mavlink:\n  serial_port: /dev/ttyAMA0\n");
        let c = MavlinkConfig::load_from(&cfg);
        assert_eq!(c.serial_port, "/dev/ttyAMA0");
        assert_eq!(c.baud_rate, 57600);
        assert_eq!(c.component_id, 191);
    }

    #[test]
    fn explicit_endpoints_override_default() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.yaml");
        write(
            &cfg,
            "mavlink:\n  endpoints:\n    - type: websocket\n      host: 0.0.0.0\n      port: 9000\n      enabled: true\n",
        );
        let c = MavlinkConfig::load_from(&cfg);
        assert_eq!(c.websocket_port(), Some(9000));
    }

    #[test]
    fn disabled_websocket_endpoint_is_not_selected() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.yaml");
        write(
            &cfg,
            "mavlink:\n  endpoints:\n    - type: websocket\n      port: 8765\n      enabled: false\n",
        );
        let c = MavlinkConfig::load_from(&cfg);
        assert_eq!(c.websocket_port(), None);
    }

    #[test]
    fn ws_proxy_enforce_auth_defaults_off() {
        let dir = tempfile::tempdir().unwrap();
        // Missing file, and a config that omits the flag, both default to off so
        // an un-upgraded config never starts enforcing.
        assert!(!MavlinkConfig::load_from(&dir.path().join("nope.yaml")).ws_proxy_enforce_auth);
        let cfg = dir.path().join("config.yaml");
        write(&cfg, "mavlink:\n  serial_port: /dev/ttyACM0\n");
        assert!(!MavlinkConfig::load_from(&cfg).ws_proxy_enforce_auth);
    }

    #[test]
    fn crsf_device_pin_reads_from_the_radio_section() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.yaml");
        write(
            &cfg,
            "mavlink:\n  serial_port: /dev/ttyACM0\nradio:\n  crsf:\n    device: \" /dev/ttyUSB0 \"\n",
        );
        let c = MavlinkConfig::load_from(&cfg);
        // The pin is read from the top-level radio section and trimmed; the
        // mavlink block itself is untouched by it.
        assert_eq!(c.crsf_device, "/dev/ttyUSB0");
        assert_eq!(c.serial_port, "/dev/ttyACM0");
    }

    #[test]
    fn crsf_device_pin_defaults_empty() {
        let dir = tempfile::tempdir().unwrap();
        // Missing file, and a config with no radio block, both read no pin.
        assert_eq!(
            MavlinkConfig::load_from(&dir.path().join("nope.yaml")).crsf_device,
            ""
        );
        let cfg = dir.path().join("config.yaml");
        write(&cfg, "mavlink:\n  serial_port: /dev/ttyACM0\n");
        assert_eq!(MavlinkConfig::load_from(&cfg).crsf_device, "");
        // A crsf block with no device also reads no pin.
        let cfg2 = dir.path().join("config2.yaml");
        write(&cfg2, "radio:\n  crsf:\n    enabled: true\n");
        assert_eq!(MavlinkConfig::load_from(&cfg2).crsf_device, "");
    }

    #[test]
    fn ws_proxy_enforce_auth_reads_explicit_true() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.yaml");
        write(&cfg, "mavlink:\n  ws_proxy_enforce_auth: true\n");
        assert!(MavlinkConfig::load_from(&cfg).ws_proxy_enforce_auth);
    }

    #[test]
    fn source_defaults_to_auto_and_reads_explicit() {
        let dir = tempfile::tempdir().unwrap();
        // Missing file and an omitted field both default to "auto".
        assert_eq!(
            MavlinkConfig::load_from(&dir.path().join("nope.yaml")).source,
            "auto"
        );
        let cfg = dir.path().join("config.yaml");
        write(&cfg, "mavlink:\n  serial_port: /dev/ttyACM0\n");
        assert_eq!(MavlinkConfig::load_from(&cfg).source, "auto");
        // An explicit value is read verbatim.
        let cfg2 = dir.path().join("config2.yaml");
        write(
            &cfg2,
            "mavlink:\n  source: serial\n  serial_port: /dev/ttyACM0\n",
        );
        assert_eq!(MavlinkConfig::load_from(&cfg2).source, "serial");
    }
}
