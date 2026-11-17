//! Bring a host's Wi-Fi under NetworkManager without touching its wired link.
//!
//! The wizard's Wi-Fi stage, the headless `--wifi-ssid` join and the agent's
//! own Wi-Fi surfaces all drive `nmcli`. Plenty of SBC images run a different
//! stack: netplan rendering to systemd-networkd, bare systemd-networkd,
//! ifupdown, or dhcpcd. This module reads which stack owns what ([`probe`]),
//! decides the smallest change that hands the management Wi-Fi to
//! NetworkManager ([`plan`], pure and unit-tested), and carries it out
//! ([`ensure_networkmanager`]).
//!
//! The rule that shapes every decision: the wired link the install (and often
//! the operator's SSH session) runs over is never reconfigured live. Wired
//! interfaces another stack owns are marked unmanaged for NetworkManager. On a
//! netplan host the renderer switch is written for the next boot only, so the
//! wired link moves to NetworkManager at the reboot the installer already
//! performs, never mid-install.

mod adopt;

use std::path::Path;

use ados_term::exec;

pub use adopt::{ensure_networkmanager, render_handoff_conf, wifi_uplink_in_use, Adopted};

/// The netplan drop-in that switches the renderer to NetworkManager.
pub const NETPLAN_NM_PATH: &str = "/etc/netplan/90-ados-networkmanager.yaml";

/// What the probe found on this host. Every field tolerates a missing binary
/// or file by reading as false / empty.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NetStack {
    /// The NetworkManager daemon is installed.
    pub nm_installed: bool,
    /// The NetworkManager service is running.
    pub nm_active: bool,
    /// `netplan get renderer`, when netplan is present and configured.
    pub netplan_renderer: Option<String>,
    /// The renderer switch this module writes is already on disk.
    pub ados_netplan_written: bool,
    /// systemd-networkd links as `(name, SETUP)`.
    pub networkd_links: Vec<(String, String)>,
    /// Interfaces declared in `/etc/network/interfaces{,.d/*}`, `lo` excluded.
    pub ifupdown_ifaces: Vec<String>,
    /// The dhcpcd service is running.
    pub dhcpcd_active: bool,
    /// Physical interfaces with no wireless extension.
    pub wired_ifaces: Vec<String>,
    /// Every interface with a wireless extension, long-range radios included.
    pub wireless_ifaces: Vec<String>,
    /// The management Wi-Fi interface: the first wireless interface whose
    /// driver is not a long-range-radio driver.
    pub wifi_iface: Option<String>,
    /// `wpa_supplicant@<wifi_iface>` is enabled (it would fight NetworkManager
    /// for the radio).
    pub wpa_iface_unit_enabled: bool,
}

/// True for a networkd SETUP state that means networkd is running the link.
fn networkd_owns(setup: &str) -> bool {
    matches!(setup, "configured" | "configuring")
}

impl NetStack {
    /// A short, operator-facing name for the stack that runs this host.
    pub fn label(&self) -> &'static str {
        if self.nm_active {
            "NetworkManager"
        } else if self.netplan_renderer.as_deref() == Some("networkd") {
            "netplan + systemd-networkd"
        } else if self.networkd_links.iter().any(|(_, s)| networkd_owns(s)) {
            "systemd-networkd"
        } else if self.dhcpcd_active {
            "dhcpcd"
        } else if !self.ifupdown_ifaces.is_empty() {
            "ifupdown"
        } else {
            "no network manager"
        }
    }

    fn is_wireless(&self, iface: &str) -> bool {
        self.wireless_ifaces.iter().any(|w| w == iface)
    }

    fn networkd_owns_iface(&self, iface: &str) -> bool {
        self.networkd_links
            .iter()
            .any(|(n, s)| n == iface && networkd_owns(s))
    }
}

