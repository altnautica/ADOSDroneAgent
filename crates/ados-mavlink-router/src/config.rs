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

/// The serial baud of an RC module running its native MAVLink mode. Fixed by
/// the module firmware (distinct from the 420 kbaud CRSF RC framing rate),
/// and deliberately absent from the discovery sweep: the MAVLink-over-ELRS
/// serial source opens only a pinned device, never a guessed one.
pub const CRSF_MAVLINK_BAUD: u32 = 460_800;

/// The conventional MAVLink UDP port the RC module's WiFi backpack streams
/// its MAVLink datagrams to on the host.
pub const BACKPACK_UDP_PORT: u16 = 14550;

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
    /// filled in by the loader). Empty = no pin. In `crsf_rc` mode a pinned
    /// device is excluded from FC candidacy entirely: the router must never
    /// open or baud-sweep the RC module's port, or it contends with the CRSF
    /// lane for the same tty. In `mavlink` mode the ownership inverts — the
    /// pin names the serial carrier of the MAVLink-over-ELRS ingest source
    /// this router owns (see [`MavlinkConfig::crsf_mavlink_source`]).
    #[serde(skip)]
    pub crsf_device: String,
    /// Whether the `radio.crsf` lane is opted in (`radio.crsf.enabled`, filled
    /// in by the loader like `crsf_device`). Gates the MAVLink-over-ELRS
    /// source resolution: a lane that is not opted in never owns the FC
    /// source slot regardless of its configured mode.
    #[serde(skip)]
    pub crsf_enabled: bool,
    /// What the RC module carries (`radio.crsf.mode`, filled in by the
    /// loader): `crsf_rc` (the RC lane service owns the port; this router only
    /// excludes it), `mavlink` (the module runs its native MAVLink mode and
    /// THIS router owns the carrier as its FC source), or `airport` (a
    /// generic serial data pipe, no router involvement).
    #[serde(skip)]
    pub crsf_mode: String,
    /// The carrier for `mode: mavlink` (`radio.crsf.mavlink_transport`, filled
    /// in by the loader): `serial` (the pinned device at
    /// [`CRSF_MAVLINK_BAUD`]) or `backpack_wifi` (a UDP listen on
    /// [`BACKPACK_UDP_PORT`]).
    #[serde(skip)]
    pub crsf_mavlink_transport: String,
}

