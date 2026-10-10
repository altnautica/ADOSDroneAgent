//! Tests for the ethernet uplink manager.

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
    let m =
        EthernetManager::with_sysfs_root(Some("eth0"), dir.path().to_path_buf(), runner.clone());
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
    let m = EthernetManager::with_interface("eth0", runner.clone());
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
    let m = EthernetManager::with_interface("eth0", runner.clone());
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
    let m = EthernetManager::with_sysfs_root(Some("eth0"), dir.path().to_path_buf(), runner);
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
    let m = EthernetManager::with_sysfs_root(Some("eth0"), dir.path().to_path_buf(), runner);
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
    let m = EthernetManager::with_interface("eth0", runner);
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
    let m = EthernetManager::with_interface("eth0", runner);
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
    let m = EthernetManager::with_interface("eth0", runner.clone());
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
    let m = EthernetManager::with_interface("eth0", runner);
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
    let m = EthernetManager::with_sysfs_root(Some("eth0"), dir.path().to_path_buf(), runner);
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
    let m = EthernetManager::with_sysfs_root(Some("eth0"), dir.path().to_path_buf(), runner);
    let cfg = m.config().await;
    assert_eq!(cfg["mode"], "dhcp");
    assert!(cfg["connection_name"].is_null());
    assert!(cfg["ip"].is_null());
    assert_eq!(cfg["dns"], json!([]));
    assert_eq!(cfg["link"], false);
}

/// One netdev under `<root>/class/net`: Ethernet link type, a backing device
/// bound to `driver`, and a carrier value.
#[cfg(unix)]
fn rig_port(root: &std::path::Path, name: &str, device: &str, driver: &str, carrier: &str) {
    let dev = root.join("devices").join(device);
    let drv = root.join("drivers").join(driver);
    std::fs::create_dir_all(&dev).unwrap();
    std::fs::create_dir_all(&drv).unwrap();
    std::os::unix::fs::symlink(&drv, dev.join("driver")).unwrap();
    let iface = root.join("class/net").join(name);
    std::fs::create_dir_all(&iface).unwrap();
    std::fs::write(iface.join("type"), "1\n").unwrap();
    std::fs::write(iface.join("carrier"), format!("{carrier}\n")).unwrap();
    std::os::unix::fs::symlink(&dev, iface.join("device")).unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn wired_skips_rndis_and_picks_stmmac() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    // A phone tether sorts first by name and must not be taken for the port.
    rig_port(root, "eth0", "usb1/1-1/1-1:1.0", "rndis_host", "1");
    rig_port(root, "eth1", "platform/fe300000.ethernet", "stmmaceth", "1");
    let m =
        EthernetManager::with_sysfs_root(None, root.to_path_buf(), Arc::new(ScriptedRunner::new()));
    assert_eq!(m.get_iface(), "eth1");
    assert!(m.is_up().await);
}

#[cfg(unix)]
#[tokio::test]
async fn hotplug_after_construction_is_used() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join("class/net")).unwrap();
    let runner = Arc::new(ScriptedRunner::new());
    let m = EthernetManager::with_sysfs_root(None, root.to_path_buf(), runner.clone());
    // No wired port yet: unresolved, down, and no command is aimed at a guess.
    assert_eq!(m.get_iface(), "");
    assert!(!m.is_up().await);
    assert_eq!(m.get_gateway().await, None);
    assert!(runner.recorded().is_empty());

    rig_port(root, "enx001122334455", "usb1/1-2/1-2:1.0", "r8152", "1");
    assert_eq!(m.get_iface(), "enx001122334455");
    assert!(m.is_up().await);
}
