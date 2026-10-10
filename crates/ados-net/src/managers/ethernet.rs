//! Ethernet uplink manager for the ground-station profile.
//!
//! Most of the work is passive link detection: NetworkManager already brings
//! the wired iface up on cable-plug. This manager reads
//! `/sys/class/net/<iface>/carrier` for liveness, resolves the default gateway
//! via `ip route`, and offers nmcli static/DHCP configuration. It implements
//! the [`UplinkManager`] trait so the router can probe it. Ports
//! `ethernet_manager.py`. Bench-runnable: carrier reads return "down" when the
//! iface is absent, no NIC required.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{json, Value};
use tracing::{info, warn};

use crate::cmd::CmdRunner;
use crate::nmcli;
use crate::router::UplinkManager;

const RUN_TIMEOUT: Duration = Duration::from_secs(5);
const UP_TIMEOUT: Duration = Duration::from_secs(20);
const MODIFY_TIMEOUT: Duration = Duration::from_secs(10);

/// Ethernet link manager. The carrier path is rooted at `sysfs_root` so tests
/// can point it at a tempdir; production uses `/sys`.
///
/// The wired port is identified by device and driver, never by name, and is
/// resolved on every read rather than once at startup: a USB NIC plugged in
/// after the daemon started (or a board whose NIC enumerates late) is picked
/// up on the next tick. An explicit interface pins it instead.
pub struct EthernetManager {
    pinned: Option<String>,
    sysfs_root: PathBuf,
    runner: Arc<dyn CmdRunner>,
}

impl EthernetManager {
    /// Manager that resolves the wired port live from the real `/sys` tree.
    pub fn new(runner: Arc<dyn CmdRunner>) -> Self {
        Self {
            pinned: None,
            sysfs_root: PathBuf::from("/sys"),
            runner,
        }
    }

    /// Manager pinned to `interface`, using the real `/sys` tree.
    pub fn with_interface(interface: impl Into<String>, runner: Arc<dyn CmdRunner>) -> Self {
        Self {
            pinned: Some(interface.into()),
            sysfs_root: PathBuf::from("/sys"),
            runner,
        }
    }

    /// Manager with an explicit sysfs root (tests). `None` resolves the wired
    /// port live under `<sysfs_root>/class/net`.
    pub fn with_sysfs_root(
        interface: Option<&str>,
        sysfs_root: PathBuf,
        runner: Arc<dyn CmdRunner>,
    ) -> Self {
        Self {
            pinned: interface.map(str::to_string),
            sysfs_root,
            runner,
        }
    }

    /// The wired interface right now: the pinned one, else the first physical
    /// wired port by name. Empty when the box has none.
    fn iface(&self) -> String {
        if let Some(pinned) = &self.pinned {
            return pinned.clone();
        }
        ados_protocol::netif::list_wired_in(&self.sysfs_root.join("class/net"))
            .into_iter()
            .next()
            .unwrap_or_default()
    }

    /// Carrier read: true iff the sysfs file holds "1". Absent file or no
    /// wired port → false.
    fn read_carrier(&self, iface: &str) -> bool {
        if iface.is_empty() {
            return false;
        }
        let path = self
            .sysfs_root
            .join("class/net")
            .join(iface)
            .join("carrier");
        std::fs::read_to_string(path)
            .map(|s| s.trim() == "1")
            .unwrap_or(false)
    }

    /// Link speed in Mbps, or `None` when absent / `-1`.
    fn read_speed(&self, iface: &str) -> Option<i64> {
        let path = self.sysfs_root.join("class/net").join(iface).join("speed");
        let raw = std::fs::read_to_string(path).ok()?;
        let val = raw.trim();
        if val.is_empty() || val == "-1" {
            return None;
        }
        val.parse::<i64>().ok()
    }

    /// Resolve the default gateway IPv4 on this iface via `ip route`.
    async fn read_gateway(&self, iface: &str) -> Option<String> {
        if iface.is_empty() {
            return None;
        }
        let out = self
            .runner
            .run(
                &["ip", "-4", "route", "show", "default", "dev", iface],
                RUN_TIMEOUT,
            )
            .await;
        if !out.ok() {
            return None;
        }
        parse_default_via(&out.stdout)
    }

    /// Current IPv4 address on this iface via `ip addr`.
    async fn read_ip(&self, iface: &str) -> Option<String> {
        if iface.is_empty() {
            return None;
        }
        let out = self
            .runner
            .run(&["ip", "-4", "addr", "show", iface], RUN_TIMEOUT)
            .await;
        if !out.ok() {
            return None;
        }
        parse_inet(&out.stdout)
    }

    /// Live link + IP + gateway + speed.
    pub async fn status(&self) -> Value {
        let iface = self.iface();
        let link = self.read_carrier(&iface);
        let speed = if link { self.read_speed(&iface) } else { None };
        let ip = self.read_ip(&iface).await;
        let gateway = self.read_gateway(&iface).await;
        json!({
            "link": link,
            "speed_mbps": speed,
            "ip": ip,
            "gateway": gateway,
        })
    }

