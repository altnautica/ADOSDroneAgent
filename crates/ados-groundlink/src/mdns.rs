//! Receiver discovery over mDNS on the batman-adv fabric.
//!
//! The receiver advertises `_ados-receiver._tcp` on `bat0`; relays browse for
//! it and forward `wfb_rx -f` fragments to the resolved `(ip, port)`. Both ends
//! live here in Rust (the `mdns-sd` daemon), so there is no Python handoff: the
//! relay loop calls [`resolve_receiver`] each poll and the receiver loop holds a
//! [`ReceiverAdvert`] guard that unregisters the record on drop.
//!
//! Scoping: `mdns-sd` answers on every interface, so a relay could otherwise
//! see a receiver advertised on the shared LAN. [`resolve_receiver`] filters
//! resolved addresses to the `bat0` /24 (mirroring the Python `_same_subnet`)
//! so discovery stays on the mesh fabric, never the operator's LAN.

use std::net::Ipv4Addr;
use std::time::Duration;

#[cfg(target_os = "linux")]
use std::net::IpAddr;

#[cfg(target_os = "linux")]
use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};

/// An active `_ados-receiver._tcp` advertisement. Dropping it unregisters the
/// record and shuts the mDNS daemon down (mirrors the Python
/// `zc.unregister_service(info); zc.close()` teardown).
pub struct ReceiverAdvert {
    #[cfg(target_os = "linux")]
    daemon: ServiceDaemon,
    #[cfg(target_os = "linux")]
    fullname: String,
}

impl ReceiverAdvert {
    /// Explicitly unregister + shut down. Also runs on `Drop`, but the loop
    /// calls this on a clean shutdown so the record is gone before the process
    /// exits rather than at an unspecified Drop time.
    pub fn shutdown(&self) {
        #[cfg(target_os = "linux")]
        {
            let _ = self.daemon.unregister(&self.fullname);
            let _ = self.daemon.shutdown();
        }
    }
}

impl Drop for ReceiverAdvert {
    fn drop(&mut self) {
        #[cfg(target_os = "linux")]
        {
            let _ = self.daemon.unregister(&self.fullname);
            let _ = self.daemon.shutdown();
        }
    }
}

/// Advertise `_ados-receiver._tcp` on the mesh fabric. The record carries the
/// `bat0` IPv4 address and the aggregator `listen_port`. Returns `None` when
/// the mesh interface has no IP yet (the loop retries) or mDNS is unavailable.
#[cfg(target_os = "linux")]
pub fn advertise_receiver(
    service_type: &str,
    mesh_iface: &str,
    port: u16,
) -> Option<ReceiverAdvert> {
    let mesh_ip = iface_ipv4(mesh_iface)?;
    let daemon = match ServiceDaemon::new() {
        Ok(d) => d,
        Err(e) => {
            tracing::warn!(error = %e, "mdns_daemon_new_failed");
            return None;
        }
    };
    let hostname = system_hostname();
    let ty = normalise_service_type(service_type);
    let server = format!("{hostname}.local.");
    // `ServiceInfo::new` accepts an `IpAddr` (not a bare `Ipv4Addr`) for the
    // address argument; wrap the resolved mesh IPv4 accordingly.
    let info = match ServiceInfo::new(
        &ty,
        &hostname,
        &server,
        IpAddr::V4(mesh_ip),
        port,
        &[] as &[(&str, &str)],
    ) {
        Ok(i) => i,
        Err(e) => {
            tracing::warn!(error = %e, "mdns_service_info_failed");
            let _ = daemon.shutdown();
            return None;
        }
    };
    let fullname = info.get_fullname().to_string();
    if let Err(e) = daemon.register(info) {
        tracing::warn!(error = %e, "mdns_register_failed");
        let _ = daemon.shutdown();
        return None;
    }
    tracing::info!(service = %ty, ip = %mesh_ip, port, instance = %fullname, "mdns_published");
    Some(ReceiverAdvert { daemon, fullname })
}

#[cfg(not(target_os = "linux"))]
pub fn advertise_receiver(
    _service_type: &str,
    _mesh_iface: &str,
    _port: u16,
) -> Option<ReceiverAdvert> {
    None
}

/// Browse `_ados-receiver._tcp` for up to `timeout`, returning the first
/// resolved `(ip, port)` whose address is on the `bat0` subnet. `None` on
/// timeout or when no on-mesh receiver answers.
#[cfg(target_os = "linux")]
pub async fn resolve_receiver(
    service_type: &str,
    mesh_iface: &str,
    timeout: Duration,
) -> Option<(String, u16)> {
    let bat_ip = iface_ipv4(mesh_iface);
    let daemon = ServiceDaemon::new().ok()?;
    let ty = normalise_service_type(service_type);
    let rx = match daemon.browse(&ty) {
        Ok(rx) => rx,
        Err(e) => {
            tracing::debug!(error = %e, "mdns_browse_failed");
            let _ = daemon.shutdown();
            return None;
        }
    };

    let result = tokio::time::timeout(timeout, async {
        while let Ok(event) = rx.recv_async().await {
            if let ServiceEvent::ServiceResolved(info) = event {
                let port = info.get_port();
                for addr in info.get_addresses() {
                    if let IpAddr::V4(v4) = addr {
                        if accept_address(*v4, bat_ip) {
                            return Some((v4.to_string(), port));
                        }
                    }
                }
            }
        }
        None
    })
    .await
    .ok()
    .flatten();

    let _ = daemon.shutdown();
    result
}

