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
pub struct EthernetManager {
    interface: String,
    sysfs_root: PathBuf,
    runner: Arc<dyn CmdRunner>,
}

impl EthernetManager {
    /// Manager for `interface`, using the real `/sys` tree.
    pub fn new(interface: impl Into<String>, runner: Arc<dyn CmdRunner>) -> Self {
        Self {
            interface: interface.into(),
            sysfs_root: PathBuf::from("/sys"),
            runner,
        }
    }

    /// Manager with an explicit sysfs root (tests).
    pub fn with_sysfs_root(
        interface: impl Into<String>,
        sysfs_root: PathBuf,
        runner: Arc<dyn CmdRunner>,
    ) -> Self {
        Self {
            interface: interface.into(),
            sysfs_root,
            runner,
        }
    }

    fn carrier_path(&self) -> PathBuf {
        self.sysfs_root
            .join("class/net")
            .join(&self.interface)
            .join("carrier")
    }

    fn speed_path(&self) -> PathBuf {
        self.sysfs_root
            .join("class/net")
            .join(&self.interface)
            .join("speed")
    }

    /// Carrier read: true iff the sysfs file holds "1". Absent file → false.
    fn read_carrier(&self) -> bool {
        std::fs::read_to_string(self.carrier_path())
            .map(|s| s.trim() == "1")
            .unwrap_or(false)
    }

    /// Link speed in Mbps, or `None` when absent / `-1`.
    fn read_speed(&self) -> Option<i64> {
        let raw = std::fs::read_to_string(self.speed_path()).ok()?;
        let val = raw.trim();
        if val.is_empty() || val == "-1" {
            return None;
        }
        val.parse::<i64>().ok()
    }

    /// Resolve the default gateway IPv4 on this iface via `ip route`.
    async fn read_gateway(&self) -> Option<String> {
        let out = self
            .runner
            .run(
                &[
                    "ip",
                    "-4",
                    "route",
                    "show",
                    "default",
                    "dev",
                    &self.interface,
                ],
                RUN_TIMEOUT,
            )
            .await;
        if !out.ok() {
            return None;
        }
        parse_default_via(&out.stdout)
    }

    /// Current IPv4 address on this iface via `ip addr`.
    async fn read_ip(&self) -> Option<String> {
        let out = self
            .runner
            .run(&["ip", "-4", "addr", "show", &self.interface], RUN_TIMEOUT)
            .await;
        if !out.ok() {
            return None;
        }
        parse_inet(&out.stdout)
    }

    /// Live link + IP + gateway + speed. Mirrors `status`.
    pub async fn status(&self) -> Value {
        let link = self.read_carrier();
        let speed = if link { self.read_speed() } else { None };
        let ip = self.read_ip().await;
        let gateway = self.read_gateway().await;
        json!({
            "link": link,
            "speed_mbps": speed,
            "ip": ip,
            "gateway": gateway,
        })
    }