    /// Discover the primary NM ethernet connection name. Prefers an ACTIVE ethernet
    /// connection on this iface; else the first ethernet profile on this device; else
    /// the first ethernet profile. Returns the connection name only.
    async fn discover_primary_connection(&self) -> Option<String> {
        let iface = self.iface();
        let show = self
            .runner
            .run(
                &[
                    "nmcli",
                    "-t",
                    "-f",
                    "NAME,TYPE,DEVICE",
                    "connection",
                    "show",
                ],
                RUN_TIMEOUT,
            )
            .await;
        let mut saved: Vec<(String, String)> = Vec::new();
        if show.ok() {
            for row in nmcli::parse_terse(&show.stdout, 3) {
                if row[1] == "802-3-ethernet" {
                    saved.push((row[0].clone(), row[2].clone()));
                }
            }
        }

        let active = self
            .runner
            .run(
                &[
                    "nmcli",
                    "-t",
                    "-f",
                    "NAME,TYPE,DEVICE",
                    "connection",
                    "show",
                    "--active",
                ],
                RUN_TIMEOUT,
            )
            .await;
        let mut active_names: std::collections::HashSet<String> = std::collections::HashSet::new();
        if active.ok() {
            for row in nmcli::parse_terse(&active.stdout, 1) {
                if !row[0].is_empty() {
                    active_names.insert(row[0].clone());
                }
            }
        }

        for (name, dev) in &saved {
            if active_names.contains(name) && (*dev == iface || dev.is_empty()) {
                return Some(name.clone());
            }
        }
        for (name, dev) in &saved {
            if *dev == iface {
                return Some(name.clone());
            }
        }
        if let Some((name, _dev)) = saved.first() {
            return Some(name.clone());
        }
        warn!(interface = %iface, "no_nm_ethernet_connection");
        None
    }

    /// Apply static IPv4 via nmcli on the primary connection.
    pub async fn configure_static(&self, ip: &str, gateway: &str, dns: &[String]) -> Value {
        let name = match self.discover_primary_connection().await {
            Some(n) => n,
            None => {
                return json!({
                    "ok": false,
                    "error": "no_ethernet_connection",
                    "hint": "No saved NetworkManager Ethernet connection found",
                });
            }
        };
        let dns_str = dns.join(" ");
        let modify = self
            .runner
            .run(
                &[
                    "nmcli",
                    "connection",
                    "modify",
                    &name,
                    "ipv4.method",
                    "manual",
                    "ipv4.addresses",
                    ip,
                    "ipv4.gateway",
                    gateway,
                    "ipv4.dns",
                    &dns_str,
                ],
                MODIFY_TIMEOUT,
            )
            .await;
        if !modify.ok() {
            warn!(name = %name, "ethernet_static_modify_failed");
            return json!({"ok": false, "error": err_or(&modify.stderr, "nmcli_modify_failed")});
        }
        let up = self
            .runner
            .run(&["nmcli", "connection", "up", &name], UP_TIMEOUT)
            .await;
        if !up.ok() {
            warn!(name = %name, "ethernet_up_failed");
            return json!({"ok": false, "error": err_or(&up.stderr, "nmcli_up_failed")});
        }
        info!(name = %name, ip = ip, gateway = gateway, "ethernet_configured_static");
        json!({"mode": "static", "ip": ip, "gateway": gateway, "dns": dns, "ok": true})
    }

