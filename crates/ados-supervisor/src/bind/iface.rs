//! Bind-tunnel interface helpers.
//!
//! Two thin wrappers over the kernel:
//!   - [`wait_for_iface`] polls `ip -4 addr show dev <iface>` until the L3 bind
//!     tunnel TUN device the wfb-ng bind profile creates has an address.
//!   - [`read_rx_packets_counter`] reads
//!     `/sys/class/net/<iface>/statistics/rx_packets`. A monotonic increment
//!     proves the peer transmitted a frame that passed FEC + decryption inside
//!     `wfb_rx` and was handed to the TUN device — a kernel integer immune to
//!     wfb-ng log-format churn (Rule 37: trust `/sys/class/net` counters).

use std::path::Path;
use std::time::Duration;

use tokio::process::Command;
use tokio::time::{sleep, Instant};

/// Poll `ip -4 addr show dev <iface>` every [`super::TUNNEL_POLL_INTERVAL`]
/// until it exits 0 (the tunnel iface exists with an inet addr) or `timeout`
/// elapses. Mirrors `_wait_for_iface`.
pub async fn wait_for_iface(iface: &str, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        let ok = Command::new("ip")
            .args(["-4", "addr", "show", "dev", iface])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .await
            .map(|s| s.success())
            .unwrap_or(false);
        if ok {
            return true;
        }
        sleep(super::TUNNEL_POLL_INTERVAL).await;
    }
    false
}

/// Read `/sys/class/net/<iface>/statistics/rx_packets` as a counter. `None` if
/// the iface is absent / torn down / the value is unparseable — the caller
/// treats "no signal" and "iface missing" uniformly. Mirrors
/// `_read_rx_packets_counter`.
pub fn read_rx_packets_counter(iface: &str) -> Option<u64> {
    let path = format!("/sys/class/net/{iface}/statistics/rx_packets");
    std::fs::read_to_string(Path::new(&path))
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_iface_counter_is_none() {
        // A guaranteed-absent iface name.
        assert!(read_rx_packets_counter("ados-no-such-iface-xyz").is_none());
    }

    #[tokio::test]
    async fn wait_returns_false_quickly_for_absent_iface() {
        // 0-budget: the loop body runs zero times → immediate false, no spawn.
        let got = wait_for_iface("ados-no-such-iface-xyz", Duration::from_millis(0)).await;
        assert!(!got);
    }
}
