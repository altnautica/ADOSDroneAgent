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

use ados_term::exec;

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
pub(crate) fn iface_driver(iface: &str) -> Option<String> {
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
///
/// The passphrase never rides the command line: nmcli runs with `--ask` and
/// reads it from stdin, so it is not visible in `/proc/<pid>/cmdline` or in
/// the exec log.
pub fn connect(
    iface: &str,
    ssid: &str,
    password: Option<&str>,
    hidden: bool,
) -> Result<(), String> {
    let mut args: Vec<&str> = vec!["--ask", "device", "wifi", "connect", ssid, "ifname", iface];
    if hidden {
        args.extend(["hidden", "yes"]);
    }
    let input = password.map(|pw| format!("{pw}\n")).unwrap_or_default();
    let res = exec::run_with_stdin("nmcli", &args, input.as_bytes());
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

/// Which of the saved networks a profile is. NetworkManager joins the highest
/// autoconnect priority in range, so the backup is used only when the main
/// network is out of reach.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WifiRole {
    Primary,
    Backup,
}

impl WifiRole {
    /// The NetworkManager `connection.autoconnect-priority` for this role.
    pub fn priority(self) -> &'static str {
        match self {
            WifiRole::Primary => "10",
            WifiRole::Backup => "5",
        }
    }
}

/// Persist the joined profile for the next boot: auto-connect on with the
/// role's priority, a route metric higher than wired so a plugged cable stays
/// primary, and Wi-Fi power-save off so the radio never parks the uplink.
///
/// The profile is not pinned to an interface name or an access point: kernel
/// names race at boot (a long-range radio can come up as `wlan0`), and a
/// network with several access points must stay joinable through any of them.
/// NetworkManager names the profile after the SSID. Best-effort — a failure
/// here only affects the next boot.
pub fn persist(ssid: &str, role: WifiRole) {
    let settings: [(&str, &str); 7] = [
        ("connection.autoconnect", "yes"),
        ("connection.autoconnect-priority", role.priority()),
        ("connection.interface-name", ""),
        ("802-11-wireless.bssid", ""),
        // A higher metric = lower priority: wired (default ~100) wins when present.
        ("ipv4.route-metric", "700"),
        ("ipv6.route-metric", "700"),
        // 2 = power-save disabled.
        ("802-11-wireless.powersave", "2"),
    ];
    for (key, value) in settings {
        let _ = exec::run("nmcli", &["connection", "modify", ssid, key, value]);
    }
}

/// Bring the saved profile for `ssid` up (switch back to it).
pub fn activate(ssid: &str) -> Result<(), String> {
    let res = exec::run("nmcli", &["connection", "up", "id", ssid]);
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

/// The IPv4 address on `iface`, without its prefix length, or `None`.
pub fn iface_ipv4(iface: &str) -> Option<String> {
    let res = exec::run("ip", &["-4", "-o", "addr", "show", "dev", iface]);
    if res.success() {
        parse_ipv4_addr(&res.stdout)
    } else {
        None
    }
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

/// Parse the first address from `ip -4 -o addr show dev <if>` output: the
/// token after `inet`, without its `/prefix` (pure).
pub fn parse_ipv4_addr(output: &str) -> Option<String> {
    let mut tokens = output.split_whitespace();
    while let Some(tok) = tokens.next() {
        if tok == "inet" {
            return tokens
                .next()
                .and_then(|a| a.split('/').next())
                .filter(|a| !a.is_empty())
                .map(str::to_string);
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
mod tests;