    /// Discover the primary NM ethernet connection name. Prefers an ACTIVE
    /// ethernet connection on this iface; else the first ethernet profile on
    /// this device; else the first ethernet profile. Mirrors
    /// `_discover_primary_connection` (returns the connection name only).
    async fn discover_primary_connection(&self) -> Option<String> {
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
            if active_names.contains(name) && (dev == &self.interface || dev.is_empty()) {
                return Some(name.clone());
            }
        }
        for (name, dev) in &saved {
            if dev == &self.interface {
                return Some(name.clone());
            }
        }
        if let Some((name, _dev)) = saved.first() {
            return Some(name.clone());
        }
        warn!(interface = %self.interface, "no_nm_ethernet_connection");
        None
    }

    /// Apply static IPv4 via nmcli on the primary connection. Mirrors
    /// `configure_static`.
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
    /// the success body of the ethernet PUT. Mirrors `config`: the mode + static
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

    /// Reset the primary connection to DHCP via nmcli. Mirrors `configure_dhcp`.
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
        self.read_carrier()
    }
    fn get_iface(&self) -> String {
        self.interface.clone()
    }
    async fn get_gateway(&self) -> Option<String> {
        self.read_gateway().await
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
mod tests {
    use super::*;
    use crate::cmd::testing::ScriptedRunner;
    use crate::cmd::CmdOut;

    fn write_carrier(root: &std::path::Path, iface: &str, val: &str) {
        let dir = root.join("class/net").join(iface);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("carrier"), val).unwrap();
    }

    #[tokio::test]
    async fn is_up_reads_carrier_and_absent_is_down() {
        let dir = tempfile::tempdir().unwrap();
        let runner = Arc::new(ScriptedRunner::new());
        let m = EthernetManager::with_sysfs_root("eth0", dir.path().to_path_buf(), runner.clone());
        // Absent carrier → down (bench-runnable).
        assert!(!m.is_up().await);
        write_carrier(dir.path(), "eth0", "1\n");
        assert!(m.is_up().await);
        write_carrier(dir.path(), "eth0", "0\n");
        assert!(!m.is_up().await);
        assert_eq!(m.get_iface(), "eth0");
    }

    #[test]
    fn parse_inet_and_default_via() {
        let addr = "    inet 192.168.1.50/24 brd 192.168.1.255 scope global dynamic eth0\n       valid_lft 86399sec preferred_lft 86399sec";
        assert_eq!(parse_inet(addr).as_deref(), Some("192.168.1.50"));
        let route = "default via 192.168.1.1 dev eth0 proto dhcp metric 100";
        assert_eq!(parse_default_via(route).as_deref(), Some("192.168.1.1"));
        // No default route → None.
        assert!(parse_default_via("10.0.0.0/24 dev eth0 scope link").is_none());
    }

    #[tokio::test]
    async fn get_gateway_uses_ip_route() {
        let runner = Arc::new(ScriptedRunner::new());
        runner.push(CmdOut {
            rc: 0,
            stdout: "default via 10.0.0.1 dev eth0\n".to_string(),
            stderr: String::new(),
        });
        let m = EthernetManager::new("eth0", runner.clone());
        assert_eq!(m.get_gateway().await.as_deref(), Some("10.0.0.1"));
        // It invoked `ip -4 route show default dev eth0`.
        let calls = runner.recorded();
        assert_eq!(calls[0][0], "ip");
        assert!(calls[0].contains(&"default".to_string()));
    }

    #[tokio::test]
    async fn discover_primary_prefers_active_then_device_then_first() {
        let runner = Arc::new(ScriptedRunner::new());
        // `connection show` → two ethernet profiles.
        runner.push(CmdOut {
            rc: 0,
            stdout: "Saved Wired:802-3-ethernet:\nWired connection 1:802-3-ethernet:eth0\nMyWifi:802-11-wireless:wlan0\n".to_string(),
            stderr: String::new(),
        });
        // `--active` → "Wired connection 1" is active.
        runner.push(CmdOut {
            rc: 0,
            stdout: "Wired connection 1:802-3-ethernet:eth0\n".to_string(),
            stderr: String::new(),
        });
        let m = EthernetManager::new("eth0", runner.clone());
        assert_eq!(
            m.discover_primary_connection().await.as_deref(),
            Some("Wired connection 1")
        );
    }

    #[tokio::test]
    async fn status_surfaces_link_ip_gateway_and_speed() {
        let dir = tempfile::tempdir().unwrap();
        write_carrier(dir.path(), "eth0", "1\n");
        let speed_dir = dir.path().join("class/net").join("eth0");
        std::fs::write(speed_dir.join("speed"), "1000\n").unwrap();
        let runner = Arc::new(ScriptedRunner::new());
        // status() calls ip addr then ip route.
        runner.push(CmdOut {
            rc: 0,
            stdout: "    inet 10.10.0.5/24 scope global eth0\n".to_string(),
            stderr: String::new(),
        }); // ip addr
        runner.push(CmdOut {
            rc: 0,
            stdout: "default via 10.10.0.1 dev eth0\n".to_string(),
            stderr: String::new(),
        }); // ip route (status)
        runner.push(CmdOut {
            rc: 0,
            stdout: "default via 10.10.0.1 dev eth0\n".to_string(),
            stderr: String::new(),
        }); // ip route (the follow-up get_gateway call)
        let m = EthernetManager::with_sysfs_root("eth0", dir.path().to_path_buf(), runner);
        let s = m.status().await;
        assert_eq!(s["link"], true);
        assert_eq!(s["speed_mbps"], 1000);
        assert_eq!(s["ip"], "10.10.0.5");
        assert_eq!(s["gateway"], "10.10.0.1");
        // is_up() reflects the carrier (a sysfs read, no nmcli), get_gateway()
        // the resolved route.
        assert!(m.is_up().await);
        assert_eq!(m.get_gateway().await.as_deref(), Some("10.10.0.1"));
    }

    #[tokio::test]
    async fn status_reports_no_link_when_carrier_down() {
        // Carrier down → link false, speed null (speed is only read when up).
        let dir = tempfile::tempdir().unwrap();
        write_carrier(dir.path(), "eth0", "0\n");
        let runner = Arc::new(ScriptedRunner::new());
        runner.push(CmdOut::failed(1, "")); // ip addr (no address)
        runner.push(CmdOut::failed(1, "")); // ip route (no default)
        let m = EthernetManager::with_sysfs_root("eth0", dir.path().to_path_buf(), runner);
        let s = m.status().await;
        assert_eq!(s["link"], false);
        assert!(s["speed_mbps"].is_null());
        assert!(s["ip"].is_null());
        assert!(s["gateway"].is_null());
        assert!(!m.is_up().await);
    }

    #[tokio::test]
    async fn configure_static_with_no_ethernet_connection_reports_error() {
        // No ethernet profile on the box → configure_static returns a clean
        // error dict, never a panic.
        let runner = Arc::new(ScriptedRunner::new());
        // discover_primary_connection: `connection show` (no ethernet rows) +
        // `--active` (empty).
        runner.push(CmdOut {
            rc: 0,
            stdout: "MyWifi:802-11-wireless:wlan0\n".to_string(),
            stderr: String::new(),
        });
        runner.push(CmdOut::failed(0, ""));
        let m = EthernetManager::new("eth0", runner);
        let res = m
            .configure_static("10.0.0.5/24", "10.0.0.1", &["1.1.1.1".to_string()])
            .await;
        assert_eq!(res["ok"], false);
        assert_eq!(res["error"], "no_ethernet_connection");
    }

    #[tokio::test]
    async fn configure_static_modifies_then_ups() {
        let runner = Arc::new(ScriptedRunner::new());
        // discover: connection show + active.
        runner.push(CmdOut {
            rc: 0,
            stdout: "Wired:802-3-ethernet:eth0\n".to_string(),
            stderr: String::new(),
        });
        runner.push(CmdOut::failed(0, "")); // --active (empty)
        runner.push(CmdOut::failed(0, "")); // modify ok
        runner.push(CmdOut::failed(0, "")); // up ok
        let m_runner = Arc::clone(&runner);
        let m = EthernetManager::new("eth0", runner);
        let res = m
            .configure_static("10.0.0.5/24", "10.0.0.1", &["8.8.8.8".to_string()])
            .await;
        assert_eq!(res["ok"], true);
        assert_eq!(res["mode"], "static");
        assert_eq!(res["ip"], "10.0.0.5/24");
        // The modify call carried the static address + gateway + dns.
        let calls = m_runner.recorded();
        let modify = calls
            .iter()
            .find(|c| c.iter().any(|a| a == "modify"))
            .expect("a modify call");
        assert!(modify.iter().any(|a| a == "manual"));
        assert!(modify.iter().any(|a| a == "10.0.0.5/24"));
        assert!(modify.iter().any(|a| a == "10.0.0.1"));
        assert!(modify.iter().any(|a| a == "8.8.8.8"));
    }

    #[tokio::test]
    async fn configure_dhcp_modifies_then_ups() {
        let runner = Arc::new(ScriptedRunner::new());
        // discover: connection show + active.
        runner.push(CmdOut {
            rc: 0,
            stdout: "Wired:802-3-ethernet:eth0\n".to_string(),
            stderr: String::new(),
        });
        runner.push(CmdOut::failed(0, "")); // --active (empty)
        runner.push(CmdOut::failed(0, "")); // modify ok
        runner.push(CmdOut::failed(0, "")); // up ok
        let m = EthernetManager::new("eth0", runner.clone());
        let res = m.configure_dhcp().await;
        assert_eq!(res["ok"], true);
        assert_eq!(res["mode"], "dhcp");
    }

    #[tokio::test]
    async fn no_ethernet_connection_error_carries_the_hint() {
        // The no-connection error carries the same `hint` the Python manager
        // returns, so the PUT route's error body matches byte-for-byte.
        let runner = Arc::new(ScriptedRunner::new());
        runner.push(CmdOut {
            rc: 0,
            stdout: "MyWifi:802-11-wireless:wlan0\n".to_string(),
            stderr: String::new(),
        });
        runner.push(CmdOut::failed(0, "")); // --active empty
        let m = EthernetManager::new("eth0", runner);
        let res = m.configure_dhcp().await;
        assert_eq!(res["ok"], false);
        assert_eq!(res["error"], "no_ethernet_connection");
        assert_eq!(
            res["hint"],
            "No saved NetworkManager Ethernet connection found"
        );
    }

    #[tokio::test]
    async fn config_reads_the_static_profile_and_merges_live_link() {
        let dir = tempfile::tempdir().unwrap();
        write_carrier(dir.path(), "eth0", "1\n");
        let speed_dir = dir.path().join("class/net").join("eth0");
        std::fs::write(speed_dir.join("speed"), "1000\n").unwrap();
        let runner = Arc::new(ScriptedRunner::new());
        // discover_primary_connection: connection show + active.
        runner.push(CmdOut {
            rc: 0,
            stdout: "Wired:802-3-ethernet:eth0\n".to_string(),
            stderr: String::new(),
        });
        runner.push(CmdOut {
            rc: 0,
            stdout: "Wired:802-3-ethernet:eth0\n".to_string(),
            stderr: String::new(),
        }); // --active
            // The profile read (-t -f ipv4.*): a manual (static) profile.
        runner.push(CmdOut {
            rc: 0,
            stdout: "ipv4.method:manual\nipv4.addresses:10.0.0.5/24\nipv4.gateway:10.0.0.1\nipv4.dns:8.8.8.8,1.1.1.1\n".to_string(),
            stderr: String::new(),
        });
        // status(): ip addr + ip route (status) + ip route (get_gateway follow-up).
        runner.push(CmdOut {
            rc: 0,
            stdout: "    inet 10.0.0.5/24 scope global eth0\n".to_string(),
            stderr: String::new(),
        });
        runner.push(CmdOut {
            rc: 0,
            stdout: "default via 10.0.0.1 dev eth0\n".to_string(),
            stderr: String::new(),
        });
        runner.push(CmdOut {
            rc: 0,
            stdout: "default via 10.0.0.1 dev eth0\n".to_string(),
            stderr: String::new(),
        });
        let m = EthernetManager::with_sysfs_root("eth0", dir.path().to_path_buf(), runner);
        let cfg = m.config().await;
        assert_eq!(cfg["mode"], "static");
        assert_eq!(cfg["connection_name"], "Wired");
        assert_eq!(cfg["ip"], "10.0.0.5/24");
        assert_eq!(cfg["gateway"], "10.0.0.1");
        assert_eq!(cfg["dns"], json!(["8.8.8.8", "1.1.1.1"]));
        assert_eq!(cfg["link"], true);
        assert_eq!(cfg["speed_mbps"], 1000);
        assert_eq!(cfg["current_ip"], "10.0.0.5");
        assert_eq!(cfg["current_gateway"], "10.0.0.1");
    }

    #[tokio::test]
    async fn config_with_no_profile_reports_dhcp_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let runner = Arc::new(ScriptedRunner::new());
        // discover: no ethernet profile.
        runner.push(CmdOut {
            rc: 0,
            stdout: "MyWifi:802-11-wireless:wlan0\n".to_string(),
            stderr: String::new(),
        });
        runner.push(CmdOut::failed(0, "")); // --active empty
                                            // status(): ip addr + ip route both fail (no link).
        runner.push(CmdOut::failed(1, ""));
        runner.push(CmdOut::failed(1, ""));
        let m = EthernetManager::with_sysfs_root("eth0", dir.path().to_path_buf(), runner);
        let cfg = m.config().await;
        assert_eq!(cfg["mode"], "dhcp");
        assert!(cfg["connection_name"].is_null());
        assert!(cfg["ip"].is_null());
        assert_eq!(cfg["dns"], json!([]));
        assert_eq!(cfg["link"], false);
    }
}