/// The resolved MAVLink-over-ELRS ingest source.
///
/// When the `radio.crsf` block declares the RC module runs its native MAVLink
/// mode, the module is a plain bidirectional MAVLink byte pipe — the module
/// firmware owns the CRSF air protocol internally, so nothing host-side
/// parses CRSF on this lane — and this router owns the carrier as its FC
/// source. The RC lane service (`ados-crsf`) holds off the device entirely in
/// that mode (one owner per port; it stands by at state `ready` with the mode
/// reported and transmits no RC).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CrsfMavlinkSource<'a> {
    /// The module's USB-serial port at the fixed MAVLink-mode baud
    /// ([`CRSF_MAVLINK_BAUD`]). Resolves only from a pinned device: the baud
    /// is not in the discovery sweep, so an unpinned module in MAVLink mode
    /// is an unresolved source — never a discovery guess.
    Serial { device: &'a str },
    /// The module's WiFi-backpack UDP bridge: the backpack streams MAVLink
    /// datagrams to the conventional port on the host, so the router listens
    /// there and locks to the first peer that talks.
    BackpackUdp { port: u16 },
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
            crsf_enabled: false,
            crsf_mode: "crsf_rc".to_string(),
            crsf_mavlink_transport: "serial".to_string(),
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
    /// Reads the `mavlink:` section plus the foreign `radio.crsf` fields the
    /// router gates on: the pinned CRSF/ELRS serial device (excluded from FC
    /// port candidacy in `crsf_rc` mode) and the lane's enable/mode/transport
    /// (which resolve the MAVLink-over-ELRS ingest source in `mavlink` mode).
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
            enabled: bool,
            // Nullable on disk (the config model writes `device: null` for
            // "no pin"); a bare String would fail the parse on the explicit
            // null and take the whole mavlink section down with it. The other
            // string fields are Options for the same reason.
            #[serde(default)]
            device: Option<String>,
            #[serde(default)]
            mode: Option<String>,
            #[serde(default)]
            mavlink_transport: Option<String>,
        }
        let Ok(text) = std::fs::read_to_string(path) else {
            return (MavlinkConfig::default(), None);
        };
        let (raw, error): (RawConfig, _) = ados_config::yaml_reporting(&text, "mavlink");
        let mut cfg = raw.mavlink;
        let crsf = raw.radio.crsf;
        cfg.crsf_device = crsf.device.as_deref().unwrap_or("").trim().to_string();
        cfg.crsf_enabled = crsf.enabled;
        // Absent enum-ish fields read their model defaults, matching the lane
        // service's own parse of the same block.
        cfg.crsf_mode = crsf.mode.as_deref().unwrap_or("crsf_rc").trim().to_string();
        cfg.crsf_mavlink_transport = crsf
            .mavlink_transport
            .as_deref()
            .unwrap_or("serial")
            .trim()
            .to_string();
        (cfg, error)
    }

    /// Resolve the MAVLink-over-ELRS ingest source from the `radio.crsf`
    /// block: `Some` only when the lane is opted in, its mode is `mavlink`,
    /// AND the carrier resolves — the backpack carrier always does (the
    /// listen port is fixed), the serial carrier only with a pinned device.
    /// Matches the lane service's parse posture on an unknown
    /// `mavlink_transport` value: it degrades to the serial carrier.
    pub fn crsf_mavlink_source(&self) -> Option<CrsfMavlinkSource<'_>> {
        if !self.crsf_enabled || self.crsf_mode != "mavlink" {
            return None;
        }
        if self.crsf_mavlink_transport == "backpack_wifi" {
            return Some(CrsfMavlinkSource::BackpackUdp {
                port: BACKPACK_UDP_PORT,
            });
        }
        let device = self.crsf_device.trim();
        if device.is_empty() {
            None
        } else {
            Some(CrsfMavlinkSource::Serial { device })
        }
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
        // The config model writes `device: null` for "no pin": the explicit
        // null reads as no pin and must not fail the whole mavlink parse.
        let cfg3 = dir.path().join("config3.yaml");
        write(
            &cfg3,
            "mavlink:\n  serial_port: /dev/ttyACM0\nradio:\n  crsf:\n    device: null\n",
        );
        let c3 = MavlinkConfig::load_from(&cfg3);
        assert_eq!(c3.crsf_device, "");
        assert_eq!(c3.serial_port, "/dev/ttyACM0");
    }

    #[test]
    fn crsf_mavlink_source_resolves_serial_from_the_radio_section() {
        // The lane opted in, in MAVLink mode, with a pinned device: the
        // router owns the pin at the fixed MAVLink-mode baud.
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.yaml");
        write(
            &cfg,
            "radio:\n  crsf:\n    enabled: true\n    device: /dev/ttyUSB0\n    mode: mavlink\n",
        );
        let c = MavlinkConfig::load_from(&cfg);
        assert_eq!(
            c.crsf_mavlink_source(),
            Some(CrsfMavlinkSource::Serial {
                device: "/dev/ttyUSB0"
            })
        );
    }

    #[test]
    fn crsf_mavlink_serial_without_a_pin_does_not_resolve() {
        // The fixed baud is not in the discovery sweep, so an unpinned module
        // is an unresolved source — never a guessed port.
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.yaml");
        write(
            &cfg,
            "radio:\n  crsf:\n    enabled: true\n    device: null\n    mode: mavlink\n",
        );
        assert_eq!(MavlinkConfig::load_from(&cfg).crsf_mavlink_source(), None);
    }

    #[test]
    fn crsf_backpack_wifi_resolves_without_a_pin() {
        // The backpack carrier is a fixed local UDP listen; no serial pin is
        // involved, so it resolves on the mode + transport alone.
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.yaml");
        write(
            &cfg,
            "radio:\n  crsf:\n    enabled: true\n    mode: mavlink\n    mavlink_transport: backpack_wifi\n",
        );
        assert_eq!(
            MavlinkConfig::load_from(&cfg).crsf_mavlink_source(),
            Some(CrsfMavlinkSource::BackpackUdp { port: 14550 })
        );
    }

    #[test]
    fn crsf_rc_mode_or_opted_out_lane_never_resolves_a_source() {
        let dir = tempfile::tempdir().unwrap();
        // Default mode (crsf_rc): the RC lane owns the port; no ingest source.
        let rc = dir.path().join("rc.yaml");
        write(
            &rc,
            "radio:\n  crsf:\n    enabled: true\n    device: /dev/ttyUSB0\n",
        );
        assert_eq!(MavlinkConfig::load_from(&rc).crsf_mavlink_source(), None);
        // MAVLink mode but the lane opted out: no source either.
        let off = dir.path().join("off.yaml");
        write(
            &off,
            "radio:\n  crsf:\n    enabled: false\n    device: /dev/ttyUSB0\n    mode: mavlink\n",
        );
        assert_eq!(MavlinkConfig::load_from(&off).crsf_mavlink_source(), None);
        // No radio block at all, and a missing file: both resolve nothing.
        let bare = dir.path().join("bare.yaml");
        write(&bare, "mavlink:\n  serial_port: /dev/ttyACM0\n");
        assert_eq!(MavlinkConfig::load_from(&bare).crsf_mavlink_source(), None);
        assert_eq!(
            MavlinkConfig::load_from(&dir.path().join("nope.yaml")).crsf_mavlink_source(),
            None
        );
    }

    #[test]
    fn unknown_mavlink_transport_degrades_to_the_serial_carrier() {
        // The lane service parses an unknown transport as its serial default;
        // the router must interpret the same block the same way.
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.yaml");
        write(
            &cfg,
            "radio:\n  crsf:\n    enabled: true\n    device: /dev/ttyUSB0\n    mode: mavlink\n    mavlink_transport: carrier_pigeon\n",
        );
        assert_eq!(
            MavlinkConfig::load_from(&cfg).crsf_mavlink_source(),
            Some(CrsfMavlinkSource::Serial {
                device: "/dev/ttyUSB0"
            })
        );
    }

    #[test]
    fn crsf_mode_flip_re_resolves_on_the_next_config_load() {
        // The router reads its config at startup; the persist path kicks the
        // unit on a router-relevant crsf change. A flip must resolve cleanly
        // in both directions from the same file.
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.yaml");
        write(
            &cfg,
            "radio:\n  crsf:\n    enabled: true\n    device: /dev/ttyUSB0\n    mode: mavlink\n",
        );
        assert!(MavlinkConfig::load_from(&cfg)
            .crsf_mavlink_source()
            .is_some());
        write(
            &cfg,
            "radio:\n  crsf:\n    enabled: true\n    device: /dev/ttyUSB0\n    mode: crsf_rc\n",
        );
        let back = MavlinkConfig::load_from(&cfg);
        assert_eq!(back.crsf_mavlink_source(), None);
        // Back in RC mode the pin is once again an exclusion, not a source.
        assert_eq!(back.crsf_device, "/dev/ttyUSB0");
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
