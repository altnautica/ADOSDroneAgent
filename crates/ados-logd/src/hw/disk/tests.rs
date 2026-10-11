use super::*;
use std::fs;

// A realistic /proc/diskstats covering an eMMC whole device + partition, an
// NVMe whole device + partition, an SD-style disk + partition, and a loop.
const SAMPLE: &str = "\
 179       0 mmcblk0 12000 100 480000 5000 8000 50 320000 4000 0 6000 9000
 179       1 mmcblk0p1 5000 0 200000 2000 3000 0 120000 1500 0 3000 3500
 259       0 nvme0n1 90000 0 7200000 30000 60000 0 4800000 20000 0 40000 50000
 259       1 nvme0n1p1 1000 0 8000 100 500 0 4000 50 0 120 130
   8       0 sda 4000 0 160000 1800 2000 0 80000 900 0 2200 2500
   8       1 sda1 100 0 800 10 50 0 400 5 0 12 13
   7       0 loop0 0 0 0 0 0 0 0 0 0 0 0
";

#[test]
fn keeps_backing_devices_and_drops_partitions_and_loops() {
    let stats = parse_diskstats(SAMPLE);
    let names: Vec<&str> = stats.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, vec!["mmcblk0", "nvme0n1", "sda"]);
}

#[test]
fn parses_the_right_counter_fields() {
    let stats = parse_diskstats(SAMPLE);
    let emmc = stats.iter().find(|s| s.name == "mmcblk0").unwrap();
    // field 5 (zero-based) = sectors read = 480000.
    assert_eq!(emmc.rd_sectors, 480_000);
    // field 9 = sectors written = 320000.
    assert_eq!(emmc.wr_sectors, 320_000);
    // field 12 = io_ms = 6000.
    assert_eq!(emmc.io_ms, 6000);
}

#[test]
fn read_diskstats_from_a_fixture_root() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join("proc")).unwrap();
    fs::write(dir.path().join("proc/diskstats"), SAMPLE).unwrap();
    let stats = read_diskstats(dir.path());
    assert_eq!(stats.len(), 3);
}

#[test]
fn read_diskstats_is_empty_for_a_missing_file() {
    let dir = tempfile::tempdir().unwrap();
    assert!(read_diskstats(dir.path()).is_empty());
}

#[test]
fn short_lines_are_skipped() {
    // A truncated line (kernel without the extended fields) is dropped.
    assert!(parse_diskstats(" 8 0 sda 1 2 3\n").is_empty());
}
