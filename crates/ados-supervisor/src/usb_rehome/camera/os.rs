//! OS edges for camera USB-recovery: the camera-state + last-good sidecar IO,
//! the protected-set builder, the sysfs unbind/rebind + port-cycle executor, and
//! the small pure bind-id ancestor helpers.
//!
//! The reconciler reaches the OS only through [`CameraOs`], so its decisions
//! run against a fake sysfs and clock in tests; [`LinuxOs`] is the real one.
//! The supervisor executes the authorized plan through
//! `execute_camera_recovery`. The video pipeline re-discovers the camera via
//! the udev→SIGUSR1 path once the device re-enumerates, so this never restarts
//! `ados-video`.

#[cfg(target_os = "linux")]
use super::CameraRecoveryAction;
use super::CameraRecoveryPlan;

#[cfg(target_os = "linux")]
use super::super::topo::{self, ControlPath, UsbTopo};
#[cfg(target_os = "linux")]
use super::config::CameraRecoveryConfig;

#[cfg(target_os = "linux")]
use std::time::Duration;

#[cfg(target_os = "linux")]
pub(super) const SIDECAR_PATH: &str = "/run/ados/camera-usb-recovery.json";
/// Schema version of the `camera-usb-recovery.json` sidecar. Bump on an
/// incompatible field-set change; a reader compares it best-effort via
/// `ados_protocol::sidecar::check_sidecar_version`. Kept in step with the
/// registry in `contracts.toml`. Gated to the platforms that build the writer
/// (Linux) or the version test.
#[cfg(any(target_os = "linux", test))]
pub(super) const CAMERA_USB_RECOVERY_SIDECAR_VERSION: u16 = 1;
#[cfg(target_os = "linux")]
const CAMERA_STATE_PATH: &str = "/run/ados/camera-state.json";
/// The primary camera's location record. A secondary camera's sits beside it
/// as `camera-last-good-<id>.json`.
#[cfg(target_os = "linux")]
const LAST_GOOD_DIR: &str = "/var/ados";
#[cfg(target_os = "linux")]
const WFB_STATS_PATH: &str = "/run/ados/wfb-stats.json";
#[cfg(target_os = "linux")]
const USB_UNBIND_PATH: &str = "/sys/bus/usb/drivers/usb/unbind";
#[cfg(target_os = "linux")]
const USB_BIND_PATH: &str = "/sys/bus/usb/drivers/usb/bind";
#[cfg(target_os = "linux")]
pub(super) const USB_DEVICES_DIR: &str = "/sys/bus/usb/devices";

/// Settle between the unbind and the bind / the disable toggle.
#[cfg(target_os = "linux")]
const SETTLE: Duration = Duration::from_millis(1500);
/// Hold the port disabled before re-enabling.
#[cfg(target_os = "linux")]
const PORT_DISABLE_HOLD: Duration = Duration::from_millis(600);

/// Persisted last-known-good camera record.
#[cfg(target_os = "linux")]
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, PartialEq, Eq)]
pub(crate) struct LastGood {
    pub(crate) bind_id: String,
    pub(crate) hub: String,
    pub(crate) port: u32,
    #[serde(default)]
    pub(crate) vid: String,
    #[serde(default)]
    pub(crate) pid: String,
    #[serde(default)]
    pub(crate) updated_at_unix: u64,
}

/// The file name of a camera's location record: the primary keeps the node's
/// original `camera-last-good.json`; every other camera gets its own, keyed by
/// its roster id (restricted to `[A-Za-z0-9_-]`).
#[cfg(any(target_os = "linux", test))]
pub(crate) fn last_good_name(primary: bool, id: &str) -> String {
    if primary {
        return "camera-last-good.json".to_string();
    }
    let safe: String = id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    format!("camera-last-good-{safe}.json")
}

/// Everything the reconciler reads from or writes to the OS.
#[cfg(target_os = "linux")]
pub(crate) trait CameraOs {
    fn config(&self) -> CameraRecoveryConfig;
    /// The camera-state sidecar document, `None` when absent or not JSON.
    async fn camera_state(&self) -> Option<serde_json::Value>;
    fn now_unix(&self) -> u64;
    async fn read_last_good(&self, primary: bool, id: &str) -> Option<LastGood>;
    async fn write_last_good(&self, primary: bool, id: &str, lg: &LastGood);
    /// The USB topology of a V4L2 node, `None` when it is gone or not USB.
    async fn video_topo(&self, dev_path: &str) -> Option<UsbTopo>;
    fn device_present(&self, bind_id: &str) -> bool;
    /// `(idVendor, idProduct)` of a USB device, empty when unreadable.
    async fn usb_ids(&self, bind_id: &str) -> (String, String);
    async fn protected_set(&self) -> (Vec<ControlPath>, Vec<UsbTopo>);
    fn ppps_capable(&self, hub: &str, port: u32) -> bool;
    fn within_boot_window(&self, window: Duration) -> bool;
    async fn fc_armed(&self) -> Option<bool>;
    async fn radio_topo(&self) -> Option<UsbTopo>;
    fn write_sidecar(&self, body: &serde_json::Value);
}

