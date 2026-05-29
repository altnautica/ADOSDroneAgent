//! batman-adv mesh lifecycle for relay/receiver roles.
//!
//! Ports `MeshManager` from `mesh_manager.py`: bring up a second wireless
//! interface in 802.11s (preferred) or IBSS (fallback), bind it to `bat0`,
//! drive batman-adv gateway mode from role + uplink, poll neighbors/gateways,
//! and publish the `mesh-state.json` snapshot. Subprocess calls go through
//! `batctl::run` (tokio async `Command` + per-call timeout) so a wedged kernel
//! module cannot stall the poll loop.
//!
//! Out of scope here (matching the Python module's non-goals): pairing (that is
//! `pairing`), WFB fragment forwarding (`relay`/`receiver`), and cloud-uplink
//! bringup (this only reads the result and advertises it as a gateway).

use std::path::Path;
use std::time::Duration;

use crate::paths::{MESH_GATEWAY_JSON, MESH_ROLE_PATH};

use super::batctl;
use super::state::MeshSnapshot;

const POLL_INTERVAL: Duration = Duration::from_secs(2);
/// `/run/ados/uplink-active` sentinel: present ⟺ a cloud uplink is live.
const UPLINK_ACTIVE_FLAG: &str = "/run/ados/uplink-active";

/// Read the on-disk mesh-role sentinel, falling back to `direct` on a missing,
/// unreadable, or unknown value. Mirrors `role_manager.get_current_role`.
///
/// The operator-apply path (`role_manager.apply_role`: stop/start units, clear
/// stale snapshots, publish the event) is NOT ported here. It lives in the
/// shared `ados-supervisor` crate (`role::apply_role_on_boot` already covers the
/// boot mask/unmask), and the full operator transition stays in Python for now
/// to avoid touching a shared crate from this chunk. See the chunk report.
pub fn get_current_role() -> String {
    get_current_role_at(Path::new(MESH_ROLE_PATH))
}

/// Read the role sentinel from an explicit path (test seam).
pub fn get_current_role_at(path: &Path) -> String {
    if let Ok(text) = std::fs::read_to_string(path) {
        let v = text.trim();
        if matches!(v, "direct" | "relay" | "receiver") {
            return v.to_string();
        }
    }
    "direct".to_string()
}

/// True when the cloud-uplink sentinel is present.
pub fn has_uplink() -> bool {
    Path::new(UPLINK_ACTIVE_FLAG).is_file()
}

/// Re-apply the operator's last gateway pin on mesh setup. Reads
/// `/etc/ados/mesh/gateway.json` (`{"mode": ..., "pinned_mac": ...}`); only the
/// `pinned` mode with a non-null MAC triggers `batctl gw_sel`. Returns the
/// pinned MAC on success. Mirrors `_apply_persisted_gateway_preference`.
pub async fn apply_persisted_gateway_preference() -> Option<String> {
    let path = Path::new(MESH_GATEWAY_JSON);
    let text = std::fs::read_to_string(path).ok()?;
    let data: serde_json::Value = serde_json::from_str(&text).ok()?;
    if data.get("mode").and_then(|m| m.as_str()) != Some("pinned") {
        return None;
    }
    let pinned_mac = data.get("pinned_mac").and_then(|m| m.as_str())?;
    if pinned_mac.is_empty() {
        return None;
    }
    let (rc, _o, err) =
        batctl::run("batctl", &["gw_sel", pinned_mac], Duration::from_secs(5)).await;
    if rc != 0 {
        tracing::warn!(mac = pinned_mac, err = %err.trim(), "gateway_pin_apply_failed");
        return None;
    }
    tracing::info!(mac = pinned_mac, "gateway_pin_applied");
    Some(pinned_mac.to_string())
}

/// `modprobe batman-adv`. Returns false on failure.
pub async fn modprobe_batman() -> bool {
    let (rc, _o, err) = batctl::run("modprobe", &["batman-adv"], Duration::from_secs(10)).await;
    if rc != 0 {
        tracing::error!(err = %err.trim(), "modprobe_batman_failed");
        return false;
    }
    true
}

