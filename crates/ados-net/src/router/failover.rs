//! Failover policy: priority chain, hysteresis, persistence, and the kernel
//! routing-table primitive.
//!
//! The router FSM owns the loop and the live state. This module owns the pure
//! logic: the configured priority list (load/save), the hysteresis thresholds,
//! the routing-table replace command, and the selection helpers that pick the
//! next viable uplink. Ports `uplink/failover.py`.

use std::path::Path;
use std::process::Command;

use serde_json::Value;
use tracing::{info, warn};

use crate::sidecar;
use crate::sysfs::detect_ethernet_iface;

/// Default priority chain. The LAN-side AP SSID served to phones and laptops
/// is not an uplink, so it is absent here.
pub const DEFAULT_PRIORITY: [&str; 4] = ["eth0", "wlan0_client", "wwan0", "usb0"];

/// Per-uplink route metric. Lower wins in the kernel routing table; the gap is
/// kept large to survive manual `ip route` probes. Unknown ifaces use 500.
pub const PRIORITY_METRIC_DEFAULT: u32 = 500;

/// Resolve the route metric for `iface` given the resolved wired iface name.
///
/// The wired uplink is the top-priority route, but its kernel name varies by
/// BSP (`eth0`, `end1`, `enp*`). Matching the resolved name (not a hard-coded
/// `eth0`) makes the wired link metric 100 wherever it lands. Pure so the
/// mapping is unit-testable without touching sysfs.
pub fn priority_metric_for(iface: &str, wired_iface: &str) -> u32 {
    if iface == wired_iface {
        return 100;
    }
    match iface {
        "wlan0_client" => 200,
        "wwan0" => 300,
        "usb0" => 400,
        _ => PRIORITY_METRIC_DEFAULT,
    }
}

/// Resolve the route metric for `iface`, detecting the wired iface from sysfs.
/// Mirrors `PRIORITY_METRIC.get(iface, 500)` with the wired slot resolved to
/// whatever the board calls its NIC.
pub fn priority_metric(iface: &str) -> u32 {
    priority_metric_for(iface, &detect_ethernet_iface())
}

/// Three consecutive fails flip us down to the next viable uplink.
pub const FAIL_DOWN_THRESHOLD: u32 = 3;
/// Three consecutive successes on a higher-priority uplink flip us back up.
pub const SUCCESS_UP_THRESHOLD: u32 = 3;
/// Cooldown between switches to prevent thrash when two uplinks are both flaky.
pub const SWITCH_COOLDOWN_SECONDS: f64 = 30.0;

/// The default priority chain as owned `String`s.
pub fn default_priority() -> Vec<String> {
    DEFAULT_PRIORITY.iter().map(|s| s.to_string()).collect()
}

/// Load the priority list from disk, falling back to the default on any error,
/// a missing file, a malformed body, a non-string element, or an empty list.
/// Mirrors `failover.load_priority`.
pub fn load_priority(path: &Path) -> Vec<String> {
    match std::fs::read(path) {
        Ok(bytes) => match serde_json::from_slice::<Value>(&bytes) {
            Ok(raw) => match raw.get("priority") {
                Some(Value::Array(items)) => {
                    let mut order: Vec<String> = Vec::with_capacity(items.len());
                    for item in items {
                        match item.as_str() {
                            Some(s) => order.push(s.to_string()),
                            // A non-string element invalidates the whole list.
                            None => return default_priority(),
                        }
                    }
                    if order.is_empty() {
                        default_priority()
                    } else {
                        order
                    }
                }
                _ => default_priority(),
            },
            Err(exc) => {
                warn!(error = %exc, "uplink.priority_load_failed");
                default_priority()
            }
        },
        // Missing file is the common path; only a real I/O error is logged.
        Err(exc) if exc.kind() == std::io::ErrorKind::NotFound => default_priority(),
        Err(exc) => {
            warn!(error = %exc, "uplink.priority_load_failed");
            default_priority()
        }
    }
}

/// Render `{"priority": [...]}` byte-identically to Python `json.dumps` default
/// separators (`", "` between items, `": "` after the key), single line, no
/// trailing newline. `serde_json::to_string` is compact (no spaces) so the body
/// is built by hand; each element is escaped through `serde_json` so embedded
/// quotes / unicode match Python's escaping.
pub fn render_priority_json(priority: &[String]) -> String {
    let mut out = String::from("{\"priority\": [");
    for (i, item) in priority.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        // Escape the single string value the same way json.dumps does.
        out.push_str(&serde_json::to_string(item).unwrap_or_else(|_| "\"\"".to_string()));
    }
    out.push_str("]}");
    out
}