#[cfg(not(target_os = "linux"))]
pub async fn resolve_receiver(
    _service_type: &str,
    _mesh_iface: &str,
    _timeout: Duration,
) -> Option<(String, u16)> {
    None
}

/// Accept a resolved address iff it shares the `/24` of the mesh IP. When the
/// mesh IP is unknown (no `bat0` address yet) we accept any address rather than
/// drop everything — matching the Python `if bat_ip and not _same_subnet`
/// guard, which only filters when the local mesh IP is known.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn accept_address(addr: Ipv4Addr, bat_ip: Option<Ipv4Addr>) -> bool {
    match bat_ip {
        Some(local) => same_subnet_24(addr, local),
        None => true,
    }
}

/// True when `a` and `b` share the same `/24` network. Mirrors the Python
/// `_same_subnet` default `mask_prefix=24` used for the bat0 receiver scope.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn same_subnet_24(a: Ipv4Addr, b: Ipv4Addr) -> bool {
    let ao = a.octets();
    let bo = b.octets();
    ao[0] == bo[0] && ao[1] == bo[1] && ao[2] == bo[2]
}

/// Service-type normalisation to the `_x._tcp.local.` form `mdns-sd` expects.
/// The config carries `_ados-receiver._tcp`; we append `.local.` if absent.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn normalise_service_type(service_type: &str) -> String {
    let trimmed = service_type.trim_end_matches('.');
    if trimmed.ends_with(".local") {
        format!("{trimmed}.")
    } else {
        format!("{trimmed}.local.")
    }
}

/// Best-effort hostname for the advertised instance name.
#[cfg(target_os = "linux")]
fn system_hostname() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "ados".to_string())
}

/// Parse the first IPv4 address bound to `iface` out of `ip -4 addr show dev`.
/// Returns `None` when the interface has no address (the loop retries).
#[cfg(target_os = "linux")]
fn iface_ipv4(iface: &str) -> Option<Ipv4Addr> {
    let out = std::process::Command::new("ip")
        .args(["-4", "addr", "show", "dev", iface])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    parse_iface_ipv4(&text)
}

/// Pull the `inet <a.b.c.d>/prefix` address out of `ip -4 addr show` output.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn parse_iface_ipv4(text: &str) -> Option<Ipv4Addr> {
    for line in text.lines() {
        let toks: Vec<&str> = line.split_whitespace().collect();
        if let Some(idx) = toks.iter().position(|t| *t == "inet") {
            if let Some(cidr) = toks.get(idx + 1) {
                let ip_part = cidr.split('/').next().unwrap_or(cidr);
                if let Ok(v4) = ip_part.parse::<Ipv4Addr>() {
                    return Some(v4);
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalise_appends_local() {
        assert_eq!(
            normalise_service_type("_ados-receiver._tcp"),
            "_ados-receiver._tcp.local."
        );
        // Already-qualified forms collapse to one trailing dot.
        assert_eq!(
            normalise_service_type("_ados-receiver._tcp.local"),
            "_ados-receiver._tcp.local."
        );
        assert_eq!(
            normalise_service_type("_ados-receiver._tcp.local."),
            "_ados-receiver._tcp.local."
        );
    }

    #[test]
    fn same_subnet_matches_24() {
        let a: Ipv4Addr = "10.0.0.5".parse().unwrap();
        let b: Ipv4Addr = "10.0.0.200".parse().unwrap();
        let c: Ipv4Addr = "10.0.1.5".parse().unwrap();
        assert!(same_subnet_24(a, b));
        assert!(!same_subnet_24(a, c));
    }

    #[test]
    fn accept_address_only_filters_when_local_known() {
        let on_mesh: Ipv4Addr = "10.0.0.9".parse().unwrap();
        let off_mesh: Ipv4Addr = "192.168.1.9".parse().unwrap();
        let bat: Ipv4Addr = "10.0.0.1".parse().unwrap();
        // Known local IP: only same-/24 accepted.
        assert!(accept_address(on_mesh, Some(bat)));
        assert!(!accept_address(off_mesh, Some(bat)));
        // Unknown local IP: accept anything (no premature drop).
        assert!(accept_address(off_mesh, None));
    }

    #[test]
    fn parse_iface_ipv4_pulls_inet() {
        let text = "5: bat0: <BROADCAST,MULTICAST,UP,LOWER_UP> mtu 1532\n    inet 10.0.0.3/24 brd 10.0.0.255 scope global bat0\n       valid_lft forever preferred_lft forever\n";
        assert_eq!(
            parse_iface_ipv4(text),
            Some("10.0.0.3".parse::<Ipv4Addr>().unwrap())
        );
    }

    #[test]
    fn parse_iface_ipv4_none_when_no_inet() {
        let text = "7: bat0: <BROADCAST,MULTICAST> mtu 1532 state DOWN\n";
        assert_eq!(parse_iface_ipv4(text), None);
    }
}
