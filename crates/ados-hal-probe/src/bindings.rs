//! Optional sensor bindings a board profile may declare: the IMU source, the
//! GNSS source, a USB sensor hub and a host-I2C battery gauge.
//!
//! Same shape as the Python `ImuBinding`, `GnssBinding`, `SensorHubBinding` and
//! `BatteryGaugeBinding` models in `src/ados/hal/_detect_bindings.py`; both sides
//! parse the same board-profile YAMLs.

use serde::{Deserialize, Serialize};

/// The USB VID:PID the sensor-hub firmware enumerates with (CDC-ACM).
pub const SENSOR_HUB_VID_PID: &str = "1209:ad05";

/// Bus the board's primary IMU is read over.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ImuBus {
    Spi,
    I2c,
    Hub,
    Fc,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImuBinding {
    pub bus: ImuBus,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub rate_hz: Option<i64>,
}

/// Where the GNSS receiver bytes come from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GnssTransport {
    Fc,
    Hub,
    Serial,
    Udp,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GnssBinding {
    pub transport: GnssTransport,
    #[serde(default)]
    pub device: Option<String>,
    #[serde(default)]
    pub baud: Option<i64>,
    #[serde(default)]
    pub pps_gpio: Option<i64>,
    #[serde(default)]
    pub model: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SensorHubBinding {
    #[serde(default = "default_hub_vid_pid")]
    pub usb_vid_pid: String,
    #[serde(default)]
    pub board: Option<String>,
}

fn default_hub_vid_pid() -> String {
    SENSOR_HUB_VID_PID.to_string()
}

impl Default for SensorHubBinding {
    fn default() -> Self {
        Self {
            usb_vid_pid: default_hub_vid_pid(),
            board: None,
        }
    }
}

/// Supported battery gauge chips.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GaugeType {
    Ina226,
}

/// A host-I2C battery gauge. Copied verbatim into `board.json` so the battery
/// service reads the binding without parsing board YAML.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BatteryGaugeBinding {
    #[serde(rename = "type")]
    pub kind: GaugeType,
    pub i2c_bus: i64,
    pub addr: i64,
    pub shunt_ohm: f64,
    pub max_current_a: f64,
}

/// Parse `"vvvv:pppp"` (hex, any case) into `(vid, pid)`.
pub fn parse_vid_pid(s: &str) -> Option<(u16, u16)> {
    let (v, p) = s.trim().split_once(':')?;
    Some((
        u16::from_str_radix(v.trim(), 16).ok()?,
        u16::from_str_radix(p.trim(), 16).ok()?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bindings_parse_the_yaml_shapes() {
        let imu: ImuBinding =
            serde_norway::from_str("bus: hub\nmodel: ICM-42688-P\nrate_hz: 1000\n").unwrap();
        assert_eq!(imu.bus, ImuBus::Hub);
        assert_eq!(imu.rate_hz, Some(1000));

        let gnss: GnssBinding = serde_norway::from_str(
            "transport: serial\ndevice: /dev/ttyS3\nbaud: 115200\npps_gpio: 18\n",
        )
        .unwrap();
        assert_eq!(gnss.transport, GnssTransport::Serial);
        assert_eq!(gnss.pps_gpio, Some(18));
        assert_eq!(gnss.model, None);

        let hub: SensorHubBinding = serde_norway::from_str("board: matek_h743_slim_v3\n").unwrap();
        assert_eq!(hub.usb_vid_pid, SENSOR_HUB_VID_PID);

        let gauge: BatteryGaugeBinding = serde_norway::from_str(
            "type: ina226\ni2c_bus: 1\naddr: 64\nshunt_ohm: 0.0005\nmax_current_a: 60.0\n",
        )
        .unwrap();
        assert_eq!(gauge.kind, GaugeType::Ina226);
        let v = serde_json::to_value(&gauge).unwrap();
        assert_eq!(v["type"], "ina226");
        assert_eq!(v["addr"], 64);
        assert_eq!(v.as_object().unwrap().len(), 5);
    }

    #[test]
    fn unknown_enum_values_are_rejected() {
        assert!(serde_norway::from_str::<ImuBinding>("bus: usb\n").is_err());
        assert!(serde_norway::from_str::<BatteryGaugeBinding>(
            "type: ina219\ni2c_bus: 1\naddr: 64\nshunt_ohm: 0.1\nmax_current_a: 3.2\n"
        )
        .is_err());
    }

    #[test]
    fn vid_pid_parses_hex_pairs() {
        assert_eq!(parse_vid_pid("1209:ad05"), Some((0x1209, 0xAD05)));
        assert_eq!(parse_vid_pid("1209:AD05\n"), Some((0x1209, 0xAD05)));
        assert_eq!(parse_vid_pid("1209"), None);
        assert_eq!(parse_vid_pid("zz:01"), None);
    }
}
