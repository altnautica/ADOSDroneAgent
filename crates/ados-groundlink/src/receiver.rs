//! WFB receiver: FEC-combine fragments from the local NIC + remote relays.
//!
//! Ports `wfb_receiver.py`'s FEC supervision. `wfb_rx -p 0 -c 127.0.0.1 -u 5600
//! -a <listen_port> [<drone_iface>]` aggregates fragments arriving on the local
//! monitor adapter AND from relays forwarding over batman-adv into the
//! aggregator UDP port, FEC-combines them, and emits the decoded stream to
//! localhost UDP 5600 where the existing mediamtx-gs pipeline republishes it.
//! The stderr stats line drives `fragments_after_dedup` / `fec_repaired` /
//! `output_kbps`; `wfb-receiver.json` is written atomically.
//!
//! mDNS SEAM: the receiver advertises `_ados-receiver._tcp` on `bat0` so relays
//! can resolve it. That publication STAYS IN PYTHON (zeroconf). This Rust module
//! owns the aggregator subprocess lifecycle, the stats tail, and the state
//! file; Python owns the mDNS service registration alongside.

use std::path::Path;

use crate::process_spawn::GsWfbProcess;

/// The receiver's published state (the `wfb-receiver.json` shape, byte-identical
/// to the Python `_write_state`). Relays are flattened to a list on write.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ReceiverState {
    pub role: String,
    pub drone_iface: String,
    pub listen_port: i64,
    pub accept_local_nic: bool,
    pub mesh_iface: String,
    pub relays: Vec<RelayStats>,
    pub fragments_after_dedup: i64,
    pub fec_repaired: i64,
    pub output_kbps: i64,
    pub up: bool,
}

/// Per-relay fragment stats (one entry in the `relays` list).
#[derive(Debug, Clone, serde::Serialize)]
pub struct RelayStats {
    pub mac: String,
    pub last_seen_ms: i64,
    pub fragments: i64,
}

impl Default for ReceiverState {
    fn default() -> Self {
        Self {
            role: "receiver".to_string(),
            drone_iface: String::new(),
            listen_port: 5800,
            accept_local_nic: true,
            mesh_iface: "bat0".to_string(),
            relays: Vec::new(),
            fragments_after_dedup: 0,
            fec_repaired: 0,
            output_kbps: 0,
            up: false,
        }
    }
}

impl ReceiverState {
    /// Atomically write the state to `wfb-receiver.json` (Contract-E path).
    pub fn write(&self) -> std::io::Result<()> {
        let path = Path::new(crate::paths::WFB_RECEIVER_JSON);
        crate::sidecars::write_json_atomic(path, self, 0o644)
    }
}

/// Build the `wfb_rx -a` aggregator args. With `accept_local_nic` the local
/// monitor adapter is appended so its fragments are aggregated too; without it
/// the receiver trusts only relay forwards. Mirrors `_launch_wfb_rx_aggregate`.
pub fn aggregate_args(
    drone_iface: &str,
    listen_port: u16,
    accept_local_nic: bool,
    rx_key: &Path,
) -> Vec<String> {
    let mut args = vec![
        "-p".into(),
        "0".into(),
        "-c".into(),
        "127.0.0.1".into(),
        "-u".into(),
        "5600".into(),
        "-a".into(),
        listen_port.to_string(),
        "-K".into(),
        rx_key.to_string_lossy().into_owned(),
    ];
    if accept_local_nic {
        args.push(drone_iface.into());
    }
    args
}

/// Spawn the FEC-combine aggregator in its own process group (setsid/killpg).
/// stderr is piped so the stats tail can read the combined counters.
pub async fn spawn_aggregator(
    drone_iface: &str,
    listen_port: u16,
    accept_local_nic: bool,
) -> std::io::Result<GsWfbProcess> {
    let rx_key = Path::new(ados_radio::paths::WFB_RX_KEY);
    let args = aggregate_args(drone_iface, listen_port, accept_local_nic, rx_key);
    GsWfbProcess::spawn_stderr_piped("wfb_rx", &args).await
}

/// Parse one aggregator stderr line for the combined counters. Mirrors
/// `_tail_stats`: a line containing `n_out:` carries the post-dedup count,
/// `fec_rec:` the repaired count, `bitrate_kbps:` the output rate. Returns
/// `(after_dedup, fec_repaired, output_kbps)` updates when present.
pub fn parse_receiver_stats_line(line: &str) -> (Option<i64>, Option<i64>, Option<i64>) {
    if !line.contains("n_out:") {
        return (None, None, None);
    }
    let mut after_dedup = None;
    let mut fec_repaired = None;
    let mut output_kbps = None;
    for tok in line.split_whitespace() {
        if let Some(v) = tok.strip_prefix("n_out:") {
            after_dedup = v.parse::<i64>().ok();
        } else if let Some(v) = tok.strip_prefix("fec_rec:") {
            fec_repaired = v.parse::<i64>().ok();
        } else if let Some(v) = tok.strip_prefix("bitrate_kbps:") {
            output_kbps = v.parse::<i64>().ok();
        }
    }
    (after_dedup, fec_repaired, output_kbps)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aggregate_args_with_local_nic() {
        // wfb_rx -p 0 -c 127.0.0.1 -u 5600 -a 5800 -K <rx.key> <iface>
        let a = aggregate_args("wlan0", 5800, true, Path::new("/etc/ados/wfb/rx.key"));
        assert_eq!(
            a,
            vec![
                "-p",
                "0",
                "-c",
                "127.0.0.1",
                "-u",
                "5600",
                "-a",
                "5800",
                "-K",
                "/etc/ados/wfb/rx.key",
                "wlan0"
            ]
        );
    }

    #[test]
    fn aggregate_args_without_local_nic_drops_iface() {
        let a = aggregate_args("wlan0", 5800, false, Path::new("/k"));
        assert!(!a.contains(&"wlan0".to_string()));
        // The aggregator still listens on the relay forward port.
        let ai = a.iter().position(|x| x == "-a").unwrap();
        assert_eq!(a[ai + 1], "5800");
    }

    #[test]
    fn parse_aggregator_stats_pulls_three_counters() {
        let line = "999 PKT n_out:1500 fec_rec:12 bitrate_kbps:4200";
        let (dedup, fec, kbps) = parse_receiver_stats_line(line);
        assert_eq!(dedup, Some(1500));
        assert_eq!(fec, Some(12));
        assert_eq!(kbps, Some(4200));
    }

    #[test]
    fn non_aggregator_line_ignored() {
        let (d, f, k) = parse_receiver_stats_line("starting up");
        assert!(d.is_none() && f.is_none() && k.is_none());
    }

    #[test]
    fn receiver_state_json_shape_flattens_relays() {
        let mut s = ReceiverState::default();
        s.relays.push(RelayStats {
            mac: "aa:bb:cc:dd:ee:ff".into(),
            last_seen_ms: 123,
            fragments: 500,
        });
        let v = serde_json::to_value(&s).unwrap();
        for k in [
            "role",
            "drone_iface",
            "listen_port",
            "accept_local_nic",
            "mesh_iface",
            "relays",
            "fragments_after_dedup",
            "fec_repaired",
            "output_kbps",
            "up",
        ] {
            assert!(v.get(k).is_some(), "missing key {k}");
        }
        assert_eq!(v["relays"][0]["mac"], "aa:bb:cc:dd:ee:ff");
        assert_eq!(v["relays"][0]["fragments"], 500);
    }
}
