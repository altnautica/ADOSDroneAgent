//! Host identity the panel reads straight off the box: the board name from the
//! HAL board sidecar, the NIC MAC addresses from sysfs, and the primary address.
//!
//! NICs are resolved by what they are, not by name: a board may call its wired
//! port `eth0`, `end0` or `enp1s0`, and its Wi-Fi `wlan0` or `wlx…`. A wired NIC
//! is an Ethernet-type interface backed by a device with no wireless extension;
//! a wireless one carries a `wireless` or `phy80211` entry. Monitor-mode radios
//! (link type 803) and virtual interfaces (no `device` link) are skipped.

use std::path::{Path, PathBuf};

/// The HAL board sidecar the board detector persists.
pub const BOARD_SIDECAR_PATH: &str = "/run/ados/board.json";

/// The sysfs directory holding one entry per network interface.
pub const SYS_CLASS_NET: &str = "/sys/class/net";

/// The kernel's IPv4 routing table.
pub const PROC_NET_ROUTE: &str = "/proc/net/route";

/// `ARPHRD_ETHER`: the link type of an Ethernet or managed-mode Wi-Fi interface.
const ARPHRD_ETHER: u32 = 1;

/// `RTF_UP` in the routing table's flags column.
const RTF_UP: u32 = 0x1;

/// Largest board sidecar read. The file is a small dict; the cap keeps a
/// corrupt one from ballooning memory on the render loop.
const MAX_SIDECAR_BYTES: u64 = 64 * 1024;

/// Where the identity reads come from. The defaults are the live system paths;
/// tests point them at a fixture tree.
#[derive(Debug, Clone)]
pub struct HostPaths {
    pub board_sidecar: PathBuf,
    pub sys_class_net: PathBuf,
    pub proc_net_route: PathBuf,
}

impl Default for HostPaths {
    fn default() -> Self {
        Self {
            board_sidecar: PathBuf::from(BOARD_SIDECAR_PATH),
            sys_class_net: PathBuf::from(SYS_CLASS_NET),
            proc_net_route: PathBuf::from(PROC_NET_ROUTE),
        }
    }
}

/// The identity rows the about and diagnostics pages show. A field the box does
/// not report is `None`, never a stand-in.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HostIdentity {
    pub board_name: Option<String>,
    pub mac_wired: Option<String>,
    pub mac_wireless: Option<String>,
    pub primary_ip: Option<String>,
    pub primary_mac: Option<String>,
}

impl HostIdentity {
    /// Read every field from `paths`, plus the live primary address.
    pub fn read(paths: &HostPaths) -> Self {
        let (mac_wired, mac_wireless) = nic_macs(&paths.sys_class_net);
        let primary_mac = default_route_iface(&paths.proc_net_route)
            .and_then(|iface| mac_of(&paths.sys_class_net, &iface));
        Self {
            board_name: board_name(&paths.board_sidecar),
            mac_wired,
            mac_wireless,
            primary_ip: primary_ip(),
            primary_mac,
        }
    }
}

/// The friendly board name (`name`) from the HAL board sidecar.
pub fn board_name(path: &Path) -> Option<String> {
    use std::io::Read;
    let mut text = String::new();
    std::fs::File::open(path)
        .ok()?
        .take(MAX_SIDECAR_BYTES)
        .read_to_string(&mut text)
        .ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    let name = v.get("name")?.as_str()?.trim();
    (!name.is_empty()).then(|| name.to_string())
}

/// The MACs of the first wired and the first wireless NIC, by interface name
/// order.
pub fn nic_macs(sys_class_net: &Path) -> (Option<String>, Option<String>) {
    let Ok(entries) = std::fs::read_dir(sys_class_net) else {
        return (None, None);
    };
    let mut names: Vec<String> = entries
        .flatten()
        .filter_map(|e| e.file_name().into_string().ok())
        .collect();
    names.sort();

    let mut wired = None;
    let mut wireless = None;
    for name in names {
        let dir = sys_class_net.join(&name);
        let is_ether = read_trimmed(&dir.join("type")).and_then(|t| t.parse::<u32>().ok())
            == Some(ARPHRD_ETHER);
        if !is_ether || !dir.join("device").exists() {
            continue;
        }
        let is_wireless = dir.join("wireless").exists() || dir.join("phy80211").exists();
        let slot = if is_wireless {
            &mut wireless
        } else {
            &mut wired
        };
        if slot.is_none() {
            *slot = mac_of(sys_class_net, &name);
        }
    }
    (wired, wireless)
}

/// The MAC of `iface`, when it has a real one.
pub fn mac_of(sys_class_net: &Path, iface: &str) -> Option<String> {
    read_trimmed(&sys_class_net.join(iface).join("address"))
        .filter(|mac| !mac.is_empty() && mac != "00:00:00:00:00:00")
}

