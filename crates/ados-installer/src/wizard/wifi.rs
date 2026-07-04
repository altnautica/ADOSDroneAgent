//! Wi-Fi setup for the onboarding wizard: scan, join, verify, and persist a
//! home network so the operator can unplug the wired cable.
//!
//! It drives `nmcli` directly (NetworkManager owns credential storage, so this
//! never writes an agent-config file). The nmcli scan/join grammar is ported
//! from the ground-station Wi-Fi client manager; here it runs synchronously
//! through the installer's one shell-out primitive.
//!
//! Two safety rules shape every call:
//!   * It never touches the interface the operator's session rides on. If the
//!     install is happening over an SSH link that runs on Wi-Fi, the whole step
//!     refuses (reconfiguring that radio would drop the session).
//!   * It never touches the long-range radio adapter. The join is always pinned
//!     to a resolved management-Wi-Fi interface via `ifname`.
//!
//! Verification checks LAN reachability (the default gateway answers), NOT
//! internet, so a genuinely-degraded link can never read as connected.

use crate::exec;

/// One scanned network row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Network {
    pub ssid: String,
    pub signal: u8,
    pub secured: bool,
    pub in_use: bool,
}

/// The result of the LAN-reachability check after a join.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LanReach {
    /// The default gateway on the joined interface answered.
    pub reachable: bool,
    /// The gateway address the check used (for the operator message).
    pub gateway: Option<String>,
}

/// True for a wireless interface name.
pub fn is_wifi_iface(name: &str) -> bool {
    name.starts_with("wlan") || name.starts_with("wlp") || name.starts_with("wlx") || name == "wl"
}

/// Whether the operator's session runs over a Wi-Fi interface. When the install
/// is happening over SSH, reconfiguring that radio would drop the connection, so
/// the wizard skips the Wi-Fi step entirely. A local console session (no
/// `SSH_CONNECTION`) is always safe.
pub fn session_rides_wifi() -> bool {
    let ssh = match std::env::var("SSH_CONNECTION") {
        Ok(v) if !v.trim().is_empty() => v,
        _ => return false,
    };
    // SSH_CONNECTION = "<client-ip> <client-port> <server-ip> <server-port>".
    if let Some(client_ip) = ssh.split_whitespace().next() {
        let res = exec::run("ip", &["route", "get", client_ip]);
        if res.success() {
            if let Some(iface) = parse_egress_iface(&res.stdout) {
                return is_wifi_iface(&iface);
            }
        }
    }
    // Fall back to the default egress: if the box reaches the world over Wi-Fi
    // and we are on SSH, treat the session as riding Wi-Fi (conservative).
    default_egress_iface()
        .map(|i| is_wifi_iface(&i))
        .unwrap_or(false)
}

/// True when the box currently reaches the internet over a Wi-Fi interface (so
/// the wizard defaults to "skip" rather than "set up Wi-Fi").
pub fn currently_on_wifi() -> bool {
    default_egress_iface()
        .map(|i| is_wifi_iface(&i))
        .unwrap_or(false)
}

/// The interface that carries the default route (`ip route get 1.1.1.1`).
pub fn default_egress_iface() -> Option<String> {
    let res = exec::run("ip", &["route", "get", "1.1.1.1"]);
    if res.success() {
        parse_egress_iface(&res.stdout)
    } else {
        None
    }
}

/// Resolve the management Wi-Fi interface to operate on: a Wi-Fi device
/// NetworkManager manages whose kernel driver is NOT a long-range-radio driver
/// (so the injection adapter is never touched). Prefers a connected device.
pub fn management_wifi_iface() -> Option<String> {
    let res = exec::run(
        "nmcli",
        &["-t", "-f", "DEVICE,TYPE,STATE", "device", "status"],
    );
    if !res.success() {
        return None;
    }
    let devices = parse_wifi_devices(&res.stdout);
    let mut fallback: Option<String> = None;
    for (dev, state) in devices {
        if is_radio_driver_iface(&dev) {
            continue;
        }
        if state.contains("connected") && !state.contains("disconnected") {
            return Some(dev);
        }
        fallback.get_or_insert(dev);
    }
    fallback
}

