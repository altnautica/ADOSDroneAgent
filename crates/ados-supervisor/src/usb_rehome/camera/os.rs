//! OS edges for camera USB-recovery: the camera-state + last-good sidecar IO,
//! the protected-set builder, the sysfs unbind/rebind + port-cycle executor, and
//! the small pure bind-id ancestor helpers.
//!
//! The sysfs read/write edges are Linux-only. The reconciler FSM in the module
//! root decides; the supervisor executes the authorized plan through
//! `execute_camera_recovery`. The video pipeline re-discovers the camera via the
//! udev→SIGUSR1 path once the device re-enumerates, so this never restarts
//! `ados-video`.

#[cfg(target_os = "linux")]
use super::CameraRecoveryAction;
use super::CameraRecoveryPlan;

#[cfg(target_os = "linux")]
use super::super::topo;

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
#[cfg(target_os = "linux")]
pub(super) const LAST_GOOD_PATH: &str = "/var/ados/camera-last-good.json";
#[cfg(target_os = "linux")]
const WFB_STATS_PATH: &str = "/run/ados/wfb-stats.json";
#[cfg(target_os = "linux")]
const USB_UNBIND_PATH: &str = "/sys/bus/usb/drivers/usb/unbind";
#[cfg(target_os = "linux")]
const USB_BIND_PATH: &str = "/sys/bus/usb/drivers/usb/bind";
#[cfg(target_os = "linux")]
pub(super) const USB_DEVICES_DIR: &str = "/sys/bus/usb/devices";

/// Treat a camera-state snapshot older than this as unknown (do not act).
#[cfg(target_os = "linux")]
const STATE_FRESHNESS: Duration = Duration::from_secs(120);
/// Settle between the unbind and the bind / the disable toggle.
#[cfg(target_os = "linux")]
const SETTLE: Duration = Duration::from_millis(1500);
/// Hold the port disabled before re-enabling.
#[cfg(target_os = "linux")]
const PORT_DISABLE_HOLD: Duration = Duration::from_millis(600);

/// The camera-state snapshot read from the video pipeline's sidecar.
#[cfg(target_os = "linux")]
pub(super) struct CameraSignals {
    pub(super) state: String,
    pub(super) primary_path: Option<String>,
    pub(super) fresh: bool,
}

/// Persisted last-known-good camera record.
#[cfg(target_os = "linux")]
#[derive(serde::Serialize, serde::Deserialize, Clone)]
pub(super) struct LastGood {
    pub(super) bind_id: String,
    pub(super) hub: String,
    pub(super) port: u32,
    #[serde(default)]
    pub(super) vid: String,
    #[serde(default)]
    pub(super) pid: String,
    #[serde(default)]
    pub(super) updated_at_unix: u64,
}

