//! Pure parsing + classification + decision types for the WiFi self-heal
//! watchdog.
//!
//! The connection model, the heal decision enum, the terse-nmcli parsing, the
//! gateway / neighbor parsing, and the candidate classification are OS-free so
//! they are unit-tested on every host. The OS edges that drive them live in
//! `os`; the threshold + cooldown state machine lives on the `WifiSelfHeal`
//! struct in the module root.

/// One onboard managed-WiFi connection considered by the watchdog: the
/// NetworkManager connection name and the interface it is bound to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WifiConnection {
    /// The NetworkManager connection name (the `nmcli connection up <name>` key).
    pub name: String,
    /// The interface the connection is bound to.
    pub iface: String,
}

/// What the state machine decided to do for one connection on one tick. Pure so
/// the threshold + cooldown logic is unit-tested without nmcli / ip / a clock.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HealDecision {
    /// Gateway reachable: clear the failure count, do nothing.
    Healthy,
    /// Gateway unreachable but the threshold is not met yet, or a heal cooldown
    /// is still in force: accumulate, do nothing this tick. Carries the running
    /// consecutive-failure count for the log.
    Wait { consecutive_failures: u32 },
    /// Threshold met and no cooldown in force: re-associate now. Carries the
    /// failure count that crossed the threshold for the heal event.
    Heal { consecutive_failures: u32 },
}

/// Heuristic for whether a NetworkManager connection name denotes an access
/// point the box hosts (a hotspot the watchdog must NOT re-associate) versus the
/// infrastructure link the box joins. The agent's own hotspot connection name is
/// stable; a generic hotspot / `-ap` suffix is also treated as an AP. Anything
/// else is an infrastructure link. Pure.
#[cfg(any(target_os = "linux", test))]
pub(super) fn looks_like_access_point(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    n == "ados-hotspot"
        || n.contains("hotspot")
        || n.ends_with("-ap")
        || n.ends_with(" ap")
        || n == "ap"
}

/// Split one terse `nmcli` line into its fields on unescaped colons, unescaping
/// the `\:` and `\\` sequences nmcli uses for literal colons / backslashes
/// inside a field. Pure so the field parsing is unit-tested without nmcli.
#[cfg(any(target_os = "linux", test))]
fn split_terse_line(line: &str) -> Vec<String> {
    let mut fields = Vec::new();
    let mut cur = String::new();
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                if let Some(&next) = chars.peek() {
                    cur.push(next);
                    chars.next();
                } else {
                    cur.push('\\');
                }
            }
            ':' => fields.push(std::mem::take(&mut cur)),
            other => cur.push(other),
        }
    }
    fields.push(cur);
    fields
}

/// Parse `nmcli -t -f NAME,TYPE,DEVICE,STATE connection show` terse output into
/// the onboard managed-WiFi candidates: an active `802-11-wireless` connection,
/// bound to a device, not an access point. Wired and non-active connections are
/// dropped. The interface-level exclusions (injection driver, monitor mode) are
/// applied by the caller, which can read sysfs / `iw`; this pure pass keeps only
/// the connection-level shape.
#[cfg(any(target_os = "linux", test))]
pub(super) fn parse_active_wifi_connections(
    terse: &str,
    is_access_point: impl Fn(&str) -> bool,
) -> Vec<WifiConnection> {
    let mut out = Vec::new();
    for line in terse.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let fields = split_terse_line(line);
        // NAME:TYPE:DEVICE:STATE — a malformed short line is skipped.
        let (Some(name), Some(ctype), Some(device), Some(state)) =
            (fields.first(), fields.get(1), fields.get(2), fields.get(3))
        else {
            continue;
        };
        if name.is_empty() || ctype != "802-11-wireless" {
            continue;
        }
        // Only an activated connection is a candidate: a defined-but-down profile
        // is NetworkManager's job to bring up, not ours to re-associate.
        if state.trim() != "activated" {
            continue;
        }
        let dev = device.trim();
        if dev.is_empty() || dev == "--" {
            continue;
        }
        // The link the box hosts (a hotspot) is never re-associated; we rebuild
        // only the link the box USES to reach the LAN.
        if is_access_point(name) {
            continue;
        }
        out.push(WifiConnection {
            name: name.to_string(),
            iface: dev.to_string(),
        });
    }
    out
}

/// Parse the gateway out of `ip -4 route show default dev <iface>` output: the
/// `via <gw>` token on the `default` line. Returns the gateway IP, or `None`
/// when there is no default route on that interface. Pure.
#[cfg(any(target_os = "linux", test))]
pub(super) fn parse_gateway(text: &str) -> Option<String> {
    for line in text.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.first() == Some(&"default") {
            if let Some(idx) = parts.iter().position(|p| *p == "via") {
                if let Some(gw) = parts.get(idx + 1) {
                    return Some((*gw).to_string());
                }
            }
        }
    }
    None
}

/// Parse the neighbor (ARP) reachability for a gateway out of
/// `ip neighbor show <gw> dev <iface>` output, read right after a probe has
/// been sent to the gateway. Only a confirmed state counts: REACHABLE (the
/// gateway answered recently), PERMANENT or NOARP (static entries). STALE
/// persists indefinitely on an idle link, and DELAY / PROBE are the kernel
/// actively doubting the entry, so none of them proves the data path; nor do
/// INCOMPLETE / FAILED or an absent entry. Pure.
#[cfg(any(target_os = "linux", test))]
pub(super) fn parse_neighbor_reachable(text: &str) -> bool {
    text.lines().any(|line| {
        line.split_whitespace()
            .next_back()
            .is_some_and(|state| matches!(state, "REACHABLE" | "PERMANENT" | "NOARP"))
    })
}

/// Decide whether an interface (given its driver, USB ids and current mode)
/// is an onboard managed-WiFi candidate. Excludes the injection adapter (the
/// shared WFB adapter table: driver or VID:PID) and anything not in
/// managed/station mode (a monitor-mode iface is the radio adapter and must
/// never be touched). `mode` is the `iw` operating mode string, or `None` when
/// unreadable — an unreadable mode is treated as NOT a candidate (fail safe:
/// never act on an interface we cannot positively confirm is a managed
/// station). Pure.
#[cfg(any(target_os = "linux", test))]
pub(super) fn iface_is_managed_candidate(
    driver: &str,
    vid_pid: Option<(u16, u16)>,
    mode: Option<&str>,
) -> bool {
    if ados_protocol::netif::is_wfb_adapter(driver, vid_pid) {
        return false;
    }
    match mode {
        Some(m) => {
            let m = m.trim().to_ascii_lowercase();
            m == "managed" || m == "station"
        }
        None => false,
    }
}

#[cfg(test)]
mod tests;