/// Bring up the mesh-side wireless interface in 802.11s (`carrier == "802.11s"`)
/// or IBSS mode. The 2.4 GHz channel maps to a frequency (channel 1 = 2412 MHz).
/// Mirrors `_bring_up_mesh_iface`.
pub async fn bring_up_mesh_iface(iface: &str, carrier: &str, mesh_id: &str, channel: u8) -> bool {
    // Flush to a clean baseline first.
    batctl::run(
        "ip",
        &["link", "set", iface, "down"],
        Duration::from_secs(5),
    )
    .await;
    batctl::run("iw", &["dev", iface, "disconnect"], Duration::from_secs(5)).await;

    let freq_mhz = if (1..=13).contains(&channel) {
        2407 + channel as i32 * 5
    } else {
        2412
    };
    let freq = freq_mhz.to_string();

    match carrier {
        "802.11s" => {
            let (rc, _o, e) = batctl::run(
                "iw",
                &["dev", iface, "set", "type", "mp"],
                Duration::from_secs(5),
            )
            .await;
            if rc != 0 {
                tracing::warn!(iface, err = %e.trim(), "iw_set_type_mp_failed");
            }
            batctl::run("ip", &["link", "set", iface, "up"], Duration::from_secs(5)).await;
            let (rc, _o, e) = batctl::run(
                "iw",
                &["dev", iface, "mesh", "join", mesh_id, "freq", &freq, "HT20"],
                Duration::from_secs(10),
            )
            .await;
            if rc != 0 {
                tracing::error!(iface, err = %e.trim(), "iw_mesh_join_failed");
                return false;
            }
        }
        "ibss" => {
            let (rc, _o, e) = batctl::run(
                "iw",
                &["dev", iface, "set", "type", "ibss"],
                Duration::from_secs(5),
            )
            .await;
            if rc != 0 {
                tracing::warn!(iface, err = %e.trim(), "iw_set_type_ibss_failed");
            }
            batctl::run("ip", &["link", "set", iface, "up"], Duration::from_secs(5)).await;
            let (rc, _o, e) = batctl::run(
                "iw",
                &["dev", iface, "ibss", "join", mesh_id, &freq, "HT20"],
                Duration::from_secs(10),
            )
            .await;
            if rc != 0 {
                tracing::error!(iface, err = %e.trim(), "iw_ibss_join_failed");
                return false;
            }
        }
        other => {
            tracing::error!(carrier = other, "unknown_carrier");
            return false;
        }
    }
    true
}

/// Bind the mesh interface to `bat_iface` and bring `bat0` up. Mirrors
/// `_bind_iface_to_bat`.
pub async fn bind_iface_to_bat(iface: &str, bat_iface: &str) -> bool {
    let (rc, _o, e) = batctl::run("batctl", &["if", "add", iface], Duration::from_secs(5)).await;
    if rc != 0 && !e.to_lowercase().contains("already") {
        tracing::error!(iface, err = %e.trim(), "batctl_if_add_failed");
        return false;
    }
    let (rc, _o, e) = batctl::run(
        "ip",
        &["link", "set", bat_iface, "up"],
        Duration::from_secs(5),
    )
    .await;
    if rc != 0 {
        tracing::error!(iface = bat_iface, err = %e.trim(), "bat_iface_up_failed");
        return false;
    }
    true
}

/// One poll pass: refresh neighbors + gateways + selected gateway into `snap`.
/// Returns the current neighbor MAC set (the caller diffs it for churn events,
/// which are published by the Python event bus, out of scope here).
pub async fn poll_once(snap: &mut MeshSnapshot) {
    let now_ms = now_ms();
    let (rc, out, _e) = batctl::run("batctl", &["n", "-H"], Duration::from_secs(3)).await;
    if rc == 0 {
        snap.neighbors = batctl::parse_neighbors(&out, now_ms);
    }
    let (rc, out, _e) = batctl::run("batctl", &["gwl", "-H"], Duration::from_secs(3)).await;
    if rc == 0 {
        snap.gateways = batctl::parse_gateways(&out);
        snap.selected_gateway = snap
            .gateways
            .iter()
            .find(|g| g.selected)
            .map(|g| g.mac.clone());
    }
    snap.last_poll_ms = now_ms;
}

/// Tear down the mesh: detach the iface from `bat0`, disconnect, and bring both
/// down. Mirrors `MeshManager.teardown`.
pub async fn teardown(mesh_iface: &str, bat_iface: &str) {
    if !mesh_iface.is_empty() {
        batctl::run("batctl", &["if", "del", mesh_iface], Duration::from_secs(5)).await;
        batctl::run(
            "iw",
            &["dev", mesh_iface, "disconnect"],
            Duration::from_secs(5),
        )
        .await;
        batctl::run(
            "ip",
            &["link", "set", mesh_iface, "down"],
            Duration::from_secs(5),
        )
        .await;
    }
    batctl::run(
        "ip",
        &["link", "set", bat_iface, "down"],
        Duration::from_secs(5),
    )
    .await;
}

/// Wall-clock unix milliseconds.
pub fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// The mesh poll loop: refresh + persist every `POLL_INTERVAL` until cancelled.
/// The caller spawns this after a successful `setup` on a relay/receiver node.
pub async fn run_poll_loop(mut snap: MeshSnapshot) {
    loop {
        poll_once(&mut snap).await;
        if let Err(e) = snap.write() {
            tracing::debug!(error = %e, "mesh_state_write_failed");
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn role_reader_falls_back_to_direct() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("role");
        // Missing → direct.
        assert_eq!(get_current_role_at(&p), "direct");
        // Valid values pass through.
        std::fs::write(&p, "relay\n").unwrap();
        assert_eq!(get_current_role_at(&p), "relay");
        std::fs::write(&p, "receiver\n").unwrap();
        assert_eq!(get_current_role_at(&p), "receiver");
        // Unknown → direct.
        std::fs::write(&p, "bogus\n").unwrap();
        assert_eq!(get_current_role_at(&p), "direct");
    }

    #[test]
    fn now_ms_is_positive() {
        assert!(now_ms() > 0);
    }
}
