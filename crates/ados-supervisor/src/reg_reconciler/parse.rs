//! Pure `iw` output parsers for the regulatory reconciler.
//!
//! These transcribe `iw reg get` / `iw <iface> info` / `iw phy <phy> channels` /
//! `iw dev` output into the values the OS edges act on. Pure, so the parsing is
//! unit-tested without shelling `iw`. Gated to Linux + test (the OS edges that
//! drive them are Linux-only).

#![cfg(any(target_os = "linux", test))]

/// Parse the global regulatory country from `iw reg get` output: the first
/// `country XX:` line (before any per-phy self-managed block). Returns the
/// uppercase two-character code, or `None`. Pure.
pub(super) fn parse_global_reg_domain(text: &str) -> Option<String> {
    for line in text.lines() {
        let s = line.trim();
        if let Some(rest) = s.strip_prefix("country ") {
            let cc: String = rest.chars().take(2).collect();
            if cc.len() == 2 {
                return Some(cc.to_ascii_uppercase());
            }
        }
    }
    None
}

/// Extract the `phyN` wiphy name from `iw <iface> info` output (the `wiphy <N>`
/// line). Returns e.g. `"phy0"`, or `None`. Pure.
pub(super) fn parse_wiphy(info: &str) -> Option<String> {
    for line in info.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("wiphy ") {
            let n = rest.split_whitespace().next()?;
            if !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()) {
                return Some(format!("phy{}", n));
            }
        }
    }
    None
}

/// Parse `iw phy <phy> channels` output into the set of usable channel numbers
/// (the `[<channel>]` token on a line not marked `disabled` / `no ir` /
/// `radar`). An empty set means "could not determine". Pure. Identical filter
/// to the radio-side `parse_enabled_channels` so the two halves agree.
pub(super) fn parse_enabled_channels(text: &str) -> std::collections::BTreeSet<u8> {
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
        if low.contains("disabled") || low.contains("no ir") || low.contains("radar") {
            continue;
        }
        out.insert(ch);
    }
    out
}

/// First WFB-compatible injection interface from `iw dev` output, or `None`. The
/// channel-safety read needs the injection adapter's wiphy. We do not parse the
/// driver here (that needs sysfs); the wiphy channel set is the same for any
/// interface on that phy, and the only interface whose enabled set matters for
/// the WFB channel is the injection adapter — which is the only one whose phy
/// would carry the U-NII-3 channels in the first place. We pick the first phy
/// whose enabled set contains the target channel, so an onboard 2.4 GHz phy is
/// naturally skipped. Pure.
pub(super) fn parse_interfaces(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in text.lines() {
        let s = line.trim();
        if let Some(rest) = s.strip_prefix("Interface ") {
            let name = rest.trim();
            if !name.is_empty() {
                out.push(name.to_string());
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_global_reg_domain_before_self_managed_block() {
        let text = "\
global
country BO: DFS-FCC
        (5170 - 5250 @ 80), (24)
phy#3 (self-managed)
country US: DFS-FCC
";
        // The FIRST country line is the global domain.
        assert_eq!(parse_global_reg_domain(text).as_deref(), Some("BO"));
    }

    #[test]
    fn parses_wiphy_and_channels() {
        let info = "Interface wlan1\n\twiphy 3\n\ttype monitor\n";
        assert_eq!(parse_wiphy(info).as_deref(), Some("phy3"));
        let chans = "\
* 5745 MHz [149] (24.0 dBm)
* 5765 MHz [153] (disabled)
* 5260 MHz [52] (no IR, radar detection)
* 5825 MHz [165] (24.0 dBm)
";
        let enabled = parse_enabled_channels(chans);
        assert!(enabled.contains(&149));
        assert!(enabled.contains(&165));
        assert!(!enabled.contains(&153)); // disabled
        assert!(!enabled.contains(&52)); // radar / no IR
    }

    #[test]
    fn parses_interface_list() {
        let dev = "\
phy#3
\tInterface wlan1
\t\ttype monitor
phy#0
\tInterface wlan0
\t\ttype managed
";
        assert_eq!(parse_interfaces(dev), vec!["wlan1", "wlan0"]);
    }
}
