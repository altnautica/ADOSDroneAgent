//! The `sensors.hub` config slice and the hub `Config` packet built from it.

use std::path::{Path, PathBuf};

use ados_sensorhub_proto as proto;
use serde::{Deserialize, Serialize};

/// Where the hub is reached.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransportSpec {
    /// The first `/dev/ttyACM*` whose USB id is the hub firmware's.
    Auto,
    /// A pinned serial device (`serial:/dev/ttyACM0`).
    Serial(String),
    /// A simulated hub (`tcp:host:port`).
    Tcp(String),
}

impl TransportSpec {
    /// Parse a config value; `None` for anything that is not `auto`,
    /// `serial:<device>` or `tcp:<host>:<port>`.
    pub fn parse(s: &str) -> Option<Self> {
        let s = s.trim();
        if s == "auto" {
            return Some(Self::Auto);
        }
        if let Some(dev) = s.strip_prefix("serial:").map(str::trim) {
            return (!dev.is_empty()).then(|| Self::Serial(dev.to_string()));
        }
        let addr = s.strip_prefix("tcp:")?.trim();
        let (host, port) = addr.rsplit_once(':')?;
        (!host.is_empty() && port.parse::<u16>().is_ok()).then(|| Self::Tcp(addr.to_string()))
    }
}

/// The `sensors.hub` block as it sits in the config. Its `Default` is the
/// schema default, so the slice conformance test holds it to the schema.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
struct HubSection {
    transport: String,
    imu_odr_hz: u16,
    trigger_hz: u16,
    exposure_us: u32,
    gnss_forward: bool,
}

impl Default for HubSection {
    fn default() -> Self {
        Self {
            transport: "auto".to_string(),
            imu_odr_hz: 1000,
            trigger_hz: 20,
            exposure_us: 4000,
            gnss_forward: true,
        }
    }
}

/// The resolved hub configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HubConfig {
    pub transport: TransportSpec,
    pub imu_odr_hz: u16,
    pub trigger_hz: u16,
    pub exposure_us: u32,
    pub gnss_forward: bool,
}

impl Default for HubConfig {
    fn default() -> Self {
        Self::from_section(HubSection::default())
    }
}

/// The agent config file: `ADOS_CONFIG` when set, else `/etc/ados/config.yaml`.
pub fn config_path() -> PathBuf {
    std::env::var_os("ADOS_CONFIG")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/etc/ados/config.yaml"))
}

impl HubConfig {
    /// Load from the agent config file. A malformed file or slice is reported
    /// through the shared slice loader (and the config-status sidecar) and
    /// degrades to the schema defaults.
    pub fn load_from(path: &Path) -> Self {
        let (section, err) = match ados_config::schema::load_slice(path, "/sensors/hub") {
            Ok(s) => (s, None),
            Err(e) => (HubSection::default(), Some(e)),
        };
        ados_config::write_config_status("sensorhub", err.as_deref());
        Self::from_section(section)
    }

    fn from_section(s: HubSection) -> Self {
        let transport = TransportSpec::parse(&s.transport).unwrap_or_else(|| {
            tracing::warn!(configured = %s.transport, "sensor hub transport unknown; using auto");
            TransportSpec::Auto
        });
        Self {
            transport,
            imu_odr_hz: s.imu_odr_hz,
            trigger_hz: s.trigger_hz,
            exposure_us: s.exposure_us,
            gnss_forward: s.gnss_forward,
        }
    }

    /// The `Config` packet sent on connect: free-running triggers on every
    /// channel at `trigger_hz` (off at 0), the firmware defaults otherwise.
    pub fn packet(&self) -> proto::Config {
        let base = proto::Config::default();
        proto::Config {
            trigger_hz: self.trigger_hz,
            exposure_us: self.exposure_us,
            imu_odr_hz: self.imu_odr_hz,
            cam_mask: if self.trigger_hz > 0 { 0xFF } else { 0 },
            trigger_mode: if self.trigger_hz > 0 {
                proto::trigger_mode::FREE_RUN
            } else {
                proto::trigger_mode::OFF
            },
            ..base
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_slice_conforms() {
        ados_config::schema::assert_slice_conforms::<HubSection>("/sensors/hub");
    }

    #[test]
    fn transport_values_parse() {
        assert_eq!(TransportSpec::parse("auto"), Some(TransportSpec::Auto));
        assert_eq!(
            TransportSpec::parse("serial:/dev/ttyACM1"),
            Some(TransportSpec::Serial("/dev/ttyACM1".into()))
        );
        assert_eq!(
            TransportSpec::parse("tcp:127.0.0.1:7600"),
            Some(TransportSpec::Tcp("127.0.0.1:7600".into()))
        );
        for bad in [
            "",
            "serial:",
            "tcp:host",
            "tcp::7600",
            "tcp:h:99999",
            "usb:0",
        ] {
            assert_eq!(TransportSpec::parse(bad), None, "{bad}");
        }
    }

    #[test]
    fn a_config_file_loads_the_slice() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.yaml");
        std::fs::write(
            &path,
            "sensors:\n  hub:\n    transport: tcp:127.0.0.1:7600\n    imu_odr_hz: 2000\n    trigger_hz: 0\n",
        )
        .unwrap();
        let cfg = HubConfig::load_from(&path);
        assert_eq!(cfg.transport, TransportSpec::Tcp("127.0.0.1:7600".into()));
        assert_eq!(cfg.imu_odr_hz, 2000);
        assert!(cfg.gnss_forward);
        let pkt = cfg.packet();
        assert_eq!((pkt.trigger_hz, pkt.cam_mask), (0, 0));
        assert_eq!(pkt.trigger_mode, proto::trigger_mode::OFF);
        assert_eq!(pkt.imu_odr_hz, 2000);

        let absent = HubConfig::load_from(&dir.path().join("missing.yaml"));
        assert_eq!(absent, HubConfig::default());
        assert_eq!(absent.packet().cam_mask, 0xFF);
        assert_eq!(absent.packet().exposure_us, 4000);
    }
}
