//! Which serial port is the rig's scanning LiDAR, so flight-controller
//! discovery never claims it.
//!
//! A Slamtec RPLIDAR enumerates behind a CP2102 USB-UART bridge
//! ([`LIDAR_USB_ID`]), the bridge many telemetry radios and FC adapters use
//! too, so the USB id alone cannot tell the LiDAR from a flight controller. A
//! port is the LiDAR when:
//!
//! * it is the serial device the capture service drives: the World Engine
//!   plugin's `atlas.lidar` config, `rplidar.device`, when that is a path; or
//! * that device is `auto` (or unset), the rig (`rig.json`) declares a sensor
//!   of kind `lidar`, the port is a CP2102, and it is either held exclusively
//!   by another process (the capture service) or answers the RPLIDAR
//!   `GET_INFO` request ([`probe_rplidar`]).
//!
//! A CP2102 identified neither way stays a flight-controller candidate.

use std::path::{Path, PathBuf};

mod probe;
pub use probe::{
    contains_info_descriptor, probe_rplidar, RplidarProbe, INFO_DESCRIPTOR, PROBE_TIMEOUT,
};

#[cfg(test)]
mod tests;

/// The CP2102 USB-UART bridge an RPLIDAR enumerates behind.
pub const LIDAR_USB_ID: (u16, u16) = (0x10C4, 0xEA60);
/// The plugin whose capture service drives the LiDAR.
pub const LIDAR_PLUGIN_ID: &str = "com.altnautica.world-engine";
/// Its config key holding the LiDAR driver settings.
pub const LIDAR_CONFIG_KEY: &str = "atlas.lidar";
/// The device value that selects USB discovery (also the default).
const AUTO: &str = "auto";
/// Prefix of a network (simulated) LiDAR device, never a serial port.
const TCP_PREFIX: &str = "tcp:";
/// Largest plugin config file read; the plugin host refuses larger ones too.
const PLUGIN_CONFIG_MAX_BYTES: u64 = 16 * 1024 * 1024;

/// How this node's serial LiDAR port is identified.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum LidarPort {
    /// No serial LiDAR: no port is the LiDAR.
    #[default]
    None,
    /// The configured serial device (any spelling resolving to it matches).
    Device(String),
    /// A declared LiDAR found by USB discovery: a CP2102 port the probe
    /// confirms.
    Auto,
}

impl LidarPort {
    /// The rule from the agent's own files: [`rig_path`] and
    /// [`plugin_config_path`].
    pub fn load() -> Self {
        Self::load_from(&rig_path(), &plugin_config_path())
    }

    /// The rule from a `rig.json` and the plugin host's config store. A
    /// missing or unreadable file reads as absent.
    pub fn load_from(rig_json: &Path, plugin_config: &Path) -> Self {
        let device = configured_device(plugin_config);
        Self::from_parts(device.as_deref(), rig_declares_lidar(rig_json))
    }

    /// The rule from the configured `rplidar.device` (`None` = unset, which
    /// the capture service reads as `auto`) and whether the rig declares a
    /// LiDAR. An empty device runs no driver, but a declared LiDAR is still
    /// on the bus, so it reads like `auto`.
    pub fn from_parts(device: Option<&str>, rig_declares_lidar: bool) -> Self {
        match device.map(str::trim).unwrap_or(AUTO) {
            "" | AUTO if rig_declares_lidar => Self::Auto,
            "" | AUTO => Self::None,
            d if d.starts_with(TCP_PREFIX) => Self::None,
            d => Self::Device(d.to_string()),
        }
    }

    /// Whether `port` (USB id `usb`, if any) is the LiDAR. `confirm` is asked
    /// only for a CP2102 under [`LidarPort::Auto`]; pass a closure over
    /// [`probe_rplidar`] in production.
    pub fn identifies(
        &self,
        port: &str,
        usb: Option<(u16, u16)>,
        confirm: impl FnOnce(&str) -> bool,
    ) -> bool {
        match self {
            Self::None => false,
            Self::Device(device) => same_device(port, device),
            Self::Auto => usb == Some(LIDAR_USB_ID) && confirm(port),
        }
    }
}