/// The changes [`ensure_networkmanager`] makes, decided from a [`NetStack`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AdoptionPlan {
    /// Install the NetworkManager package.
    pub install_nm: bool,
    /// Enable and start the NetworkManager service.
    pub start_nm: bool,
    /// Switch the netplan renderer to NetworkManager for the next boot.
    pub netplan_switch: bool,
    /// The renderer switch is on disk but networkd still runs a wired link,
    /// so the boot that completes it has not happened yet.
    pub netplan_reboot_pending: bool,
    /// Wired interfaces another stack runs, kept unmanaged by NetworkManager.
    pub handoff_ifaces: Vec<String>,
    /// The handoff lives in `/run`: it is needed only until the reboot that
    /// completes the netplan switch.
    pub handoff_ephemeral: bool,
    /// Take the Wi-Fi interface away from systemd-networkd.
    pub networkd_release_wifi: Option<String>,
    /// Comment the Wi-Fi interface out of ifupdown.
    pub ifupdown_release_wifi: Option<String>,
    /// Tell dhcpcd to leave the Wi-Fi interface alone.
    pub dhcpcd_deny_wifi: Option<String>,
    /// Disable `wpa_supplicant@<iface>`.
    pub disable_wpa_unit: Option<String>,
    /// Disable `systemd-networkd-wait-online` (networkd no longer runs any
    /// link after the switch, so it would stall every boot).
    pub disable_networkd_wait_online: bool,
    /// The management Wi-Fi interface NetworkManager must end up managing.
    pub wifi_iface: Option<String>,
}

impl AdoptionPlan {
    /// True when nothing needs changing. A pending reboot is not a change.
    pub fn noop(&self) -> bool {
        !self.install_nm
            && !self.start_nm
            && !self.netplan_switch
            && self.handoff_ifaces.is_empty()
            && self.networkd_release_wifi.is_none()
            && self.ifupdown_release_wifi.is_none()
            && self.dhcpcd_deny_wifi.is_none()
            && self.disable_wpa_unit.is_none()
            && !self.disable_networkd_wait_online
    }
}

/// Decide the adoption from what the probe found (pure).
pub fn plan(s: &NetStack) -> AdoptionPlan {
    let netplan_switch = s.netplan_renderer.as_deref() == Some("networkd");
    let wifi = s.wifi_iface.clone();

    let mut handoff: Vec<String> = Vec::new();
    if !(s.nm_active && !netplan_switch) {
        for (name, setup) in &s.networkd_links {
            if networkd_owns(setup) && !s.is_wireless(name) {
                handoff.push(name.clone());
            }
        }
        for name in &s.ifupdown_ifaces {
            if !s.is_wireless(name) {
                handoff.push(name.clone());
            }
        }
        if s.dhcpcd_active {
            handoff.extend(s.wired_ifaces.iter().cloned());
        }
        handoff.sort();
        handoff.dedup();
    }

    let netplan_reboot_pending = s.ados_netplan_written
        && s.netplan_renderer.as_deref() == Some("NetworkManager")
        && s.networkd_links
            .iter()
            .any(|(n, setup)| networkd_owns(setup) && !s.is_wireless(n));

    AdoptionPlan {
        install_nm: !s.nm_installed,
        start_nm: !s.nm_active,
        netplan_switch,
        netplan_reboot_pending,
        handoff_ifaces: handoff,
        handoff_ephemeral: netplan_switch,
        networkd_release_wifi: wifi.clone().filter(|w| s.networkd_owns_iface(w)),
        ifupdown_release_wifi: wifi
            .clone()
            .filter(|w| s.ifupdown_ifaces.iter().any(|i| i == w)),
        dhcpcd_deny_wifi: wifi.clone().filter(|_| s.dhcpcd_active),
        disable_wpa_unit: wifi.clone().filter(|_| s.wpa_iface_unit_enabled),
        disable_networkd_wait_online: netplan_switch,
        wifi_iface: wifi,
    }
}

