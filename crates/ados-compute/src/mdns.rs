//! mDNS advertisement for the compute node.
//!
//! The Python `ados-discovery` service (which also advertises `_ados._tcp` on
//! `:8080`) is installed on every profile but is OnDemand — it starts only when
//! a cloud pairing code is generated, not at boot. So at boot a compute node has
//! no advert and would not appear in the GCS Add-a-Node card. This always-on
//! Rust advert (the `mdns-sd` daemon, held by the compute daemon for its
//! lifetime) fills that gap: the node advertises `_ados._tcp` with
//! `profile=workstation` in the TXT from boot, so it auto-appears for LAN pairing
//! (Rule 39) like a drone/ground-station, no pairing code required first.
//!
//! The advert points at the control front's pairing port (`:8080`, where the
//! node serves `/api/pairing/*`); the job-API port (`:8092`) rides the `jobApi`
//! TXT key for a consumer that wants it directly. If `ados-discovery` is later
//! started for a pairing code, both publish an `_ados._tcp` record for the same
//! host — a brief benign duplicate (both point at the same `:8080` pairing
//! front). Discovery is best-effort: if mDNS is unavailable the daemon logs and
//! degrades, and manual Add-a-Node by IP always works.

use mdns_sd::{ServiceDaemon, ServiceInfo};

/// The pairing service type the GCS browses for Add-a-Node discovery.
const PAIRING_SERVICE: &str = "_ados._tcp.local.";
/// The control front's pairing port — the node serves `/api/pairing/*` here.
const PAIRING_PORT: u16 = 8080;

/// An active `_ados._tcp` advertisement for this compute node. Dropping it
/// unregisters the record and shuts the mDNS daemon down (mirrors the Python
/// `zc.unregister_service(info); zc.close()` teardown).
pub struct ComputeAdvert {
    daemon: ServiceDaemon,
    fullname: String,
}

impl ComputeAdvert {
    /// Explicitly unregister + shut down (also runs on `Drop`).
    pub fn shutdown(&self) {
        let _ = self.daemon.unregister(&self.fullname);
        let _ = self.daemon.shutdown();
    }
}

impl Drop for ComputeAdvert {
    fn drop(&mut self) {
        let _ = self.daemon.unregister(&self.fullname);
        let _ = self.daemon.shutdown();
    }
}

/// The instance name + TXT records for this node's advert. Pure, so the wire
/// shape is unit-tested without standing up an mDNS daemon. The instance name
/// carries the node id so two compute nodes never collide on the same hostname.
fn advert_fields(node_id: &str, job_api_port: u16) -> (String, Vec<(String, String)>) {
    let short: String = node_id.chars().take(12).collect();
    let instance = format!("ados-compute-{short}");
    let txt = vec![
        ("profile".to_string(), "workstation".to_string()),
        ("path".to_string(), "/api/pairing".to_string()),
        ("jobApi".to_string(), job_api_port.to_string()),
        ("deviceId".to_string(), node_id.to_string()),
    ];
    (instance, txt)
}

/// Best-effort cross-platform hostname (the SRV target). Linux exposes it as a
/// file; elsewhere fall back to the `hostname` command, then a stable default.
fn system_hostname() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .ok()
        .or_else(|| {
            std::process::Command::new("hostname")
                .output()
                .ok()
                .and_then(|o| String::from_utf8(o.stdout).ok())
        })
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "ados".to_string())
}

/// Advertise this compute node on `_ados._tcp` so the GCS Add-a-Node card
/// discovers it for LAN pairing. Returns `None` when mDNS is unavailable; the
/// caller treats that as "no auto-discovery", not a fatal error.
pub fn advertise_compute(node_id: &str, job_api_port: u16) -> Option<ComputeAdvert> {
    let daemon = match ServiceDaemon::new() {
        Ok(d) => d,
        Err(e) => {
            tracing::warn!(error = %e, "compute_mdns_daemon_failed");
            return None;
        }
    };
    let hostname = system_hostname();
    let server = format!("{hostname}.local.");
    let (instance, txt) = advert_fields(node_id, job_api_port);
    let txt_refs: Vec<(&str, &str)> = txt.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();

    // Empty address + `enable_addr_auto` => advertise on every interface's IP,
    // matching the Python discovery's all-interface answer.
    let info = match ServiceInfo::new(
        PAIRING_SERVICE,
        &instance,
        &server,
        "",
        PAIRING_PORT,
        &txt_refs[..],
    ) {
        Ok(i) => i.enable_addr_auto(),
        Err(e) => {
            tracing::warn!(error = %e, "compute_mdns_service_info_failed");
            let _ = daemon.shutdown();
            return None;
        }
    };
    let fullname = info.get_fullname().to_string();
    if let Err(e) = daemon.register(info) {
        tracing::warn!(error = %e, "compute_mdns_register_failed");
        let _ = daemon.shutdown();
        return None;
    }
    tracing::info!(
        service = PAIRING_SERVICE,
        port = PAIRING_PORT,
        instance = %fullname,
        "compute_mdns_published"
    );
    Some(ComputeAdvert { daemon, fullname })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn advert_fields_carry_the_workstation_profile_and_ports() {
        let (instance, txt) = advert_fields("node-abcdef0123456789", 8092);
        // The instance carries a node-id prefix (first 12 chars) for uniqueness.
        assert_eq!(instance, "ados-compute-node-abcdef0");
        let get = |k: &str| {
            txt.iter()
                .find(|(key, _)| key == k)
                .map(|(_, v)| v.as_str())
        };
        assert_eq!(get("profile"), Some("workstation"));
        assert_eq!(get("path"), Some("/api/pairing"));
        assert_eq!(get("jobApi"), Some("8092"));
        assert_eq!(get("deviceId"), Some("node-abcdef0123456789"));
    }

    #[test]
    fn hostname_is_never_empty() {
        assert!(!system_hostname().is_empty());
    }
}
