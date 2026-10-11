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
mod tests;
