use super::*;
use std::fs;

fn write(root: &Path, rel: &str, body: &str) {
    let p = root.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, body).unwrap();
}

#[test]
fn parses_meminfo_in_bytes() {
    let text = "\
MemTotal:        8123456 kB
MemFree:          512000 kB
MemAvailable:    6543210 kB
Buffers:           12345 kB
Cached:           54321 kB
SwapTotal:       1000000 kB
SwapFree:         999000 kB
";
    let info = parse_meminfo(text);
    assert_eq!(info.total, Some(8_123_456 * 1024));
    assert_eq!(info.available, Some(6_543_210 * 1024));
    assert_eq!(info.swap_free, Some(999_000 * 1024));
    assert_eq!(info.swap_total, Some(1_000_000 * 1024));
    // cache = Buffers + Cached.
    assert_eq!(info.cache, Some((12_345 + 54_321) * 1024));
}

#[test]
fn read_meminfo_from_a_fixture_root() {
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        "proc/meminfo",
        "MemTotal: 1024 kB\nMemAvailable: 512 kB\n",
    );
    let info = read_meminfo(dir.path());
    assert_eq!(info.total, Some(1024 * 1024));
    assert_eq!(info.available, Some(512 * 1024));
    assert_eq!(info.swap_free, None);
}

#[test]
fn read_meminfo_is_default_for_a_missing_file() {
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(read_meminfo(dir.path()), MemInfo::default());
}

#[test]
fn parses_psi_some_line() {
    let text = "\
some avg10=1.23 avg60=4.56 avg300=7.89 total=123456789
full avg10=0.10 avg60=0.20 avg300=0.30 total=42
";
    let p = parse_pressure_some(text).unwrap();
    assert!((p.avg10 - 1.23).abs() < 0.0001);
    assert!((p.avg60 - 4.56).abs() < 0.0001);
    assert!((p.avg300 - 7.89).abs() < 0.0001);
    assert_eq!(p.total_us, 123_456_789);
}

#[test]
fn reads_psi_for_each_resource_from_a_fixture_root() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        root,
        "proc/pressure/cpu",
        "some avg10=0.50 avg60=0.40 avg300=0.30 total=1000\n",
    );
    write(
            root,
            "proc/pressure/memory",
            "some avg10=2.00 avg60=1.00 avg300=0.50 total=2000\nfull avg10=1.0 avg60=0.5 avg300=0.2 total=900\n",
        );
    assert_eq!(read_pressure(root, "cpu").unwrap().total_us, 1000);
    assert_eq!(read_pressure(root, "memory").unwrap().total_us, 2000);
    // io is absent in this fixture.
    assert!(read_pressure(root, "io").is_none());
}

#[test]
fn psi_is_none_on_a_kernel_without_the_interface() {
    // No /proc/pressure at all (CONFIG_PSI off).
    let dir = tempfile::tempdir().unwrap();
    assert!(read_pressure(dir.path(), "cpu").is_none());
    // A file with only a `full` line (no `some`) also yields None.
    assert!(parse_pressure_some("full avg10=0.0 total=0\n").is_none());
}