/// True when an interface's kernel driver is a long-range-radio driver, so the
/// Wi-Fi step must skip it. The driver comes from the sysfs symlink; the family
/// list mirrors the radio stack's compatible-driver set (its short, stable
/// name set — the radio stack owns the authoritative classification).
fn is_radio_driver_iface(iface: &str) -> bool {
    match iface_driver(iface) {
        Some(driver) => is_radio_driver(&driver),
        None => false,
    }
}

/// Read an interface's kernel driver name from sysfs (the basename of the
/// `device/driver` symlink), or `None`.
fn iface_driver(iface: &str) -> Option<String> {
    let link = format!("/sys/class/net/{iface}/device/driver");
    let target = std::fs::read_link(&link).ok()?;
    target
        .file_name()
        .and_then(|n| n.to_str())
        .map(|s| s.to_string())
}

/// True when a driver name belongs to the long-range-radio family (pure).
pub fn is_radio_driver(driver: &str) -> bool {
    const RADIO_DRIVERS: &[&str] = &[
        "8812au",
        "8812eu",
        "rtl8812au",
        "rtl8812eu",
        "rtl88x2eu",
        "rtl88xxau",
    ];
    let d = driver.to_ascii_lowercase();
    RADIO_DRIVERS.iter().any(|r| d == *r)
}

/// Scan for nearby networks on `iface`, strongest first, de-duplicated by SSID.
pub fn scan(iface: &str) -> Vec<Network> {
    let res = exec::run(
        "nmcli",
        &[
            "-t",
            "-f",
            "SSID,SIGNAL,SECURITY,IN-USE",
            "device",
            "wifi",
            "list",
            "ifname",
            iface,
            "--rescan",
            "yes",
        ],
    );
    if res.success() {
        return parse_wifi_list(&res.stdout);
    }
    // A busy radio can refuse a forced rescan; fall back to the cached list.
    let res = exec::run(
        "nmcli",
        &[
            "-t",
            "-f",
            "SSID,SIGNAL,SECURITY,IN-USE",
            "device",
            "wifi",
            "list",
            "ifname",
            iface,
        ],
    );
    if res.success() {
        parse_wifi_list(&res.stdout)
    } else {
        Vec::new()
    }
}

/// Join `ssid` on `iface`. `password` is `None` for an open network. `hidden`
/// adds the not-broadcast flag. Returns `Ok(())` on a successful association or
/// `Err(reason)` with a trimmed nmcli message.
pub fn connect(
    iface: &str,
    ssid: &str,
    password: Option<&str>,
    hidden: bool,
) -> Result<(), String> {
    let mut args: Vec<String> = vec![
        "device".into(),
        "wifi".into(),
        "connect".into(),
        ssid.into(),
    ];
    if let Some(pw) = password {
        args.push("password".into());
        args.push(pw.into());
    }
    args.push("ifname".into());
    args.push(iface.into());
    if hidden {
        args.push("hidden".into());
        args.push("yes".into());
    }
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let res = exec::run("nmcli", &arg_refs);
    if res.success() {
        Ok(())
    } else {
        let msg = res.stderr.trim();
        Err(if msg.is_empty() {
            "could not connect".to_string()
        } else {
            msg.to_string()
        })
    }
}

/// Delete the NetworkManager connection profile named after `ssid` (best-effort).
/// `connect` creates a saved profile the moment it associates; if the link then
/// fails the LAN-reachability check, that profile would otherwise linger and
/// auto-reconnect to a dead network on the next boot. Tearing it down keeps a
/// connected-but-unreachable attempt from being saved.
pub fn forget(ssid: &str) {
    let _ = exec::run("nmcli", &["connection", "delete", ssid]);
}

/// Verify the joined link reaches the LAN (its default gateway answers a single
/// ping), NOT the internet. A link with an address but no working data path
/// fails this, so it can never read as connected.
pub fn verify_lan_reachable(iface: &str) -> LanReach {
    let gateway = default_gateway(iface);
    let gw = match &gateway {
        Some(g) => g.clone(),
        None => {
            return LanReach {
                reachable: false,
                gateway: None,
            }
        }
    };
    let reachable = exec::run_ok("ping", &["-c", "1", "-W", "2", "-I", iface, &gw]);
    LanReach { reachable, gateway }
}