/// Atomically persist the priority list to disk, byte-matching the Python
/// writer. Best-effort: an I/O error is logged and swallowed.
pub fn save_priority(path: &Path, priority: &[String]) {
    let body = render_priority_json(priority);
    if let Err(exc) = sidecar::write_atomic(path, body.as_bytes()) {
        warn!(error = %exc, "uplink.priority_save_failed");
    }
}

/// Returns `Err` if the priority list is empty. (Rust's type system already
/// guarantees the per-element string invariant the Python guard re-checked.)
pub fn validate_priority(priority_list: &[String]) -> Result<(), String> {
    if priority_list.is_empty() {
        return Err("priority must be a non-empty list of strings".to_string());
    }
    Ok(())
}

/// Pick the next viable uplink below the current one. First tries strictly
/// lower-priority entries below the current uplink; if none are available,
/// falls back to any available uplink that is not the current one. Returns
/// `None` when the current uplink is the only available option. Mirrors
/// `failover.select_failover_target`.
pub fn select_failover_target(
    priority: &[String],
    available: &[String],
    current: Option<&str>,
) -> Option<String> {
    if let Some(cur) = current {
        if let Some(current_idx) = priority.iter().position(|p| p == cur) {
            for candidate in &priority[current_idx + 1..] {
                if available.iter().any(|a| a == candidate) {
                    return Some(candidate.clone());
                }
            }
        }
    }
    available
        .iter()
        .find(|u| Some(u.as_str()) != current)
        .cloned()
}

/// Return available uplinks ranked above the current one. Mirrors
/// `failover.select_higher_priority`.
pub fn select_higher_priority(
    priority: &[String],
    available: &[String],
    current: Option<&str>,
) -> Vec<String> {
    let cur = match current {
        Some(c) => c,
        None => return Vec::new(),
    };
    let current_idx = match priority.iter().position(|p| p == cur) {
        Some(i) => i,
        None => return Vec::new(),
    };
    available
        .iter()
        .filter(|u| {
            priority
                .iter()
                .position(|p| p == *u)
                .is_some_and(|idx| idx < current_idx)
        })
        .cloned()
        .collect()
}