/// The real OS.
#[cfg(target_os = "linux")]
pub(crate) struct LinuxOs;

#[cfg(target_os = "linux")]
impl CameraOs for LinuxOs {
    fn config(&self) -> CameraRecoveryConfig {
        super::config::read_config()
    }

    async fn camera_state(&self) -> Option<serde_json::Value> {
        let txt = tokio::fs::read_to_string(CAMERA_STATE_PATH).await.ok()?;
        let v: serde_json::Value = serde_json::from_str(&txt).ok()?;
        // Best-effort schema-drift signal: warn (never reject) when the
        // camera-state sidecar was written by an agent with a different schema
        // version. The writer const lives in the `ados-video` crate, so compare
        // against the shared registry (the writer pins its const to the same
        // value).
        let got = v.get("version").and_then(|x| x.as_u64()).unwrap_or(0) as u16;
        if let Some(ours) = ados_protocol::contracts::sidecar_version("camera-state") {
            ados_protocol::sidecar::check_sidecar_version("camera-state", got, ours);
        }
        Some(v)
    }

    fn now_unix(&self) -> u64 {
        now_unix()
    }

    async fn read_last_good(&self, primary: bool, id: &str) -> Option<LastGood> {
        let path = std::path::Path::new(LAST_GOOD_DIR).join(last_good_name(primary, id));
        let txt = tokio::fs::read_to_string(path).await.ok()?;
        serde_json::from_str::<LastGood>(&txt).ok()
    }

    async fn write_last_good(&self, primary: bool, id: &str, lg: &LastGood) {
        let path = std::path::Path::new(LAST_GOOD_DIR).join(last_good_name(primary, id));
        if let Err(e) = ados_protocol::sidecar::write_json_atomic(&path, lg, 0o644) {
            tracing::debug!(error = %e, "camera last-good write failed");
        }
    }

    async fn video_topo(&self, dev_path: &str) -> Option<UsbTopo> {
        topo::resolve_usb_topo_for_video(dev_path).await
    }

    fn device_present(&self, bind_id: &str) -> bool {
        std::path::Path::new(USB_DEVICES_DIR)
            .join(bind_id)
            .join("idVendor")
            .is_file()
    }

    async fn usb_ids(&self, bind_id: &str) -> (String, String) {
        (
            read_sysfs_id(bind_id, "idVendor").await,
            read_sysfs_id(bind_id, "idProduct").await,
        )
    }

    async fn protected_set(&self) -> (Vec<ControlPath>, Vec<UsbTopo>) {
        build_protected_set().await
    }

    fn ppps_capable(&self, hub: &str, port: u32) -> bool {
        topo::ppps_capable(hub, port)
    }

    fn within_boot_window(&self, window: Duration) -> bool {
        within_boot_window(window)
    }

    async fn fc_armed(&self) -> Option<bool> {
        crate::fc_state::read_fc_armed().await
    }

    async fn radio_topo(&self) -> Option<UsbTopo> {
        let iface = read_wfb_iface().await?;
        topo::resolve_usb_topo(&iface).await
    }

    fn write_sidecar(&self, body: &serde_json::Value) {
        let path = std::path::Path::new(SIDECAR_PATH);
        if let Err(e) = ados_protocol::sidecar::write_json_atomic(path, body, 0o644) {
            tracing::debug!(error = %e, "camera recovery sidecar write failed");
        }
    }
}

/// Execute the sysfs op for an authorized plan. Best-effort; the video pipeline
/// re-discovers the camera via udev once it re-enumerates.
#[cfg(target_os = "linux")]
pub async fn execute_camera_recovery(plan: &CameraRecoveryPlan) {
    let camera = plan.camera_id.as_str();
    match &plan.action {
        CameraRecoveryAction::RebindDevice { bind_id } => {
            tracing::warn!(camera, bind_id = %bind_id, attempt = plan.attempt, "camera_recovery_rebind");
            rebind(bind_id).await;
        }
        CameraRecoveryAction::ResetHub { bind_id } => {
            tracing::warn!(camera, bind_id = %bind_id, attempt = plan.attempt, "camera_recovery_hub_reset");
            rebind(bind_id).await;
        }
        CameraRecoveryAction::CyclePort { hub, port } => {
            tracing::warn!(camera, hub = %hub, port = port, attempt = plan.attempt, "camera_recovery_port_cycle");
            let attr = format!("{}/{}", USB_DEVICES_DIR, topo::port_disable_rel(hub, *port));
            if let Err(e) = sysfs_write(&attr, "1").await {
                tracing::warn!(error = %e, "camera recovery port disable failed");
            }
            tokio::time::sleep(PORT_DISABLE_HOLD).await;
            if let Err(e) = sysfs_write(&attr, "0").await {
                tracing::warn!(error = %e, "camera recovery port enable failed");
            }
        }
    }
}

