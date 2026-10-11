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
