use super::*;
use std::fs;

#[test]
fn parses_loadavg_three_fields() {
    let la = parse_loadavg("0.52 0.40 0.31 2/512 12345\n").unwrap();
    assert!((la.one - 0.52).abs() < 1e-9);
    assert!((la.five - 0.40).abs() < 1e-9);
    assert!((la.fifteen - 0.31).abs() < 1e-9);
}

#[test]
fn loadavg_none_on_garbage() {
    assert!(parse_loadavg("").is_none());
    assert!(parse_loadavg("x y z\n").is_none());
}

#[test]
fn read_loadavg_is_none_for_a_missing_file() {
    let dir = tempfile::tempdir().unwrap();
    assert!(read_loadavg(dir.path()).is_none());
}

#[test]
fn parses_page_fault_counters() {
    let text = "\
nr_free_pages 123456
pgfault 9876543
pgmajfault 1234
pgsteal_kswapd 0
";
    let vm = parse_vmstat(text);
    assert_eq!(vm.pgfault, Some(9_876_543));
    assert_eq!(vm.pgmajfault, Some(1234));
}

#[test]
fn read_vmstat_from_a_fixture_root() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join("proc")).unwrap();
    fs::write(
        dir.path().join("proc/vmstat"),
        "pgfault 100\npgmajfault 5\n",
    )
    .unwrap();
    let vm = read_vmstat(dir.path());
    assert_eq!(vm.pgfault, Some(100));
    assert_eq!(vm.pgmajfault, Some(5));
}

#[test]
fn read_vmstat_is_default_for_a_missing_file() {
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(read_vmstat(dir.path()), VmStat::default());
}

#[test]
fn missing_keys_stay_none() {
    let vm = parse_vmstat("nr_free_pages 1\npgfault 2\n");
    assert_eq!(vm.pgfault, Some(2));
    assert_eq!(vm.pgmajfault, None);
}