    /// The persisted-profile config view backing `GET .../network/ethernet` and
    /// the success body of the ethernet PUT. The mode + static
    /// fields come from the NM connection PROFILE (`nmcli -t -f
    /// ipv4.method,ipv4.addresses,ipv4.gateway,ipv4.dns connection show <name>`,
    /// so the UI reflects what applies on next reconnect, not the runtime `ip
    /// addr`), with the live link + current IP merged in from `status`. Returns
    /// `{mode, connection_name, ip, gateway, dns, link, speed_mbps, current_ip,
    /// current_gateway}`. A board with no ethernet profile reports the defaults
    /// (mode dhcp, null profile fields) plus the live link legs.
    pub async fn config(&self) -> Value {
        let name = self.discover_primary_connection().await;
        let mut mode = "dhcp".to_string();
        let mut profile_ip: Option<String> = None;
        let mut profile_gateway: Option<String> = None;
        let mut profile_dns: Vec<String> = Vec::new();

        if let Some(ref n) = name {
            let out = self
                .runner
                .run(
                    &[
                        "nmcli",
                        "-t",
                        "-f",
                        "ipv4.method,ipv4.addresses,ipv4.gateway,ipv4.dns",
                        "connection",
                        "show",
                        n,
                    ],
                    RUN_TIMEOUT,
                )
                .await;
            if out.ok() {
                for line in out.stdout.lines() {
                    // Mirror Python `line.partition(":")`: split on the FIRST ':'.
                    let Some((key, val)) = line.split_once(':') else {
                        continue;
                    };
                    let val = val.trim();
                    match key {
                        "ipv4.method" => {
                            mode = if val == "manual" {
                                "static".to_string()
                            } else {
                                "dhcp".to_string()
                            };
                        }
                        "ipv4.addresses" => {
                            profile_ip = (!val.is_empty()).then(|| val.to_string());
                        }
                        "ipv4.gateway" => {
                            profile_gateway = (!val.is_empty()).then(|| val.to_string());
                        }
                        "ipv4.dns" => {
                            profile_dns = if val.is_empty() {
                                Vec::new()
                            } else {
                                val.split(',')
                                    .filter(|d| !d.is_empty())
                                    .map(str::to_string)
                                    .collect()
                            };
                        }
                        _ => {}
                    }
                }
            }
        }

        let live = self.status().await;
        json!({
            "mode": mode,
            "connection_name": name,
            "ip": profile_ip,
            "gateway": profile_gateway,
            "dns": profile_dns,
            "link": live.get("link").and_then(Value::as_bool).unwrap_or(false),
            "speed_mbps": live.get("speed_mbps").cloned().unwrap_or(Value::Null),
            "current_ip": live.get("ip").cloned().unwrap_or(Value::Null),
            "current_gateway": live.get("gateway").cloned().unwrap_or(Value::Null),
        })
    }

    /// Reset the primary connection to DHCP via nmcli.
    pub async fn configure_dhcp(&self) -> Value {
        let name = match self.discover_primary_connection().await {
            Some(n) => n,
            None => {
                return json!({
                    "ok": false,
                    "error": "no_ethernet_connection",
                    "hint": "No saved NetworkManager Ethernet connection found",
                });
            }
        };
        let modify = self
            .runner
            .run(
                &[
                    "nmcli",
                    "connection",
                    "modify",
                    &name,
                    "ipv4.method",
                    "auto",
                    "ipv4.addresses",
                    "",
                    "ipv4.gateway",
                    "",
                    "ipv4.dns",
                    "",
                ],
                MODIFY_TIMEOUT,
            )
            .await;
        if !modify.ok() {
            warn!(name = %name, "ethernet_dhcp_modify_failed");
            return json!({"ok": false, "error": err_or(&modify.stderr, "nmcli_modify_failed")});
        }
        let up = self
            .runner
            .run(&["nmcli", "connection", "up", &name], UP_TIMEOUT)
            .await;
        if !up.ok() {
            warn!(name = %name, "ethernet_up_failed");
            return json!({"ok": false, "error": err_or(&up.stderr, "nmcli_up_failed")});
        }
        info!(name = %name, "ethernet_configured_dhcp");
        json!({"mode": "dhcp", "ok": true})
    }
}

#[async_trait]
impl UplinkManager for EthernetManager {
    async fn is_up(&self) -> bool {
        self.read_carrier(&self.iface())
    }
    fn get_iface(&self) -> String {
        self.iface()
    }
    async fn get_gateway(&self) -> Option<String> {
        self.read_gateway(&self.iface()).await
    }
}

/// First `inet A.B.C.D` IPv4 in `ip -4 addr show` output. Shared with the
/// wifi-client manager, which parses the same `ip` output shape.
pub(crate) fn parse_inet(text: &str) -> Option<String> {
    for token in text.split_whitespace().collect::<Vec<_>>().windows(2) {
        if token[0] == "inet" {
            // Strip the CIDR suffix (`/24`).
            let ip = token[1].split('/').next().unwrap_or(token[1]);
            if is_ipv4(ip) {
                return Some(ip.to_string());
            }
        }
    }
    None
}

/// The `via A.B.C.D` gateway in a `default via ...` route line. Shared with the
/// wifi-client manager.
pub(crate) fn parse_default_via(text: &str) -> Option<String> {
    for line in text.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.first() == Some(&"default") {
            if let Some(idx) = parts.iter().position(|p| *p == "via") {
                if let Some(gw) = parts.get(idx + 1) {
                    if is_ipv4(gw) {
                        return Some((*gw).to_string());
                    }
                }
            }
        }
    }
    None
}

fn is_ipv4(s: &str) -> bool {
    let octets: Vec<&str> = s.split('.').collect();
    octets.len() == 4
        && octets
            .iter()
            .all(|o| !o.is_empty() && o.parse::<u8>().is_ok())
}

fn err_or(stderr: &str, fallback: &str) -> String {
    let t = stderr.trim();
    if t.is_empty() {
        fallback.to_string()
    } else {
        t.to_string()
    }
}

#[cfg(test)]
#[path = "ethernet_tests.rs"]
mod tests;
