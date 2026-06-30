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

use std::time::Duration;

use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};

/// The pairing service type the GCS browses for Add-a-Node discovery.
const PAIRING_SERVICE: &str = "_ados._tcp.local.";
/// The control front's pairing port — the node serves `/api/pairing/*` here.
const PAIRING_PORT: u16 = 8080;
/// The TXT `profile` value a compute node advertises (the post-rename profile).
/// A resolver filters on this so it never targets a drone / ground-station node.
const WORKSTATION_PROFILE: &str = "workstation";

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
/// Public so the daemon can derive an artifact URL host that matches the mDNS
/// `.local` target this advert uses.
pub fn system_hostname() -> String {
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

/// Browse `_ados._tcp` for up to `timeout` and resolve the first **compute
/// node** — a service whose TXT carries `profile=workstation` — returning its
/// `(host, job_api_port)` so a caller can build the LAN job-API base URL
/// (`http://host:job_api_port`).
///
/// The job-API port rides the `jobApi` TXT key, NOT the SRV port: the SRV port
/// is the `:8080` pairing front (where `/api/pairing/*` lives), while the job
/// API serves on its own port. An IPv4 address is preferred for the host (a
/// reqwest client dials it directly, with no second mDNS hostname lookup); the
/// advertised hostname is the fallback. Returns `None` on timeout, when mDNS is
/// unavailable, or when no workstation answers — the caller treats that as "no
/// compute node on the LAN yet" and retries.
///
/// Mirrors `ados_groundlink::mdns::resolve_receiver` (same `mdns-sd` browse +
/// `ServiceResolved` loop + bounded `tokio::time::timeout`); the difference is
/// the accept predicate — a TXT `profile` match here vs a mesh-subnet match
/// there — and that the returned port comes from a TXT key, not the SRV record.
pub async fn resolve_compute(timeout: Duration) -> Option<(String, u16)> {
    let daemon = ServiceDaemon::new().ok()?;
    let rx = match daemon.browse(PAIRING_SERVICE) {
        Ok(rx) => rx,
        Err(e) => {
            tracing::debug!(error = %e, "compute_mdns_browse_failed");
            let _ = daemon.shutdown();
            return None;
        }
    };

    let result = tokio::time::timeout(timeout, async {
        while let Ok(event) = rx.recv_async().await {
            if let ServiceEvent::ServiceResolved(info) = event {
                // Only a compute node — skip a drone / ground-station advert that
                // shares `_ados._tcp` on the same LAN.
                if info.get_property_val_str("profile") != Some(WORKSTATION_PROFILE) {
                    continue;
                }
                // The job API rides the `jobApi` TXT key (the SRV port is the
                // pairing front). A missing / zero / unparseable port is skipped.
                let Some(port) = info
                    .get_property_val_str("jobApi")
                    .and_then(|p| p.parse::<u16>().ok())
                    .filter(|p| *p != 0)
                else {
                    continue;
                };
                // Prefer a concrete IPv4 (dial it directly); else the hostname.
                if let Some(v4) = info.get_addresses_v4().into_iter().next() {
                    return Some((v4.to_string(), port));
                }
                let host = info.get_hostname().trim_end_matches('.').to_string();
                if !host.is_empty() {
                    return Some((host, port));
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

    #[tokio::test]
    async fn resolve_compute_returns_none_when_no_workstation_answers() {
        // No compute node advertises in the unit-test environment, so a short
        // browse window resolves nothing (and if mDNS is unavailable in the
        // sandbox the daemon fails to start, which also yields `None`). The
        // function must return — not hang — within the timeout.
        let got = tokio::time::timeout(
            Duration::from_secs(5),
            resolve_compute(Duration::from_millis(300)),
        )
        .await
        .expect("resolve_compute must honour its own timeout and not hang");
        match got {
            None => {}
            // Defensive against a stray real workstation on the dev LAN: a
            // resolved node must at least carry a usable (non-zero) job-API port.
            Some((host, port)) => {
                assert!(!host.is_empty(), "a resolved node carries a host");
                assert_ne!(port, 0, "a resolved node carries a non-zero job-API port");
            }
        }
    }
}