/// The default-route gateway on `iface`, or `None`.
fn default_gateway(iface: &str) -> Option<String> {
    let res = exec::run("ip", &["-4", "route", "show", "default", "dev", iface]);
    if res.success() {
        parse_default_gw(&res.stdout)
    } else {
        None
    }
}

/// Persist the joined profile for the next boot: auto-connect on, a route metric
/// higher than wired so a plugged cable stays primary, and Wi-Fi power-save off
/// so the radio never parks the uplink. NetworkManager names the profile after
/// the SSID. Best-effort — a failure here only affects the next boot.
pub fn persist(ssid: &str) {
    let _ = exec::run(
        "nmcli",
        &[
            "connection",
            "modify",
            ssid,
            "connection.autoconnect",
            "yes",
        ],
    );
    // A higher metric = lower priority: wired (default ~100) wins when present.
    let _ = exec::run(
        "nmcli",
        &["connection", "modify", ssid, "ipv4.route-metric", "700"],
    );
    let _ = exec::run(
        "nmcli",
        &["connection", "modify", ssid, "ipv6.route-metric", "700"],
    );
    // 2 = power-save disabled.
    let _ = exec::run(
        "nmcli",
        &[
            "connection",
            "modify",
            ssid,
            "802-11-wireless.powersave",
            "2",
        ],
    );
}

// ── pure parsers (unit-tested) ────────────────────────────────────────────

/// Split one nmcli terse (`-t`) line on unescaped colons, honoring the
/// backslash escape (pure). Mirrors the ground-station terse parser.
pub fn split_terse(line: &str) -> Vec<String> {
    let mut parts: Vec<String> = Vec::new();
    let mut buf = String::new();
    let mut chars = line.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\\' => {
                if let Some(next) = chars.next() {
                    buf.push(next);
                }
            }
            ':' => {
                parts.push(std::mem::take(&mut buf));
            }
            _ => buf.push(ch),
        }
    }
    parts.push(buf);
    parts
}

/// Parse `nmcli -t -f SSID,SIGNAL,SECURITY,IN-USE device wifi list` output into
/// networks, de-duplicated by SSID (strongest kept), strongest first (pure).
pub fn parse_wifi_list(output: &str) -> Vec<Network> {
    let mut nets: Vec<Network> = Vec::new();
    for line in output.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let cols = split_terse(line);
        if cols.len() < 4 {
            continue;
        }
        let ssid = cols[0].trim().to_string();
        if ssid.is_empty() {
            continue;
        }
        let signal = cols[1].trim().parse::<u8>().unwrap_or(0).min(100);
        let sec = cols[2].trim();
        let secured = !sec.is_empty() && sec != "--";
        let in_use = cols[3].trim() == "*";
        match nets.iter_mut().find(|n| n.ssid == ssid) {
            Some(existing) => {
                if signal > existing.signal {
                    existing.signal = signal;
                }
                existing.secured = existing.secured || secured;
                existing.in_use = existing.in_use || in_use;
            }
            None => nets.push(Network {
                ssid,
                signal,
                secured,
                in_use,
            }),
        }
    }
    nets.sort_by(|a, b| b.signal.cmp(&a.signal).then(a.ssid.cmp(&b.ssid)));
    nets
}

/// Parse the egress interface from `ip route get <ip>` output: the token after
/// `dev` (pure). Handles both the direct and the `via <gw> dev <if>` forms.
pub fn parse_egress_iface(output: &str) -> Option<String> {
    let mut tokens = output.split_whitespace();
    while let Some(tok) = tokens.next() {
        if tok == "dev" {
            return tokens.next().map(|s| s.to_string());
        }
    }
    None
}

/// Parse the gateway from `ip -4 route show default dev <if>` output: the token
/// after `via` (pure).
pub fn parse_default_gw(output: &str) -> Option<String> {
    let mut tokens = output.split_whitespace();
    while let Some(tok) = tokens.next() {
        if tok == "via" {
            return tokens.next().map(|s| s.to_string());
        }
    }
    None
}

