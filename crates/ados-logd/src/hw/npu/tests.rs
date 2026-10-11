use super::*;
use std::fs;

#[test]
fn parses_a_tri_core_line() {
    let s = "NPU load:  Core0: 15%, Core1:  0%, Core2: 30%,\n";
    // (15 + 0 + 30) / 3 = 15
    assert_eq!(parse_rknpu_load(s), Some(15.0));
}

#[test]
fn parses_a_single_core_line() {
    assert_eq!(parse_rknpu_load("NPU load:  43%,\n"), Some(43.0));
}

#[test]
fn clamps_and_averages() {
    // Two cores, one pegged.
    assert_eq!(parse_rknpu_load("Core0: 100%, Core1: 0%,"), Some(50.0));
}

#[test]
fn a_line_with_no_percent_is_none() {
    assert_eq!(parse_rknpu_load("NPU load: unknown\n"), None);
    assert_eq!(parse_rknpu_load(""), None);
}

#[test]
fn reads_load_under_an_injected_root() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let p = root.join("sys/kernel/debug/rknpu/load");
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(&p, "NPU load:  Core0: 20%, Core1: 40%,\n").unwrap();
    assert_eq!(read_npu_load_pct(root), Some(30.0));
}

#[test]
fn absent_file_is_none_not_zero() {
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(read_npu_load_pct(dir.path()), None);
}