#[cfg(target_os = "linux")]
async fn rebind(bind_id: &str) {
    if let Err(e) = sysfs_write(USB_UNBIND_PATH, bind_id).await {
        tracing::warn!(error = %e, "camera recovery unbind failed");
    }
    tokio::time::sleep(SETTLE).await;
    if let Err(e) = sysfs_write(USB_BIND_PATH, bind_id).await {
        tracing::warn!(error = %e, "camera recovery bind failed");
    }
}

#[cfg(not(target_os = "linux"))]
pub async fn execute_camera_recovery(_plan: &CameraRecoveryPlan) {}

#[cfg(target_os = "linux")]
async fn sysfs_write(path: &str, val: &str) -> std::io::Result<()> {
    tokio::fs::write(path, val).await
}

/// The WFB radio interface, from the radio's stats sidecar (for the guard set).
#[cfg(target_os = "linux")]
async fn read_wfb_iface() -> Option<String> {
    let txt = tokio::fs::read_to_string(WFB_STATS_PATH).await.ok()?;
    let v: serde_json::Value = serde_json::from_str(&txt).ok()?;
    let iface = v.get("interface")?.as_str()?.to_string();
    if iface.is_empty() {
        None
    } else {
        Some(iface)
    }
}

/// Build the protected set: the management link AND the WFB radio AND the FC. A
/// hub reset must disturb none of them.
#[cfg(target_os = "linux")]
async fn build_protected_set() -> (Vec<ControlPath>, Vec<UsbTopo>) {
    let mut paths = Vec::new();
    let mut usb = Vec::new();

    let default_iface = crate::mgmt_link_guardian::detection::default_route_iface().await;
    let control = topo::resolve_control_path(default_iface.as_deref()).await;
    if let ControlPath::Usb(t) = &control {
        usb.push(t.clone());
    }
    paths.push(control);

    if let Some(iface) = read_wfb_iface().await {
        if let Some(t) = topo::resolve_usb_topo(&iface).await {
            usb.push(t.clone());
            paths.push(ControlPath::Usb(t));
        }
    }

    for tty in ["ttyACM0", "ttyACM1", "ttyUSB0", "ttyUSB1"] {
        if let Some(t) = topo::resolve_usb_topo_for_tty(tty).await {
            usb.push(t.clone());
            paths.push(ControlPath::Usb(t));
        }
    }

    (paths, usb)
}

/// Synthesize a device node's USB-node ancestors purely from its bind id, e.g.
/// `1-1.1 -> ["1-1", "usb1"]`. Used when the device is absent so it cannot be
/// walked in `/sys`.
#[cfg(any(target_os = "linux", test))]
pub(super) fn name_ancestors(bind_id: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = bind_id.to_string();
    for _ in 0..8 {
        match topo_hub_and_port(&cur) {
            Some((hub, _)) => {
                out.push(hub.clone());
                if hub.starts_with("usb") {
                    break;
                }
                cur = hub;
            }
            None => break,
        }
    }
    out
}

/// Local mirror of `topo::hub_and_port` so `name_ancestors` is pure + buildable
/// on every host (the topo fn is the same logic).
#[cfg(any(target_os = "linux", test))]
fn topo_hub_and_port(bind_id: &str) -> Option<(String, u32)> {
    if let Some(idx) = bind_id.rfind('.') {
        let hub = bind_id[..idx].to_string();
        let port = bind_id[idx + 1..].parse::<u32>().ok()?;
        if hub.is_empty() {
            return None;
        }
        return Some((hub, port));
    }
    let (bus, port) = bind_id.split_once('-')?;
    let busn = bus.parse::<u32>().ok()?;
    let portn = port.parse::<u32>().ok()?;
    Some((format!("usb{}", busn), portn))
}

#[cfg(target_os = "linux")]
async fn read_sysfs_id(bind_id: &str, attr: &str) -> String {
    let p = format!("{}/{}/{}", USB_DEVICES_DIR, bind_id, attr);
    tokio::fs::read_to_string(&p)
        .await
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

/// Whether the box is still inside the post-boot window (a proxy for "on the
/// ground, not yet flying"). Reads `/proc/uptime`.
#[cfg(target_os = "linux")]
fn within_boot_window(window: Duration) -> bool {
    match std::fs::read_to_string("/proc/uptime") {
        Ok(s) => s
            .split_whitespace()
            .next()
            .and_then(|f| f.parse::<f64>().ok())
            .map(|up| up <= window.as_secs() as f64)
            .unwrap_or(false),
        Err(_) => false,
    }
}

#[cfg(target_os = "linux")]
pub(super) fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests;
