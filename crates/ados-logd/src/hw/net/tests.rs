use super::*;
use std::fs;

fn write_iface(root: &Path, name: &str, counters: &[(&str, &str)]) {
    let dir = root.join(format!("sys/class/net/{name}/statistics"));
    fs::create_dir_all(&dir).unwrap();
    for (file, body) in counters {
        fs::write(dir.join(file), body).unwrap();
    }
}

#[test]
fn reads_all_counters_for_an_interface() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write_iface(
        root,
        "wlan0",
        &[
            ("rx_bytes", "100000\n"),
            ("tx_bytes", "200000\n"),
            ("rx_packets", "1000\n"),
            ("tx_packets", "2000\n"),
            ("rx_dropped", "3\n"),
            ("tx_dropped", "4\n"),
            ("rx_errors", "5\n"),
            ("tx_errors", "6\n"),
        ],
    );
    let stats = read_iface_stats(root);
    assert_eq!(stats.len(), 1);
    let s = &stats[0];
    assert_eq!(s.name, "wlan0");
    assert_eq!(s.rx_bytes, 100_000);
    assert_eq!(s.tx_bytes, 200_000);
    assert_eq!(s.rx_pkts, 1000);
    assert_eq!(s.tx_pkts, 2000);
    assert_eq!(s.rx_drop, 3);
    assert_eq!(s.tx_drop, 4);
    assert_eq!(s.rx_err, 5);
    assert_eq!(s.tx_err, 6);
}

#[test]
fn loopback_is_skipped_and_partial_counters_default_to_zero() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write_iface(root, "lo", &[("rx_bytes", "999\n"), ("tx_bytes", "999\n")]);
    // eth0 exposes only rx_bytes; the rest default to zero.
    write_iface(root, "eth0", &[("rx_bytes", "42\n")]);
    let stats = read_iface_stats(root);
    assert_eq!(stats.len(), 1, "lo must be skipped");
    assert_eq!(stats[0].name, "eth0");
    assert_eq!(stats[0].rx_bytes, 42);
    assert_eq!(stats[0].tx_bytes, 0);
    assert_eq!(stats[0].tx_pkts, 0);
}

#[test]
fn iface_without_a_statistics_dir_is_skipped() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    // A net entry that has no statistics subtree.
    fs::create_dir_all(root.join("sys/class/net/dummy0")).unwrap();
    assert!(read_iface_stats(root).is_empty());
}

#[test]
fn empty_root_yields_no_ifaces() {
    let dir = tempfile::tempdir().unwrap();
    assert!(read_iface_stats(dir.path()).is_empty());
}
