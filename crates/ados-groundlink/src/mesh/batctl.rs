//! batman-adv control: `batctl` output parsers + command wrappers.
//!
//! Ports `_parse_neighbors` / `_parse_gateways` / `_configure_gateway_mode` /
//! the `batctl if`/`gw_sel` calls from `mesh_manager.py`. The parsers are pure
//! (column split with TQ tolerance across batman-adv versions); the command
//! wrappers shell out to `batctl`/`ip`/`iw`/`modprobe`.
//!
//! The Python module runs `batctl` through `asyncio.to_thread` so a wedged
//! kernel module cannot stall the event loop. The Rust equivalent uses tokio's
//! async `Command` with a per-call timeout, which offloads the wait to the
//! reactor without a blocking thread; a timeout returns a non-zero rc rather
//! than hanging the poll loop.

use std::time::Duration;

use super::state::{MeshGateway, MeshNeighbor};

/// 10 Mbps down / 2 Mbps up advertisement hint for `batctl gw_mode server`.
pub const GATEWAY_BANDWIDTH_DEFAULT: &str = "10000/2000";

/// Run a command with a hard timeout. Returns `(rc, stdout, stderr)`; a timeout
/// or spawn failure yields a non-zero rc so the caller degrades gracefully.
pub async fn run(cmd: &str, args: &[&str], timeout: Duration) -> (i32, String, String) {
    let fut = tokio::process::Command::new(cmd).args(args).output();
    match tokio::time::timeout(timeout, fut).await {
        Ok(Ok(out)) => {
            let rc = out.status.code().unwrap_or(-1);
            (
                rc,
                String::from_utf8_lossy(&out.stdout).into_owned(),
                String::from_utf8_lossy(&out.stderr).into_owned(),
            )
        }
        Ok(Err(_)) => (127, String::new(), "not found".to_string()),
        Err(_) => (124, String::new(), "timeout".to_string()),
    }
}

/// Parse `batctl n -H` output. Columns: `IF  Neighbor-MAC  last-seen  [TQ]`.
/// `last-seen` is a `"0.550s"` form; the result's `last_seen_ms` is the absolute
/// age subtracted from `now_ms` (matching the Python `now_ms - last_seen_ms`).
pub fn parse_neighbors(text: &str, now_ms: i64) -> Vec<MeshNeighbor> {
    let mut out = Vec::new();
    for line in text.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() < 3 {
            continue;
        }
        let iface = parts[0].to_string();
        let mac = parts[1].to_string();
        let last_seen_s = parts[2].trim_end_matches('s');
        let last_seen_ms = last_seen_s
            .parse::<f64>()
            .map(|v| (v * 1000.0) as i64)
            .unwrap_or(0);
        // TQ is on this row in some batman-adv versions, only in `o -H` in
        // others. Tolerate its absence.
        let tq = if parts.len() >= 4 && parts[3].chars().all(|c| c.is_ascii_digit()) {
            parts[3].parse::<i64>().unwrap_or(0)
        } else {
            0
        };
        out.push(MeshNeighbor {
            mac,
            iface,
            tq,
            last_seen_ms: now_ms - last_seen_ms,
        });
    }
    out
}

/// Parse `batctl gwl -H` output. The selected row is prefixed `=>`. The class
/// column varies; pull an `<up>/<down>` pair and a TQ that may be bare or
/// parenthesized.
pub fn parse_gateways(text: &str) -> Vec<MeshGateway> {
    let mut out = Vec::new();
    for line in text.lines() {
        let mut parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() < 3 {
            continue;
        }
        let selected = parts[0] == "=>";
        if selected {
            parts.remove(0);
        }
        if parts.is_empty() {
            continue;
        }
        let mac = parts[0].to_string();
        let mut up_kbps = 0i64;
        let mut down_kbps = 0i64;
        let mut tq = 0i64;
        for tok in &parts[1..] {
            if tok.contains('/') {
                if let Some((up_s, down_s)) = tok.split_once('/') {
                    if let Ok(u) = up_s.parse::<i64>() {
                        up_kbps = u;
                    }
                    let down_clean = down_s.trim_end_matches("Mbps").trim_end_matches("kbps");
                    down_kbps = down_clean.parse::<i64>().unwrap_or(0);
                }
            } else {
                // TQ prints as "(240)" in some versions, bare "240" in others.
                let stripped = tok.trim_matches(|c| c == '(' || c == ')');
                if !stripped.is_empty() && stripped.chars().all(|c| c.is_ascii_digit()) {
                    tq = stripped.parse::<i64>().unwrap_or(0);
                }
            }
        }
        out.push(MeshGateway {
            mac,
            class_up_kbps: up_kbps,
            class_down_kbps: down_kbps,
            tq,
            selected,
        });
    }
    out
}

