//! The `gnss` config slice this service reads.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::base::{BaseMode, Llh};
use crate::valset::RxPort;

/// Baud for a serial source that names none (the u-blox UART default).
pub const DEFAULT_BAUD: u32 = 38_400;

/// Where a receiver's bytes come from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    Serial { path: String, baud: u32 },
    Udp(u16),
    Hub,
}

impl Source {
    pub fn parse(s: &str) -> Option<Self> {
        let s = s.trim();
        if s == "hub" {
            return Some(Self::Hub);
        }
        if let Some(port) = s.strip_prefix("udp:") {
            return port.parse().ok().filter(|p| *p != 0).map(Self::Udp);
        }
        let dev = s.strip_prefix("serial:")?;
        let (path, baud) = match dev.split_once('@') {
            Some((p, b)) => (p, b.parse().ok().filter(|b| *b > 0)?),
            None => (dev, DEFAULT_BAUD),
        };
        (!path.is_empty()).then(|| Self::Serial {
            path: path.to_string(),
            baud,
        })
    }

    /// The `source` field of the status: `hub`, `udp:7800`, `serial:/dev/ttyUSB0`.
    pub fn label(&self) -> String {
        match self {
            Self::Serial { path, .. } => format!("serial:{path}"),
            Self::Udp(p) => format!("udp:{p}"),
            Self::Hub => "hub".to_string(),
        }
    }
}

/// One configured receiver.
#[derive(Debug, Clone, PartialEq)]
pub struct ReceiverCfg {
    pub id: String,
    pub source: Source,
    pub model: Option<String>,
    pub port: RxPort,
}

/// The effective GNSS config.
#[derive(Debug, Clone, PartialEq)]
pub struct GnssConfig {
    pub receivers: Vec<ReceiverCfg>,
    pub pps_device: Option<PathBuf>,
    pub base: BaseMode,
    pub survey_min_dur_s: u32,
    pub survey_accuracy_m: f64,
    pub heading_enabled: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct ReceiverSection {
    id: String,
    source: String,
    #[serde(default)]
    model: Option<String>,
    #[serde(default = "default_port")]
    port: String,
}

fn default_port() -> String {
    "uart1".to_string()
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(default)]
struct BaseSection {
    mode: String,
    min_dur_s: u32,
    accuracy_m: f64,
    lat_deg: Option<f64>,
    lon_deg: Option<f64>,
    h_ellipsoid_m: Option<f64>,
}

impl Default for BaseSection {
    fn default() -> Self {
        Self {
            mode: "off".to_string(),
            min_dur_s: 300,
            accuracy_m: 2.0,
            lat_deg: None,
            lon_deg: None,
            h_ellipsoid_m: None,
        }
    }
}

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(default)]
struct HeadingSection {
    enabled: bool,
}

/// The `gnss` slice as it sits in the config; `Default` is the schema default.
#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub(crate) struct GnssSection {
    receivers: Vec<ReceiverSection>,
    pps_device: Option<String>,
    base: BaseSection,
    heading: HeadingSection,
}

impl GnssConfig {
    /// Load from the agent config. A malformed file or slice is reported
    /// through the shared loader and the config-status sidecar, and the
    /// service runs with no receivers rather than crash-looping.
    pub fn load_from(path: &Path) -> Self {
        let (sec, err) = match ados_config::schema::load_slice(path, "/gnss") {
            Ok(s) => (s, None),
            Err(e) => (GnssSection::default(), Some(e)),
        };
        ados_config::write_config_status("gnss", err.as_deref());
        Self::from_section(sec)
    }

    pub(crate) fn from_section(sec: GnssSection) -> Self {
        let receivers = sec
            .receivers
            .into_iter()
            .filter_map(|r| {
                let Some(source) = Source::parse(&r.source) else {
                    tracing::warn!(id = %r.id, source = %r.source, "gnss_receiver_source_invalid");
                    return None;
                };
                let port = RxPort::parse(&r.port).unwrap_or_else(|| {
                    tracing::warn!(id = %r.id, port = %r.port, "gnss_receiver_port_unknown");
                    RxPort::Uart1
                });
                Some(ReceiverCfg {
                    id: r.id,
                    source,
                    model: r.model.filter(|m| !m.trim().is_empty()),
                    port,
                })
            })
            .collect();
        let b = &sec.base;
        let base = match b.mode.as_str() {
            "survey" => BaseMode::Survey {
                min_dur_s: b.min_dur_s.max(1),
                accuracy_m: b.accuracy_m,
            },
            "fixed" => match (b.lat_deg, b.lon_deg, b.h_ellipsoid_m) {
                (Some(lat_deg), Some(lon_deg), Some(h_ellipsoid_m)) => BaseMode::Fixed(Llh {
                    lat_deg,
                    lon_deg,
                    h_ellipsoid_m,
                }),
                _ => {
                    tracing::warn!("gnss_base_fixed_without_position");
                    BaseMode::Off
                }
            },
            _ => BaseMode::Off,
        };
        Self {
            receivers,
            pps_device: sec
                .pps_device
                .filter(|p| !p.trim().is_empty())
                .map(PathBuf::from),
            base,
            survey_min_dur_s: b.min_dur_s.max(1),
            survey_accuracy_m: b.accuracy_m,
            heading_enabled: sec.heading.enabled,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_slice_conforms() {
        ados_config::schema::assert_slice_conforms::<GnssSection>("/gnss");
    }

    #[test]
    fn sources_parse() {
        assert_eq!(Source::parse("hub"), Some(Source::Hub));
        assert_eq!(Source::parse("udp:7800"), Some(Source::Udp(7800)));
        assert_eq!(
            Source::parse("serial:/dev/ttyUSB0@115200"),
            Some(Source::Serial {
                path: "/dev/ttyUSB0".into(),
                baud: 115_200
            })
        );
        assert_eq!(
            Source::parse("serial:/dev/ttyACM0"),
            Some(Source::Serial {
                path: "/dev/ttyACM0".into(),
                baud: DEFAULT_BAUD
            })
        );
        assert_eq!(Source::parse("udp:0"), None);
        assert_eq!(Source::parse("tcp:1"), None);
        assert_eq!(Source::Udp(7800).label(), "udp:7800");
    }

    #[test]
    fn section_maps_to_config() {
        let sec: GnssSection = serde_json::from_value(serde_json::json!({
            "receivers": [{"id": "gnss0", "source": "udp:7800", "model": null},
                          {"id": "gnss1", "source": "bogus"}],
            "pps_device": "/dev/pps0",
            "base": {"mode": "survey", "min_dur_s": 60, "accuracy_m": 1.5},
            "heading": {"enabled": true}
        }))
        .unwrap();
        let c = GnssConfig::from_section(sec);
        assert_eq!(c.receivers.len(), 1);
        assert_eq!(c.receivers[0].port, RxPort::Uart1);
        assert_eq!(c.pps_device, Some(PathBuf::from("/dev/pps0")));
        assert_eq!(
            c.base,
            BaseMode::Survey {
                min_dur_s: 60,
                accuracy_m: 1.5
            }
        );
        assert!(c.heading_enabled);
        let empty = GnssConfig::from_section(GnssSection::default());
        assert!(empty.receivers.is_empty());
        assert_eq!(empty.base, BaseMode::Off);
    }
}
