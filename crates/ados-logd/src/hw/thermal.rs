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
mod tests {
    use super::*;
    use std::fs;

    fn write(root: &Path, rel: &str, body: &str) {
        let p = root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, body).unwrap();
    }

    #[test]
    fn reads_thermal_zones_with_names_and_celsius() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(
            root,
            "sys/class/thermal/thermal_zone0/type",
            "soc-thermal\n",
        );
        write(root, "sys/class/thermal/thermal_zone0/temp", "45200\n");
        write(
            root,
            "sys/class/thermal/thermal_zone1/type",
            "gpu-thermal\n",
        );
        write(root, "sys/class/thermal/thermal_zone1/temp", "39000\n");
        // A non-zone dir under thermal/ (a cooling_device) must be ignored.
        write(root, "sys/class/thermal/cooling_device0/type", "fan\n");

        let zones = read_thermal_zones(root);
        assert_eq!(zones.len(), 2);
        assert_eq!(zones[0].name, "soc-thermal");
        assert!((zones[0].c - 45.2).abs() < 0.01);
        assert_eq!(zones[1].name, "gpu-thermal");
        assert!((zones[1].c - 39.0).abs() < 0.01);
    }

    #[test]
    fn thermal_zone_with_missing_type_falls_back_to_the_dir_name() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "sys/class/thermal/thermal_zone0/temp", "50000\n");
        let zones = read_thermal_zones(root);
        assert_eq!(zones.len(), 1);
        assert_eq!(zones[0].name, "thermal_zone0");
    }

    #[test]
    fn thermal_zone_without_temp_is_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "sys/class/thermal/thermal_zone0/type", "soc\n");
        // No temp file.
        assert!(read_thermal_zones(root).is_empty());
    }

    #[test]
    fn empty_root_yields_no_zones_and_no_hwmon() {
        let dir = tempfile::tempdir().unwrap();
        assert!(read_thermal_zones(dir.path()).is_empty());
        assert!(read_hwmon_temps(dir.path()).is_empty());
    }

    #[test]
    fn reads_hwmon_temps_with_labels() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "sys/class/hwmon/hwmon0/name", "cpu_thermal\n");
        write(root, "sys/class/hwmon/hwmon0/temp1_input", "47000\n");
        write(root, "sys/class/hwmon/hwmon0/temp1_label", "CPU\n");
        write(root, "sys/class/hwmon/hwmon0/temp2_input", "52000\n");
        // temp2 has no label -> falls back to the channel id.

        let temps = read_hwmon_temps(root);
        assert_eq!(temps.len(), 2);
        assert_eq!(temps[0].chip, "cpu_thermal");
        assert_eq!(temps[0].label, "CPU");
        assert!((temps[0].c - 47.0).abs() < 0.01);
        assert_eq!(temps[1].label, "temp2");
        assert!((temps[1].c - 52.0).abs() < 0.01);
    }

    #[test]
    fn hwmon_chip_without_name_falls_back_to_the_dir_name() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "sys/class/hwmon/hwmon3/temp1_input", "40000\n");
        let temps = read_hwmon_temps(root);
        assert_eq!(temps.len(), 1);
        assert_eq!(temps[0].chip, "hwmon3");
    }

    #[test]
    fn channel_id_matches_only_well_formed_names() {
        assert_eq!(channel_id("temp1_input", "temp", "_input"), Some("1"));
        assert_eq!(channel_id("temp12_input", "temp", "_input"), Some("12"));
        assert_eq!(channel_id("in0_input", "in", "_input"), Some("0"));
        assert_eq!(channel_id("temp1_label", "temp", "_input"), None);
        assert_eq!(channel_id("temp_input", "temp", "_input"), None);
        assert_eq!(channel_id("tempX_input", "temp", "_input"), None);
    }
}
