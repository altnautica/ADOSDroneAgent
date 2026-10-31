//! Which wireless interface carries the mesh.
//!
//! Priority: the operator's override, else the WFB adapter the native
//! adapter-role resolver left to the mesh (published as `mesh_adapter` on this
//! role's fresh sidecar), else the second non-WFB radio by kernel driver (the
//! single control radio when the box has only one). Roles are decided by the
//! bound driver, never by the racing kernel interface name.

use std::path::Path;
use std::time::{Duration, SystemTime};

use ados_protocol::wfb_tables::{DENY_DRIVER_PREFIXES, WFB_COMPATIBLE_DRIVERS};
use serde_json::Value;

/// The freshness window of the relay / receiver sidecars; an older file is a
/// stopped writer, not a current assignment.
pub const ROLE_SIDECAR_MAX_AGE: Duration = Duration::from_secs(10);

/// The sidecar the native loop for `role` writes.
pub fn role_sidecar(role: &str) -> Option<&'static str> {
    match role {
        "relay" => Some("wfb-relay.json"),
        "receiver" => Some("wfb-receiver.json"),
        _ => None,
    }
}

/// The adapter the native resolver assigned to the mesh, from this role's
/// sidecar in `run_dir`, judged fresh against `now`.
pub fn published_mesh_adapter(role: &str, run_dir: &Path, now: SystemTime) -> Option<String> {
    let path = run_dir.join(role_sidecar(role)?);
    let modified = std::fs::metadata(&path).ok()?.modified().ok()?;
    let age = now.duration_since(modified).ok()?;
    if age > ROLE_SIDECAR_MAX_AGE {
        return None;
    }
    let body: Value = serde_json::from_str(&std::fs::read_to_string(&path).ok()?).ok()?;
    body.get("mesh_adapter")
        .and_then(Value::as_str)
        .filter(|a| !a.is_empty())
        .map(str::to_string)
}

/// The kernel driver bound to `iface`, empty when unreadable.
pub fn driver_of(sys_net: &Path, iface: &str) -> String {
    std::fs::canonicalize(sys_net.join(iface).join("device/driver"))
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        .unwrap_or_default()
}

/// Every wireless netdev under `sys_net`, sorted.
pub fn wireless_interfaces(sys_net: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(sys_net) else {
        return Vec::new();
    };
    let mut out: Vec<String> = entries
        .flatten()
        .filter(|e| e.path().join("phy80211").exists() || e.path().join("wireless").exists())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    out.sort();
    out
}

fn is_wfb_compatible(driver: &str) -> bool {
    let d = driver.trim().to_lowercase();
    WFB_COMPATIBLE_DRIVERS.iter().any(|c| c.to_lowercase() == d)
}

fn is_denied_management(driver: &str) -> bool {
    let d = driver.trim().to_lowercase();
    DENY_DRIVER_PREFIXES.iter().any(|p| d.starts_with(p))
}

/// The mesh radio by driver: the second non-WFB wireless (denied management
/// chips first), else the only one.
pub fn driver_mesh_iface(sys_net: &Path) -> Option<String> {
    let mut non_wfb: Vec<(bool, String)> = wireless_interfaces(sys_net)
        .into_iter()
        .filter_map(|i| {
            let driver = driver_of(sys_net, &i);
            (!is_wfb_compatible(&driver)).then(|| (!is_denied_management(&driver), i))
        })
        .collect();
    non_wfb.sort();
    let mut names = non_wfb.into_iter().map(|(_, i)| i);
    let mgmt = names.next();
    names.next().or(mgmt)
}

/// The mesh interface for `role`.
pub fn pick_mesh_iface(
    configured: Option<&str>,
    role: &str,
    run_dir: &Path,
    sys_net: &Path,
) -> Option<String> {
    if let Some(c) = configured.filter(|c| !c.is_empty()) {
        return Some(c.to_string());
    }
    published_mesh_adapter(role, run_dir, SystemTime::now()).or_else(|| driver_mesh_iface(sys_net))
}
