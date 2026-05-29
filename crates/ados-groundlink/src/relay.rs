//! WFB relay: FEC-forward fragments to a receiver over batman-adv.
//!
//! Ports `wfb_relay.py`'s FEC supervision. The drone-facing RTL8812 adapter
//! runs `wfb_rx -p 0 -f <receiver_ip>:<port>` to forward video fragments to the
//! receiver; the stderr `PKT` stats line drives the `fragments_seen` /
//! `fragments_forwarded` counters; `wfb-relay.json` is written atomically.
//!
//! mDNS SEAM: the receiver is discovered via zeroconf `_ados-receiver._tcp` on
//! `bat0`. That discovery STAYS IN PYTHON (zeroconf has no maintained pure-Rust
//! equal we want to pull in here, and adapter-detect/mDNS already live on the
//! Python side). Python resolves the peer and hands this Rust supervisor the
//! `(ip, port)` to forward to; the FEC subprocess lifecycle, the stats tail, and
//! the state file are owned in Rust. The handoff is a `(String, u16)` arg (or, in
//! production, a Python-written address file the run loop reads). When the peer
//! goes stale Python re-resolves and re-invokes; this module just supervises the
//! forwarder for the address it was given.

use std::path::Path;

use ados_radio::config::WfbConfig;

use crate::process_spawn::GsWfbProcess;

/// The relay's published state (the `wfb-relay.json` shape, byte-identical to
/// the Python `_write_state`).
#[derive(Debug, Clone, serde::Serialize)]
pub struct RelayState {
    pub role: String,
    pub drone_iface: String,
    pub receiver_ip: Option<String>,
    pub receiver_port: i64,
    pub receiver_last_seen_ms: i64,
    pub fragments_seen: i64,
    pub fragments_forwarded: i64,
    pub up: bool,
    pub mesh_iface: String,
}

impl Default for RelayState {
    fn default() -> Self {
        Self {
            role: "relay".to_string(),
            drone_iface: String::new(),
            receiver_ip: None,
            receiver_port: 5800,
            receiver_last_seen_ms: 0,
            fragments_seen: 0,
            fragments_forwarded: 0,
            up: false,
            mesh_iface: "bat0".to_string(),
        }
    }
}

impl RelayState {
    /// Atomically write the state to `wfb-relay.json` (Contract-E path).
    pub fn write(&self) -> std::io::Result<()> {
        let path = Path::new(crate::paths::WFB_RELAY_JSON);
        crate::sidecars::write_json_atomic(path, self, 0o644)
    }
}

/// Build the `wfb_rx -f` FEC-forward args for the drone-facing adapter. Uses the
/// rx key (decrypts the drone uplink). Mirrors `_launch_wfb_rx_forward`.
pub fn forward_args(
    drone_iface: &str,
    receiver_ip: &str,
    receiver_port: u16,
    rx_key: &Path,
) -> Vec<String> {
    vec![
        "-p".into(),
        "0".into(),
        "-f".into(),
        format!("{receiver_ip}:{receiver_port}"),
        "-K".into(),
        rx_key.to_string_lossy().into_owned(),
        drone_iface.into(),
    ]
}

/// Spawn the FEC forwarder for `(receiver_ip, receiver_port)` on the
/// drone-facing adapter, in its own process group (setsid/killpg). stderr is
/// piped so the stats tail can read the `PKT` counters.
pub async fn spawn_forwarder(
    drone_iface: &str,
    receiver_ip: &str,
    receiver_port: u16,
) -> std::io::Result<GsWfbProcess> {
    let rx_key = Path::new(ados_radio::paths::WFB_RX_KEY);
    let args = forward_args(drone_iface, receiver_ip, receiver_port, rx_key);
    // stderr piped (the PKT stats land there); stdout discarded.
    GsWfbProcess::spawn_stderr_piped("wfb_rx", &args).await
}

/// Parse one `wfb_rx` stderr line for the relay fragment counters. Mirrors
/// `_tail_stats`: a `PKT` line carries `n_all:<seen>` and `n_out:<forwarded>`.
/// Returns `(seen, forwarded)` updates when present.
pub fn parse_relay_stats_line(line: &str) -> (Option<i64>, Option<i64>) {
    if !line.contains("PKT") {
        return (None, None);
    }
    let mut seen = None;
    let mut forwarded = None;
    for tok in line.split_whitespace() {
        if let Some(v) = tok.strip_prefix("n_all:") {
            seen = v.parse::<i64>().ok();
        } else if let Some(v) = tok.strip_prefix("n_out:") {
            forwarded = v.parse::<i64>().ok();
        }
    }
    (seen, forwarded)
}

/// The relay receiver port from config (`ground_station.wfb_relay.receiver_port`
/// is not part of `WfbConfig`; this is the documented default until the GS
/// config surface lands in Rust). Kept as a helper so the call site is explicit.
pub fn default_receiver_port(_cfg: &WfbConfig) -> u16 {
    5800
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forward_args_match_python() {
        // wfb_rx -p 0 -f <ip>:<port> -K <rx.key> <iface>
        let a = forward_args("wlan0", "10.0.0.5", 5800, Path::new("/etc/ados/wfb/rx.key"));
        assert_eq!(
            a,
            vec![
                "-p",
                "0",
                "-f",
                "10.0.0.5:5800",
                "-K",
                "/etc/ados/wfb/rx.key",
                "wlan0"
            ]
        );
    }

    #[test]
    fn parse_pkt_line_pulls_n_all_and_n_out() {
        let line = "12345 PKT n_all:1000 n_out:980 fec_rec:5";
        let (seen, fwd) = parse_relay_stats_line(line);
        assert_eq!(seen, Some(1000));
        assert_eq!(fwd, Some(980));
    }

    #[test]
    fn non_pkt_line_is_ignored() {
        let (seen, fwd) = parse_relay_stats_line("some random wfb_rx log");
        assert!(seen.is_none());
        assert!(fwd.is_none());
    }

    #[test]
    fn relay_state_json_shape() {
        let s = RelayState {
            drone_iface: "wlan0".into(),
            receiver_ip: Some("10.0.0.5".into()),
            up: true,
            ..Default::default()
        };
        let v = serde_json::to_value(&s).unwrap();
        for k in [
            "role",
            "drone_iface",
            "receiver_ip",
            "receiver_port",
            "receiver_last_seen_ms",
            "fragments_seen",
            "fragments_forwarded",
            "up",
            "mesh_iface",
        ] {
            assert!(v.get(k).is_some(), "missing key {k}");
        }
        assert_eq!(v["role"], "relay");
        assert_eq!(v["receiver_ip"], "10.0.0.5");
    }
}