/// Read the host's network stack. Never fails: an absent tool reads as
/// "not in use".
pub fn probe() -> NetStack {
    let mut s = NetStack {
        nm_installed: Path::new("/usr/sbin/NetworkManager").exists(),
        nm_active: unit_active("NetworkManager"),
        ados_netplan_written: Path::new(NETPLAN_NM_PATH).exists(),
        dhcpcd_active: unit_active("dhcpcd"),
        ..NetStack::default()
    };
    if netplan_configured() {
        let res = exec::run("netplan", &["get", "renderer"]);
        let r = res.stdout.trim();
        if res.success() && !r.is_empty() && r != "null" {
            s.netplan_renderer = Some(r.to_string());
        }
    }
    let res = exec::run("networkctl", &["list", "--no-legend", "--no-pager"]);
    if res.success() {
        s.networkd_links = parse_networkctl(&res.stdout);
    }
    s.ifupdown_ifaces = ifupdown_files()
        .iter()
        .filter_map(|p| std::fs::read_to_string(p).ok())
        .flat_map(|body| parse_ifupdown(&body))
        .fold(Vec::new(), |mut acc, i| {
            if !acc.contains(&i) {
                acc.push(i);
            }
            acc
        });
    let mut names: Vec<String> = std::fs::read_dir("/sys/class/net")
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    for name in names {
        let base = Path::new("/sys/class/net").join(&name);
        if base.join("wireless").is_dir() {
            let radio = crate::wizard::wifi::iface_driver(&name)
                .map(|d| crate::wizard::wifi::is_radio_driver(&d))
                .unwrap_or(false);
            if !radio && s.wifi_iface.is_none() {
                s.wifi_iface = Some(name.clone());
            }
            s.wireless_ifaces.push(name);
        } else if base.join("device").exists() {
            s.wired_ifaces.push(name);
        }
    }
    if let Some(w) = &s.wifi_iface {
        let unit = format!("wpa_supplicant@{w}");
        s.wpa_iface_unit_enabled =
            exec::run("systemctl", &["is-enabled", &unit]).stdout.trim() == "enabled";
    }
    s
}

fn unit_active(unit: &str) -> bool {
    exec::run("systemctl", &["is-active", unit]).stdout.trim() == "active"
}

/// netplan is installed and `/etc/netplan` holds at least one non-empty YAML.
fn netplan_configured() -> bool {
    let has_bin = ["/usr/sbin/netplan", "/usr/bin/netplan"]
        .iter()
        .any(|p| Path::new(p).exists());
    has_bin
        && std::fs::read_dir("/etc/netplan")
            .map(|rd| {
                rd.filter_map(|e| e.ok()).any(|e| {
                    let p = e.path();
                    p.extension().is_some_and(|x| x == "yaml")
                        && std::fs::metadata(&p).map(|m| m.len() > 0).unwrap_or(false)
                })
            })
            .unwrap_or(false)
}

/// `/etc/network/interfaces` and every file in `/etc/network/interfaces.d`.
pub(crate) fn ifupdown_files() -> Vec<std::path::PathBuf> {
    let mut files = vec![std::path::PathBuf::from("/etc/network/interfaces")];
    if let Ok(rd) = std::fs::read_dir("/etc/network/interfaces.d") {
        let mut extra: Vec<_> = rd
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.is_file())
            .collect();
        extra.sort();
        files.extend(extra);
    }
    files.retain(|p| p.is_file());
    files
}

/// Parse `networkctl list --no-legend --no-pager` (columns `IDX LINK TYPE
/// OPERATIONAL SETUP`) into `(link, setup)` pairs (pure).
pub fn parse_networkctl(out: &str) -> Vec<(String, String)> {
    out.lines()
        .filter_map(|line| {
            let cols: Vec<&str> = line.split_whitespace().collect();
            (cols.len() >= 5).then(|| (cols[1].to_string(), cols[4].to_string()))
        })
        .collect()
}

/// The interfaces an ifupdown file declares with `iface <name> inet|inet6`,
/// `lo` excluded, in file order without repeats (pure).
pub fn parse_ifupdown(body: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for line in body.lines() {
        let toks: Vec<&str> = line.split_whitespace().collect();
        if toks.len() >= 3
            && toks[0] == "iface"
            && matches!(toks[2], "inet" | "inet6")
            && toks[1] != "lo"
            && !out.iter().any(|o| o == toks[1])
        {
            out.push(toks[1].to_string());
        }
    }
    out
}

#[cfg(test)]
mod tests;
