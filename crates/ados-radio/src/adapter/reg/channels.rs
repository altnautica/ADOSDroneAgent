//! Enabled and DFS channel sets for an interface.

use super::*;

/// The 5 GHz channel numbers this adapter can actually use for the link.
///
/// Parses `iw <iface> info` to find the wiphy, then `iw phy <phyN> channels`,
/// keeping only channels that are not `(disabled)` and not radar / `no IR` (DFS
/// channels need a channel-availability check the link does not perform). The
/// drone and ground frequently run different regulatory domains, so the air
/// channel must be in the intersection of both sides' enabled sets; this exposes
/// the local half. An empty set means "could not determine"; callers treat that
/// as "do not restrict".
#[cfg(target_os = "linux")]
pub async fn enabled_channels(iface: &str) -> std::collections::BTreeSet<u8> {
    let info = match run_cmd_output("iw", &[iface, "info"]).await {
        Ok(out) => out,
        Err(()) => return std::collections::BTreeSet::new(),
    };
    let Some(phy) = parse_wiphy(&info) else {
        return std::collections::BTreeSet::new();
    };
    let chans = match run_cmd_output("iw", &["phy", &phy, "channels"]).await {
        Ok(out) => out,
        Err(()) => return std::collections::BTreeSet::new(),
    };
    parse_enabled_channels(&chans)
}

#[cfg(not(target_os = "linux"))]
pub async fn enabled_channels(_iface: &str) -> std::collections::BTreeSet<u8> {
    std::collections::BTreeSet::new()
}

/// Extract the `phyN` wiphy name from `iw <iface> info` output (the `wiphy <N>`
/// line). Returns e.g. `"phy0"`, or `None` when absent.
#[cfg(any(target_os = "linux", test))]
pub(super) fn parse_wiphy(info: &str) -> Option<String> {
    for line in info.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("wiphy ") {
            let n = rest.split_whitespace().next()?;
            if n.chars().all(|c| c.is_ascii_digit()) && !n.is_empty() {
                return Some(format!("phy{}", n));
            }
        }
    }
    None
}

/// Parse `iw phy <phy> channels` output into the set of usable channel numbers.
/// A line carries a `[<channel>]` token; it is kept only when the line is not
/// marked `disabled`, `no ir`, or `radar`.
#[cfg(any(target_os = "linux", test))]
pub(super) fn parse_enabled_channels(text: &str) -> std::collections::BTreeSet<u8> {
    let mut out = std::collections::BTreeSet::new();
    for line in text.lines() {
        // The channel number sits inside square brackets, e.g.
        //   "* 5745 MHz [149]"                       (usable)
        //   "* 5180 MHz [36] (disabled)"             (skip)
        //   "* 5260 MHz [52] (no IR, radar detection)" (skip)
        let Some(start) = line.find('[') else {
            continue;
        };
        let Some(len) = line[start + 1..].find(']') else {
            continue;
        };
        let token = &line[start + 1..start + 1 + len];
        let Ok(ch) = token.parse::<u8>() else {
            continue;
        };
        let low = line.to_lowercase();
        if low.contains("disabled") || low.contains("no ir") || low.contains("radar") {
            continue;
        }
        out.insert(ch);
    }
    out
}

/// The DFS / no-IR / radar channels for this interface's domain — the channels a
/// rendezvous home must avoid unless `dfs_allowed`. Reads the same
/// `iw phy <phy> channels` output as [`enabled_channels`] and keeps the channels
/// it marks `no ir` / `radar`. An empty set means "could not determine".
#[cfg(target_os = "linux")]
pub async fn dfs_channels(iface: &str) -> std::collections::BTreeSet<u8> {
    let info = match run_cmd_output("iw", &[iface, "info"]).await {
        Ok(out) => out,
        Err(()) => return std::collections::BTreeSet::new(),
    };
    let Some(phy) = parse_wiphy(&info) else {
        return std::collections::BTreeSet::new();
    };
    let chans = match run_cmd_output("iw", &["phy", &phy, "channels"]).await {
        Ok(out) => out,
        Err(()) => return std::collections::BTreeSet::new(),
    };
    parse_dfs_channels(&chans)
}

#[cfg(not(target_os = "linux"))]
pub async fn dfs_channels(_iface: &str) -> std::collections::BTreeSet<u8> {
    std::collections::BTreeSet::new()
}

/// Parse `iw phy <phy> channels` into the set of channels that need DFS
/// clearance (lines marked `no ir` or `radar`). A `disabled` channel is not a
/// DFS channel — it is simply unavailable — so it is excluded here.
#[cfg(any(target_os = "linux", test))]
pub(super) fn parse_dfs_channels(text: &str) -> std::collections::BTreeSet<u8> {
    let mut out = std::collections::BTreeSet::new();
    for line in text.lines() {
        let Some(start) = line.find('[') else {
            continue;
        };
        let Some(len) = line[start + 1..].find(']') else {
            continue;
        };
        let token = &line[start + 1..start + 1 + len];
        let Ok(ch) = token.parse::<u8>() else {
            continue;
        };
        let low = line.to_lowercase();
        if low.contains("disabled") {
            continue;
        }
        if low.contains("no ir") || low.contains("radar") {
            out.insert(ch);
        }
    }
    out
}