/// Decide and apply the batman gateway mode. Returns the resulting mode string
/// (`server` / `client` / `off`). Mirrors `_configure_gateway_mode`: `force_on`
/// advertises, `force_off` does not, `auto` advertises iff `has_uplink`; a
/// receiver that does not advertise runs as a gateway client, everyone else off.
pub async fn configure_gateway_mode(role: &str, cloud_uplink: &str, has_uplink: bool) -> String {
    let advertise = match cloud_uplink {
        "force_on" => true,
        "force_off" => false,
        _ => has_uplink, // auto
    };

    if advertise {
        run(
            "batctl",
            &["gw_mode", "server", GATEWAY_BANDWIDTH_DEFAULT],
            Duration::from_secs(5),
        )
        .await;
        "server".to_string()
    } else if role == "receiver" {
        run("batctl", &["gw_mode", "client"], Duration::from_secs(5)).await;
        "client".to_string()
    } else {
        run("batctl", &["gw_mode", "off"], Duration::from_secs(5)).await;
        "off".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_neighbors_with_and_without_tq() {
        // IF  MAC  last-seen[s]  [TQ]
        let text = "wlan1 aa:bb:cc:dd:ee:ff 0.550s 240\nwlan1 11:22:33:44:55:66 1.200s\n";
        let now = 100_000i64;
        let n = parse_neighbors(text, now);
        assert_eq!(n.len(), 2);
        assert_eq!(n[0].mac, "aa:bb:cc:dd:ee:ff");
        assert_eq!(n[0].iface, "wlan1");
        assert_eq!(n[0].tq, 240);
        // 0.550s → 550ms; absolute = now - 550.
        assert_eq!(n[0].last_seen_ms, now - 550);
        // Row without TQ → tq 0, 1.200s → 1200ms.
        assert_eq!(n[1].tq, 0);
        assert_eq!(n[1].last_seen_ms, now - 1200);
    }

    #[test]
    fn parse_neighbors_skips_short_rows() {
        let text = "garbage\nwlan1 mac\nwlan1 aa:bb:cc:dd:ee:ff 0.1s\n";
        let n = parse_neighbors(text, 0);
        assert_eq!(n.len(), 1);
    }

    #[test]
    fn parse_gateways_selected_row_and_class_split() {
        // Selected row prefixed "=>", class "10000/2000", TQ "(255)".
        let text = "=> 11:22:33:44:55:66 10000/2000 (255)\n   77:88:99:aa:bb:cc 5000/1000 240\n";
        let g = parse_gateways(text);
        assert_eq!(g.len(), 2);
        assert!(g[0].selected);
        assert_eq!(g[0].mac, "11:22:33:44:55:66");
        assert_eq!(g[0].class_up_kbps, 10000);
        assert_eq!(g[0].class_down_kbps, 2000);
        assert_eq!(g[0].tq, 255);
        assert!(!g[1].selected);
        assert_eq!(g[1].tq, 240);
    }

    #[test]
    fn parse_gateways_tolerates_mbps_suffix() {
        let text = "=> aa:aa:aa:aa:aa:aa 10000/2000Mbps 200\n";
        let g = parse_gateways(text);
        assert_eq!(g[0].class_down_kbps, 2000);
        assert_eq!(g[0].tq, 200);
    }

    #[test]
    fn parse_gateways_skips_short_rows() {
        let text = "=>\n=> mac only\n";
        // "=> mac only" → after stripping "=>", ["mac", "only"] len 2 < ... it
        // has a mac + one token, which is >= 1, so it parses as a gateway with
        // no class/tq. The first "=>" line strips to empty → skipped.
        let g = parse_gateways(text);
        assert_eq!(g.len(), 1);
        assert_eq!(g[0].mac, "mac");
    }
}