/// Execute the sysfs op for an authorized plan. Best-effort; the video pipeline
/// re-discovers the camera via udev once it re-enumerates.
#[cfg(target_os = "linux")]
pub async fn execute_camera_recovery(plan: &CameraRecoveryPlan) {
    match &plan.action {
        CameraRecoveryAction::RebindDevice { bind_id } => {
            tracing::warn!(bind_id = %bind_id, attempt = plan.attempt, "camera_recovery_rebind");
            rebind(bind_id).await;
        }
        CameraRecoveryAction::ResetHub { bind_id } => {
            tracing::warn!(bind_id = %bind_id, attempt = plan.attempt, "camera_recovery_hub_reset");
            rebind(bind_id).await;
        }
        CameraRecoveryAction::CyclePort { hub, port } => {
            tracing::warn!(hub = %hub, port = port, attempt = plan.attempt, "camera_recovery_port_cycle");
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

/// Read the camera-state sidecar. `None` when absent.
#[cfg(target_os = "linux")]
pub(super) async fn read_camera_signals() -> Option<CameraSignals> {
    let txt = tokio::fs::read_to_string(CAMERA_STATE_PATH).await.ok()?;
    let v: serde_json::Value = serde_json::from_str(&txt).ok()?;
    // Best-effort schema-drift signal: warn (never reject) when the camera-state
    // sidecar was written by an agent with a different schema version. The
    // writer const lives in the `ados-video` crate, so compare against the shared
    // registry (the writer pins its const to the same value).
    let got = v.get("version").and_then(|x| x.as_u64()).unwrap_or(0) as u16;
    if let Some(ours) = ados_protocol::contracts::sidecar_version("camera-state") {
        ados_protocol::sidecar::check_sidecar_version("camera-state", got, ours);
    }
    let state = v.get("state")?.as_str()?.to_string();
    let primary_path = v
        .get("primary_path")
        .and_then(|x| x.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());
    let updated = v
        .get("updated_at_unix")
        .and_then(|x| x.as_f64())
        .unwrap_or(0.0);
    let fresh = updated > 0.0 && {
        let age = now_unix().saturating_sub(updated as u64);
        Duration::from_secs(age) <= STATE_FRESHNESS
    };
    Some(CameraSignals {
        state,
        primary_path,
        fresh,
    })
}

#[cfg(target_os = "linux")]
pub(super) async fn read_last_good() -> Option<LastGood> {
    let txt = tokio::fs::read_to_string(LAST_GOOD_PATH).await.ok()?;
    serde_json::from_str::<LastGood>(&txt).ok()
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

/// The WFB radio's USB topology, for the power-contention diagnostic: the camera
/// brown-out happens when it shares a hub with this high-draw TX device.
#[cfg(target_os = "linux")]
pub(super) async fn read_radio_topo() -> Option<topo::UsbTopo> {
    let iface = read_wfb_iface().await?;
    topo::resolve_usb_topo(&iface).await
}

/// Extract the FC armed flag from one state-socket snapshot, reading whichever
/// wire form the producer emits (v1 newline JSON or v2 length-prefixed msgpack)
/// via the shared auto-detecting state reader. `None` on a clean EOF / framing
/// error / a snapshot with no `armed` field. Not cfg-gated so it builds + tests
/// on every host; the socket-connect wrapper stays Linux-only.
// The only non-test caller is `read_fc_armed`, which is Linux-only, so on other
// hosts this is exercised solely by its unit test — allow it to be otherwise
// unused there.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(super) async fn armed_from_state<R: tokio::io::AsyncRead + Unpin>(
    reader: &mut R,
) -> Option<bool> {
    ados_protocol::state::read_state_value(reader)
        .await
        .ok()
        .flatten()
        .and_then(|v| v.get("armed").and_then(|x| x.as_bool()))
}

/// Read the FC armed state from the vehicle-state socket (the MAVLink service
/// pushes a snapshot on connect, then a fresh one at ~10 Hz). `Some(armed)` on a
/// fresh read; `None` when the socket is absent / unreadable / carries no armed
/// field (an idle agent, no FC). The aggressive-hub-reset caller treats `None` as
/// unsafe (fail-closed): never reset the shared hub unless the FC is provably
/// disarmed, so the reset can never fire in flight.
#[cfg(target_os = "linux")]
pub(super) async fn read_fc_armed() -> Option<bool> {
    const STATE_SOCK: &str = "/run/ados/state.sock";
    let mut stream = tokio::time::timeout(
        Duration::from_secs(1),
        tokio::net::UnixStream::connect(STATE_SOCK),
    )
    .await
    .ok()?
    .ok()?;
    tokio::time::timeout(Duration::from_secs(1), armed_from_state(&mut stream))
        .await
        .ok()
        .flatten()
}

/// Build the protected set: the management link AND the WFB radio AND the FC. A
/// hub reset must disturb none of them.
#[cfg(target_os = "linux")]
pub(super) async fn build_protected_set() -> (Vec<topo::ControlPath>, Vec<topo::UsbTopo>) {
    use topo::ControlPath;
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
pub(super) fn device_present(bind_id: &str) -> bool {
    std::path::Path::new(USB_DEVICES_DIR)
        .join(bind_id)
        .join("idVendor")
        .is_file()
}

#[cfg(target_os = "linux")]
pub(super) async fn read_sysfs_id(bind_id: &str, attr: &str) -> String {
    let p = format!("{}/{}/{}", USB_DEVICES_DIR, bind_id, attr);
    tokio::fs::read_to_string(&p)
        .await
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

/// Whether the box is still inside the post-boot window (a proxy for "on the
/// ground, not yet flying"). Reads `/proc/uptime`.
#[cfg(target_os = "linux")]
pub(super) fn within_boot_window(window: Duration) -> bool {
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

#[cfg(target_os = "linux")]
pub(super) fn write_json_atomic<T: serde::Serialize>(
    path: &std::path::Path,
    value: &T,
    mode: u32,
) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("tmp");
    let body = serde_json::to_vec(value).map_err(std::io::Error::other)?;
    {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(mode)
            .open(&tmp)?;
        f.write_all(&body)?;
        f.sync_all()?;
    }
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(mode))?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn camera_usb_recovery_sidecar_version_matches_registry() {
        // The per-file const and the sidecar registry are the two sources of
        // truth for this sidecar's schema version; a drift is caught here.
        assert_eq!(
            CAMERA_USB_RECOVERY_SIDECAR_VERSION,
            ados_protocol::contracts::sidecar_version("camera-usb-recovery").unwrap()
        );
    }

    #[test]
    fn name_ancestors_synthesizes_from_bind_id() {
        assert_eq!(
            name_ancestors("1-1.1"),
            vec!["1-1".to_string(), "usb1".to_string()]
        );
        assert_eq!(name_ancestors("1-1"), vec!["usb1".to_string()]);
        assert_eq!(
            name_ancestors("2-1.4.3"),
            vec!["2-1.4".to_string(), "2-1".to_string(), "usb2".to_string()]
        );
    }

    #[tokio::test]
    async fn armed_from_state_reads_both_wire_forms_fail_closed() {
        use ados_protocol::state::{encode_v1, encode_v2};
        use serde_json::json;

        // v2 (length-prefixed msgpack), the form the producer actually emits.
        let mut armed_v2 = std::io::Cursor::new(encode_v2(&json!({"armed": true})).unwrap());
        assert_eq!(armed_from_state(&mut armed_v2).await, Some(true));

        let mut disarmed_v2 =
            std::io::Cursor::new(encode_v2(&json!({"armed": false, "mode": "GUIDED"})).unwrap());
        assert_eq!(armed_from_state(&mut disarmed_v2).await, Some(false));

        // v1 (newline JSON), the legacy form, still read via the auto-detect.
        let mut armed_v1 = std::io::Cursor::new(encode_v1(&json!({"armed": true})).unwrap());
        assert_eq!(armed_from_state(&mut armed_v1).await, Some(true));

        // Fail-closed: an empty reader (clean EOF) yields None.
        let mut empty = std::io::Cursor::new(Vec::<u8>::new());
        assert_eq!(armed_from_state(&mut empty).await, None);

        // Fail-closed: a snapshot with no `armed` field yields None.
        let mut no_field = std::io::Cursor::new(encode_v2(&json!({"mode": "STABILIZE"})).unwrap());
        assert_eq!(armed_from_state(&mut no_field).await, None);
    }
}
