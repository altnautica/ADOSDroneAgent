use super::*;
use std::fs;

fn write(root: &Path, rel: &str, body: &str) {
    let p = root.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, body).unwrap();
}

#[test]
fn reads_voltage_current_and_power_with_labels() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "sys/class/hwmon/hwmon2/name", "ina226\n");
    write(root, "sys/class/hwmon/hwmon2/in1_input", "5012\n");
    write(root, "sys/class/hwmon/hwmon2/in1_label", "VBUS\n");
    write(root, "sys/class/hwmon/hwmon2/curr1_input", "1234\n");
    write(root, "sys/class/hwmon/hwmon2/curr1_label", "IBUS\n");
    write(root, "sys/class/hwmon/hwmon2/power1_input", "6184000\n");
    // power1 has no label -> id fallback.

    let rails = read_power_rails(root);
    assert_eq!(rails.len(), 3);

    let v = rails.iter().find(|r| r.kind == RailKind::Voltage).unwrap();
    assert_eq!(v.chip, "ina226");
    assert_eq!(v.label, "VBUS");
    assert!((v.value - 5012.0).abs() < 0.01);

    let c = rails.iter().find(|r| r.kind == RailKind::Current).unwrap();
    assert_eq!(c.label, "IBUS");
    assert!((c.value - 1234.0).abs() < 0.01);

    let p = rails.iter().find(|r| r.kind == RailKind::Power).unwrap();
    assert_eq!(p.label, "power1");
    assert!((p.value - 6_184_000.0).abs() < 1.0);
}

#[test]
fn a_temp_only_hwmon_yields_no_rails() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "sys/class/hwmon/hwmon0/name", "cpu_thermal\n");
    write(root, "sys/class/hwmon/hwmon0/temp1_input", "47000\n");
    assert!(read_power_rails(root).is_empty());
}

#[test]
fn empty_root_yields_no_rails() {
    let dir = tempfile::tempdir().unwrap();
    assert!(read_power_rails(dir.path()).is_empty());
}
