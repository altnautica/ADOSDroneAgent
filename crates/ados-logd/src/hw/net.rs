//! Per-interface network counter reader.
//!
//! Each interface under `/sys/class/net/*` exposes cumulative counters under
//! `statistics/`: `rx_bytes`, `tx_bytes`, `rx_packets`, `tx_packets`,
//! `rx_dropped`, `tx_dropped`, `rx_errors`, `tx_errors`. They are monotonic
//! counters; the rate (bytes/s, packets/s) is derived at read time from
//! successive snapshots, so the raw cumulative value is what is recorded.
//!
//! An interface that disappears (a USB NIC unplugged) simply drops out of the
//! next snapshot; one that appears is picked up automatically because the
//! directory is re-enumerated each tick.

use std::path::Path;

use super::reader::{list_dir, read_u64, under};

/// Cumulative counters for one network interface.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IfaceStats {
    /// Interface name (e.g. `wlan0`, `eth0`).
    pub name: String,
    /// Cumulative received bytes.
    pub rx_bytes: u64,
    /// Cumulative transmitted bytes.
    pub tx_bytes: u64,
    /// Cumulative received packets.
    pub rx_pkts: u64,
    /// Cumulative transmitted packets.
    pub tx_pkts: u64,
    /// Cumulative received-packet drops.
    pub rx_drop: u64,
    /// Cumulative transmitted-packet drops.
    pub tx_drop: u64,
    /// Cumulative receive errors.
    pub rx_err: u64,
    /// Cumulative transmit errors.
    pub tx_err: u64,
}

/// Read the statistics counters for every interface under `/sys/class/net`. The
/// loopback interface `lo` is skipped (it carries no diagnostic signal). An
/// interface whose `statistics` directory is missing is skipped.
pub fn read_iface_stats(root: &Path) -> Vec<IfaceStats> {
    let base = under(root, "/sys/class/net");
    let mut out = Vec::new();
    for name in list_dir(&base) {
        if name == "lo" {
            continue;
        }
        let stats_dir = base.join(&name).join("statistics");
        // Require at least one readable counter; an iface with no statistics
        // directory at all is skipped.
        let rx_bytes = read_u64(&stats_dir.join("rx_bytes"));
        let tx_bytes = read_u64(&stats_dir.join("tx_bytes"));
        if rx_bytes.is_none() && tx_bytes.is_none() {
            continue;
        }
        out.push(IfaceStats {
            name: name.clone(),
            rx_bytes: rx_bytes.unwrap_or(0),
            tx_bytes: tx_bytes.unwrap_or(0),
            rx_pkts: read_u64(&stats_dir.join("rx_packets")).unwrap_or(0),
            tx_pkts: read_u64(&stats_dir.join("tx_packets")).unwrap_or(0),
            rx_drop: read_u64(&stats_dir.join("rx_dropped")).unwrap_or(0),
            tx_drop: read_u64(&stats_dir.join("tx_dropped")).unwrap_or(0),
            rx_err: read_u64(&stats_dir.join("rx_errors")).unwrap_or(0),
            tx_err: read_u64(&stats_dir.join("tx_errors")).unwrap_or(0),
        });
    }
    out
}

#[cfg(test)]
mod tests {
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
}