/// The interface carrying the IPv4 default route with the lowest metric.
pub fn default_route_iface(proc_net_route: &Path) -> Option<String> {
    let text = std::fs::read_to_string(proc_net_route).ok()?;
    text.lines()
        .skip(1)
        .filter_map(|line| {
            let cols: Vec<&str> = line.split_whitespace().collect();
            if cols.len() < 8 || cols[1] != "00000000" || cols[7] != "00000000" {
                return None;
            }
            let flags = u32::from_str_radix(cols[3], 16).ok()?;
            let metric: u32 = cols[6].parse().ok()?;
            (flags & RTF_UP != 0).then(|| (metric, cols[0].to_string()))
        })
        .min()
        .map(|(_, iface)| iface)
}

/// The source address the kernel picks for off-box traffic.
///
/// Connecting a UDP socket sends nothing; it only resolves the route, so this
/// reads the default route's source address without touching the network. A
/// box with no route out reports `None`.
pub fn primary_ip() -> Option<String> {
    let socket = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    // TEST-NET-1: never a real destination, only a route lookup.
    socket.connect("192.0.2.1:9").ok()?;
    let ip = socket.local_addr().ok()?.ip();
    (!ip.is_unspecified()).then(|| ip.to_string())
}

fn read_trimmed(path: &Path) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a fake sysfs interface entry.
    fn nic(root: &Path, name: &str, link_type: u32, mac: &str, device: bool, wireless: bool) {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("type"), format!("{link_type}\n")).unwrap();
        std::fs::write(dir.join("address"), format!("{mac}\n")).unwrap();
        if device {
            std::fs::create_dir_all(dir.join("device")).unwrap();
        }
        if wireless {
            std::fs::create_dir_all(dir.join("wireless")).unwrap();
        }
    }

    /// NICs are found by kind, whatever the board names them, and loopback,
    /// virtual and monitor-mode interfaces are never reported.
    #[test]
    fn nics_are_resolved_by_kind_not_by_name() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        nic(root, "lo", 772, "00:00:00:00:00:00", false, false);
        nic(root, "docker0", 1, "02:42:ac:11:00:01", false, false);
        nic(root, "end0", 1, "dc:a6:32:00:11:22", true, false);
        nic(root, "wlx00c0cafe", 803, "00:c0:ca:fe:00:01", true, true);
        nic(root, "wlan0", 1, "dc:a6:32:00:11:23", true, true);

        assert_eq!(
            nic_macs(root),
            (
                Some("dc:a6:32:00:11:22".to_string()),
                Some("dc:a6:32:00:11:23".to_string())
            )
        );
        assert_eq!(nic_macs(&root.join("absent")), (None, None));
    }

    #[test]
    fn the_default_route_with_the_lowest_metric_wins() {
        let dir = tempfile::tempdir().unwrap();
        let route = dir.path().join("route");
        std::fs::write(
            &route,
            "Iface\tDestination\tGateway \tFlags\tRefCnt\tUse\tMetric\tMask\t\tMTU\tWindow\tIRTT\n\
             wlan0\t00000000\t0101A8C0\t0003\t0\t0\t600\t00000000\t0\t0\t0\n\
             end0\t00000000\t0101A8C0\t0003\t0\t0\t100\t00000000\t0\t0\t0\n\
             end0\t0001A8C0\t00000000\t0001\t0\t0\t100\t00FFFFFF\t0\t0\t0\n\
             usb0\t00000000\t0102A8C0\t0002\t0\t0\t10\t00000000\t0\t0\t0\n",
        )
        .unwrap();
        // usb0 has the lowest metric but is down (no RTF_UP); end0 beats wlan0.
        assert_eq!(default_route_iface(&route).as_deref(), Some("end0"));
        assert_eq!(default_route_iface(&dir.path().join("absent")), None);
    }

    #[test]
    fn the_board_name_is_the_sidecar_name_field() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("board.json");
        std::fs::write(&path, r#"{"arch":"aarch64","name":"Raspberry Pi 4B"}"#).unwrap();
        assert_eq!(board_name(&path).as_deref(), Some("Raspberry Pi 4B"));
        std::fs::write(&path, r#"{"name":"  "}"#).unwrap();
        assert_eq!(board_name(&path), None);
        std::fs::write(&path, "not json").unwrap();
        assert_eq!(board_name(&path), None);
        assert_eq!(board_name(&dir.path().join("absent.json")), None);
    }

    #[test]
    fn the_primary_mac_follows_the_default_route_interface() {
        let dir = tempfile::tempdir().unwrap();
        let net = dir.path().join("net");
        nic(&net, "end0", 1, "dc:a6:32:00:11:22", true, false);
        nic(&net, "wlan0", 1, "dc:a6:32:00:11:23", true, true);
        let route = dir.path().join("route");
        std::fs::write(
            &route,
            "Iface\tDestination\tGateway\tFlags\tRefCnt\tUse\tMetric\tMask\n\
             wlan0\t00000000\t0101A8C0\t0003\t0\t0\t600\t00000000\n",
        )
        .unwrap();
        let id = HostIdentity::read(&HostPaths {
            board_sidecar: dir.path().join("board.json"),
            sys_class_net: net,
            proc_net_route: route,
        });
        assert_eq!(id.primary_mac.as_deref(), Some("dc:a6:32:00:11:23"));
        assert_eq!(id.board_name, None);
    }
}