/// The agent config directory (`ADOS_ETC_DIR`, default `/etc/ados`).
fn etc_dir() -> PathBuf {
    std::env::var_os("ADOS_ETC_DIR").map_or_else(|| PathBuf::from("/etc/ados"), PathBuf::from)
}

/// The rig description, `rig.json` in the agent config directory.
pub fn rig_path() -> PathBuf {
    etc_dir().join("rig.json")
}

/// The plugin host's persisted config store (`ADOS_PLUGIN_CONFIG_PATH`,
/// default `/etc/ados/plugin-config.json`, as the plugin host resolves it).
pub fn plugin_config_path() -> PathBuf {
    std::env::var_os("ADOS_PLUGIN_CONFIG_PATH").map_or_else(
        || PathBuf::from("/etc/ados/plugin-config.json"),
        PathBuf::from,
    )
}

/// Whether two device paths name the same node: literally, or after
/// resolving symlinks (`/dev/serial/by-id/...` to `/dev/ttyUSB0`).
pub(crate) fn same_device(a: &str, b: &str) -> bool {
    if a == b {
        return true;
    }
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(ca), Ok(cb)) => ca == cb,
        _ => false,
    }
}

/// Whether the rig declares a sensor of kind `lidar`.
fn rig_declares_lidar(rig_json: &Path) -> bool {
    let Ok(text) = std::fs::read_to_string(rig_json) else {
        return false;
    };
    let Ok(rig) = serde_json::from_str::<serde_json::Value>(&text) else {
        return false;
    };
    rig["sensors"]
        .as_array()
        .is_some_and(|s| s.iter().any(|s| s["kind"] == "lidar"))
}

/// One persisted plugin config record: the value is base64 of its msgpack
/// encoding; `agent_id` is set on a per-drone record, absent on a global one.
#[derive(serde::Deserialize)]
struct ConfigRecord {
    plugin_id: String,
    key: String,
    #[serde(default)]
    agent_id: Option<String>,
    value: String,
}

/// The configured `rplidar.device`, or `None` when the LiDAR config or the
/// field is absent. A per-drone record wins over the global one, as the
/// plugin host resolves a read.
fn configured_device(plugin_config: &Path) -> Option<String> {
    let len = std::fs::metadata(plugin_config).ok()?.len();
    if len > PLUGIN_CONFIG_MAX_BYTES {
        return None;
    }
    let text = std::fs::read_to_string(plugin_config).ok()?;
    let records: Vec<ConfigRecord> = serde_json::from_str(&text).ok()?;
    let (mut drone, mut global) = (None, None);
    for r in records {
        if r.plugin_id != LIDAR_PLUGIN_ID || r.key != LIDAR_CONFIG_KEY {
            continue;
        }
        let slot = if r.agent_id.is_some() {
            &mut drone
        } else {
            &mut global
        };
        slot.get_or_insert(r.value);
    }
    let value = drone
        .as_deref()
        .and_then(decode_value)
        .or_else(|| global.as_deref().and_then(decode_value))?;
    rplidar_device(&value)
}

fn decode_value(encoded: &str) -> Option<rmpv::Value> {
    use base64::Engine;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .ok()?;
    rmpv::decode::read_value(&mut bytes.as_slice()).ok()
}

/// `value.rplidar.device` of an `atlas.lidar` map.
fn rplidar_device(value: &rmpv::Value) -> Option<String> {
    let field = |v: &rmpv::Value, name: &str| -> Option<rmpv::Value> {
        v.as_map()?
            .iter()
            .find(|(k, _)| k.as_str() == Some(name))
            .map(|(_, v)| v.clone())
    };
    let rplidar = field(value, "rplidar")?;
    field(&rplidar, "device")?.as_str().map(str::to_string)
}
