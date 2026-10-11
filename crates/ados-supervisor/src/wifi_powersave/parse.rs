//! Pure `iw` output parsers for the WiFi power-save reconciler.
//!
//! These transcribe `iw dev` / `iw dev <iface> get power_save` /
//! `iw dev <iface> link` output into the values the OS edges act on. Pure, so the
//! parsing is unit-tested without shelling `iw`. Gated to Linux + test (the OS
//! edges that drive them are Linux-only).

#![cfg(any(target_os = "linux", test))]

/// Parse the station (`type managed`) interface names from `iw dev` output.
///
/// Selected by interface type, not name: predictable names (`wlx…`, `wlp…`)
/// are stations too, and a monitor-mode injection adapter or a P2P helper is
/// not. The OS edge additionally drops any interface the WFB adapter table
/// claims. Pure.
pub(super) fn parse_station_interfaces(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current: Option<&str> = None;
    for line in text.lines() {
        let s = line.trim();
        if let Some(rest) = s.strip_prefix("Interface ") {
            current = Some(rest.trim());
        } else if let Some(kind) = s.strip_prefix("type ") {
            if let Some(name) = current.take() {
                if kind.trim() == "managed" {
                    out.push(name.to_string());
                }
            }
        }
    }
    out
}

/// Parse `iw dev <iface> get power_save` output. Returns `Some(true)` on
/// `Power save: on`, `Some(false)` on `Power save: off`, and `None` when the line
/// is absent or unparseable. Pure.
pub(super) fn parse_power_save(text: &str) -> Option<bool> {
    for line in text.lines() {
        let low = line.trim().to_ascii_lowercase();
        if let Some(rest) = low.strip_prefix("power save:") {
            let v = rest.trim();
            if v.starts_with("on") {
                return Some(true);
            }
            if v.starts_with("off") {
                return Some(false);
            }
        }
    }
    None
}

/// Parsed `iw dev <iface> link` output: the RX signal in dBm (when present) and a
/// coarse link state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct LinkInfo {
    /// The received signal strength in dBm, from the `signal: -52 dBm` line.
    pub signal_dbm: Option<i32>,
    /// `connected` when the station is associated, `disconnected` on
    /// `Not connected.`, else `unknown`.
    pub link_state: String,
}

/// Parse `iw dev <iface> link` output into a [`LinkInfo`]. Pure.
pub(super) fn parse_link(text: &str) -> LinkInfo {
    let mut signal_dbm = None;
    let mut link_state = "unknown".to_string();
    for line in text.lines() {
        let low = line.trim().to_ascii_lowercase();
        if low.starts_with("connected to") {
            link_state = "connected".to_string();
        } else if low.starts_with("not connected") {
            link_state = "disconnected".to_string();
        } else if let Some(rest) = low.strip_prefix("signal:") {
            // "signal: -52 dBm" -> -52 (the first whitespace-delimited token;
            // split_whitespace already skips the leading spaces).
            if let Some(tok) = rest.split_whitespace().next() {
                if let Ok(v) = tok.parse::<i32>() {
                    signal_dbm = Some(v);
                }
            }
        }
    }
    LinkInfo {
        signal_dbm,
        link_state,
    }
}

#[cfg(test)]
mod tests;