/// Replace the kernel default route to point at `iface`. Returns `true` on a
/// zero exit, `false` on a non-zero exit or a spawn failure. Mirrors
/// `failover.apply_default_route`.
pub fn apply_default_route(iface: &str, gateway: Option<&str>) -> bool {
    let metric = priority_metric(iface);
    let metric_s = metric.to_string();
    let mut cmd = Command::new("ip");
    cmd.args(["route", "replace", "default"]);
    match gateway {
        Some(gw) => {
            cmd.args(["via", gw, "dev", iface, "metric", &metric_s]);
        }
        None => {
            cmd.args(["dev", iface, "metric", &metric_s]);
        }
    }
    match cmd.output() {
        Ok(result) => {
            if !result.status.success() {
                warn!(
                    iface = iface,
                    rc = result.status.code().unwrap_or(-1),
                    stderr = %String::from_utf8_lossy(&result.stderr).trim(),
                    "uplink.route_replace_failed"
                );
                return false;
            }
            info!(
                iface = iface,
                gateway = gateway,
                metric = metric,
                "uplink.route_applied"
            );
            true
        }
        Err(exc) => {
            warn!(error = %exc, "uplink.route_apply_exc");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn priority_metric_table_and_default() {
        // The classic eth0 board: eth0 is the wired iface, metric 100.
        assert_eq!(priority_metric_for("eth0", "eth0"), 100);
        assert_eq!(priority_metric_for("wlan0_client", "eth0"), 200);
        assert_eq!(priority_metric_for("wwan0", "eth0"), 300);
        assert_eq!(priority_metric_for("usb0", "eth0"), 400);
        assert_eq!(priority_metric_for("anything-else", "eth0"), 500);
    }

    #[test]
    fn detected_wired_iface_gets_top_metric() {
        // A board whose NIC is `end1`: the detected wired iface, not a literal
        // `eth0`, must be the metric-100 route. The plain `eth0` string is then
        // just another unknown iface (metric 500).
        assert_eq!(priority_metric_for("end1", "end1"), 100);
        assert_eq!(priority_metric_for("eth0", "end1"), 500);
        // The other slots are unchanged.
        assert_eq!(priority_metric_for("wlan0_client", "end1"), 200);
        assert_eq!(priority_metric_for("wwan0", "end1"), 300);
        assert_eq!(priority_metric_for("usb0", "end1"), 400);
    }

    #[test]
    fn priority_json_is_byte_exact_to_python_json_dumps() {
        // Python: json.dumps({"priority": [...]}) → spaces after ':' and ','.
        let body = render_priority_json(&default_priority());
        assert_eq!(
            body,
            r#"{"priority": ["eth0", "wlan0_client", "wwan0", "usb0"]}"#
        );
        // No trailing newline.
        assert!(!body.ends_with('\n'));
        // Single element form.
        assert_eq!(
            render_priority_json(&s(&["eth0"])),
            r#"{"priority": ["eth0"]}"#
        );
        // Empty list (degenerate, never persisted by validate but byte-checked).
        assert_eq!(render_priority_json(&[]), r#"{"priority": []}"#);
    }

    #[test]
    fn save_then_load_round_trips_and_file_matches_python_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ground-station-uplink.json");
        let order = s(&["wlan0_client", "eth0"]);
        save_priority(&path, &order);
        // On-disk bytes byte-match Python.
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            r#"{"priority": ["wlan0_client", "eth0"]}"#
        );
        // And round-trips back through the loader.
        assert_eq!(load_priority(&path), order);
    }

    #[test]
    fn load_falls_back_on_missing_malformed_and_empty() {
        let dir = tempfile::tempdir().unwrap();
        // Missing file.
        assert_eq!(
            load_priority(&dir.path().join("nope.json")),
            default_priority()
        );
        // Malformed JSON.
        let bad = dir.path().join("bad.json");
        std::fs::write(&bad, b"not json").unwrap();
        assert_eq!(load_priority(&bad), default_priority());
        // Empty priority list.
        let empty = dir.path().join("empty.json");
        std::fs::write(&empty, br#"{"priority": []}"#).unwrap();
        assert_eq!(load_priority(&empty), default_priority());
        // Non-string element invalidates.
        let mixed = dir.path().join("mixed.json");
        std::fs::write(&mixed, br#"{"priority": ["eth0", 7]}"#).unwrap();
        assert_eq!(load_priority(&mixed), default_priority());
    }

    #[test]
    fn select_failover_target_walks_below_then_falls_back() {
        let prio = default_priority();
        // From eth0 with wlan0_client + wwan0 available → next below is wlan0_client.
        assert_eq!(
            select_failover_target(&prio, &s(&["wlan0_client", "wwan0"]), Some("eth0")),
            Some("wlan0_client".to_string())
        );
        // From wwan0, nothing below available but eth0 is → fall back to the
        // first available that is not current.
        assert_eq!(
            select_failover_target(&prio, &s(&["eth0"]), Some("wwan0")),
            Some("eth0".to_string())
        );
        // Only the current uplink available → None.
        assert_eq!(
            select_failover_target(&prio, &s(&["eth0"]), Some("eth0")),
            None
        );
        // No current → first alternative.
        assert_eq!(
            select_failover_target(&prio, &s(&["wwan0", "usb0"]), None),
            Some("wwan0".to_string())
        );
    }

    #[test]
    fn select_higher_priority_ranks_above_current() {
        let prio = default_priority();
        // Current wwan0; eth0 + wlan0_client available and rank above.
        assert_eq!(
            select_higher_priority(&prio, &s(&["eth0", "wlan0_client", "usb0"]), Some("wwan0")),
            s(&["eth0", "wlan0_client"])
        );
        // Current eth0 (top) → nothing above.
        assert!(select_higher_priority(&prio, &s(&["wlan0_client"]), Some("eth0")).is_empty());
        // No current → empty.
        assert!(select_higher_priority(&prio, &s(&["eth0"]), None).is_empty());
    }

    #[test]
    fn validate_rejects_empty() {
        assert!(validate_priority(&s(&["eth0"])).is_ok());
        assert!(validate_priority(&[]).is_err());
    }
}
