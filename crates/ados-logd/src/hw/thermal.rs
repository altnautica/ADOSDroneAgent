//! Thermal-zone and hwmon temperature readers.
//!
//! Two sources of board temperature:
//!
//! - `/sys/class/thermal/thermal_zone*` — each zone exposes a `type` (a short
//!   name such as `soc-thermal` / `cpu-thermal` / `gpu-thermal`) and a `temp` in
//!   millidegrees Celsius.
//! - `/sys/class/hwmon/hwmon*` — chips that expose `temp*_input` (millidegrees)
//!   often alongside a `temp*_label` and a chip `name`.
//!
//! Both readers take an injectable root and skip any node that is absent or
//! unreadable, so a board with no thermal zones or no hwmon temps simply yields
//! an empty list for that source.

use std::path::Path;

use super::reader::{list_dir, read_i64, read_trimmed, under};

/// One thermal zone reading: the zone name and its temperature in Celsius.
#[derive(Debug, Clone, PartialEq)]
pub struct ZoneTemp {
    /// The zone `type` string (e.g. `soc-thermal`).
    pub name: String,
    /// Temperature in degrees Celsius.
    pub c: f32,
}

/// One hwmon temperature channel: the chip name, the channel label, and the
/// temperature in Celsius.
#[derive(Debug, Clone, PartialEq)]
pub struct HwmonTemp {
    /// The hwmon chip `name` (e.g. `cpu_thermal`).
    pub chip: String,
    /// The per-channel `temp*_label`, or the channel id when no label exists.
    pub label: String,
    /// Temperature in degrees Celsius.
    pub c: f32,
}

/// Read every `/sys/class/thermal/thermal_zone*` zone, returning the name + the
/// temperature in Celsius. Zones with no readable `temp` are skipped.
pub fn read_thermal_zones(root: &Path) -> Vec<ZoneTemp> {
    let base = under(root, "/sys/class/thermal");
    let mut out = Vec::new();
    for entry in list_dir(&base) {
        if !entry.starts_with("thermal_zone") {
            continue;
        }
        let zdir = base.join(&entry);
        let Some(milli) = read_i64(&zdir.join("temp")) else {
            continue;
        };
        // The zone `type` is the human name; fall back to the directory name so a
        // zone with a missing `type` is still attributable.
        let name = read_trimmed(&zdir.join("type")).unwrap_or_else(|| entry.clone());
        out.push(ZoneTemp {
            name,
            c: milli as f32 / 1000.0,
        });
    }
    out
}

/// Read every `temp*_input` channel under each `/sys/class/hwmon/hwmon*` chip,
/// returning the chip name, the channel label, and the temperature in Celsius.
/// Channels with no readable input are skipped.
pub fn read_hwmon_temps(root: &Path) -> Vec<HwmonTemp> {
    let base = under(root, "/sys/class/hwmon");
    let mut out = Vec::new();
    for entry in list_dir(&base) {
        let cdir = base.join(&entry);
        let chip = read_trimmed(&cdir.join("name")).unwrap_or_else(|| entry.clone());
        for file in list_dir(&cdir) {
            // Match `tempN_input`, capturing the channel id `N`.
            let Some(id) = channel_id(&file, "temp", "_input") else {
                continue;
            };
            let Some(milli) = read_i64(&cdir.join(&file)) else {
                continue;
            };
            let label = read_trimmed(&cdir.join(format!("temp{id}_label")))
                .unwrap_or_else(|| format!("temp{id}"));
            out.push(HwmonTemp {
                chip: chip.clone(),
                label,
                c: milli as f32 / 1000.0,
            });
        }
    }
    out
}

/// Extract the numeric channel id from an hwmon file name of the form
/// `<prefix>N<suffix>` (e.g. `temp1_input` with prefix `temp` and suffix
/// `_input` yields `1`). Returns `None` when the name does not match or the
/// middle is not all digits.
pub fn channel_id<'a>(file: &'a str, prefix: &str, suffix: &str) -> Option<&'a str> {
    let mid = file.strip_prefix(prefix)?.strip_suffix(suffix)?;
    if !mid.is_empty() && mid.bytes().all(|b| b.is_ascii_digit()) {
        Some(mid)
    } else {
        None
    }
}

#[cfg(test)]
mod tests;