/// Parse `nmcli -t -f DEVICE,TYPE,STATE device status` into the Wi-Fi devices
/// as `(device, state)` pairs (pure).
pub fn parse_wifi_devices(output: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for line in output.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let cols = split_terse(line);
        if cols.len() < 3 {
            continue;
        }
        if cols[1].trim() == "wifi" {
            out.push((cols[0].trim().to_string(), cols[2].trim().to_string()));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wifi_iface_names_are_recognized() {
        assert!(is_wifi_iface("wlan0"));
        assert!(is_wifi_iface("wlp3s0"));
        assert!(is_wifi_iface("wlx00c0ca"));
        assert!(!is_wifi_iface("eth0"));
        assert!(!is_wifi_iface("end0"));
        assert!(!is_wifi_iface("lo"));
    }

    #[test]
    fn radio_drivers_are_flagged_case_insensitively() {
        assert!(is_radio_driver("8812eu"));
        assert!(is_radio_driver("RTL8812AU"));
        assert!(is_radio_driver("rtl88x2eu"));
        // The onboard management-WiFi drivers are not long-range radios.
        assert!(!is_radio_driver("aic8800_fdrv"));
        assert!(!is_radio_driver("brcmfmac"));
        assert!(!is_radio_driver("iwlwifi"));
    }

    #[test]
    fn terse_split_honors_backslash_escape() {
        // A colon inside an SSID is backslash-escaped in nmcli terse output.
        let cols = split_terse(r"home\:5G:82:WPA2:*");
        assert_eq!(cols, vec!["home:5G", "82", "WPA2", "*"]);
        // Trailing empty field is preserved.
        assert_eq!(split_terse("open:70::"), vec!["open", "70", "", ""]);
    }

    #[test]
    fn wifi_list_parses_dedups_and_sorts_by_signal() {
        let out = "\
home-5G:82:WPA2:*
home-2G:60:WPA2:
neighbor:45::
home-5G:70:WPA2:
:33:WPA2:
";
        let nets = parse_wifi_list(out);
        // Strongest first; the blank SSID row is dropped; home-5G de-duped to 82.
        assert_eq!(nets.len(), 3);
        assert_eq!(nets[0].ssid, "home-5G");
        assert_eq!(nets[0].signal, 82);
        assert!(nets[0].secured);
        assert!(nets[0].in_use);
        assert_eq!(nets[1].ssid, "home-2G");
        assert_eq!(nets[2].ssid, "neighbor");
        // "neighbor" has empty SECURITY → open.
        assert!(!nets[2].secured);
    }

    #[test]
    fn egress_iface_parsed_from_both_route_forms() {
        // Direct (on-link) form.
        assert_eq!(
            parse_egress_iface("1.1.1.1 dev eth0 src 192.168.1.42 uid 0"),
            Some("eth0".to_string())
        );
        // Via-gateway form.
        assert_eq!(
            parse_egress_iface("1.1.1.1 via 192.168.1.1 dev wlan0 src 192.168.1.50"),
            Some("wlan0".to_string())
        );
        // No `dev` token → no interface.
        assert_eq!(parse_egress_iface("1.1.1.1 unreachable"), None);
    }

    #[test]
    fn default_gateway_parsed() {
        assert_eq!(
            parse_default_gw("default via 192.168.1.1 proto dhcp metric 600"),
            Some("192.168.1.1".to_string())
        );
        // An on-link default route with no gateway → none.
        assert_eq!(parse_default_gw("default dev eth0 proto dhcp"), None);
    }

    #[test]
    fn wifi_devices_filtered_from_device_status() {
        let out = "\
eth0:ethernet:connected:Wired
wlan0:wifi:disconnected:
wlan1:wifi:connected:home-5G
lo:loopback:unmanaged:
";
        let devs = parse_wifi_devices(out);
        assert_eq!(
            devs,
            vec![
                ("wlan0".to_string(), "disconnected".to_string()),
                ("wlan1".to_string(), "connected".to_string()),
            ]
        );
    }
}
