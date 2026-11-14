//! The rig's serial LiDAR is never an FC candidate: the capture service owns
//! its port, and a baud sweep holding it (or a fallback open latching it)
//! keeps the LiDAR from starting.
//!
//! On every profile a port is the LiDAR when [`LidarPort`] identifies it: the
//! configured LiDAR device, or, for a declared LiDAR on USB discovery, a
//! CP2102 that another process holds or that answers the RPLIDAR `GET_INFO`
//! request. A `visor` keeps its stricter rule: every CP2102 is the LiDAR.

use ados_hal_probe::lidar_port::{probe_rplidar, LidarPort, LIDAR_USB_ID};

use super::discovery::FcCandidate;
use super::FcConnection;

/// Whether an enumerated port is the rig's serial LiDAR on a `visor` profile,
/// where the auto sweep must never open any CP2102.
pub(super) fn is_rig_lidar_on_visor(profile: &str, vid: u16, pid: u16) -> bool {
    profile == "visor" && (vid, pid) == LIDAR_USB_ID
}

/// Whether the candidate `port` (USB id `usb`) is the rig's LiDAR. `confirm`
/// probes a CP2102 when the rule needs it.
pub(super) fn is_lidar_port(
    profile: &str,
    lidar: &LidarPort,
    port: &str,
    usb: Option<(u16, u16)>,
    confirm: impl FnOnce(&str) -> bool,
) -> bool {
    if matches!(usb, Some((vid, pid)) if is_rig_lidar_on_visor(profile, vid, pid)) {
        return true;
    }
    lidar.identifies(port, usb, confirm)
}

/// `candidates` without the LiDAR's port, order kept.
pub(super) fn drop_lidar_ports(
    profile: &str,
    lidar: &LidarPort,
    candidates: Vec<FcCandidate>,
    mut confirm: impl FnMut(&str) -> bool,
) -> Vec<FcCandidate> {
    candidates
        .into_iter()
        .filter(|c| {
            let is_lidar = is_lidar_port(profile, lidar, &c.name, c.usb, &mut confirm);
            if is_lidar {
                tracing::info!(port = %c.name, "fc_candidate_is_the_lidar_skipped");
            }
            !is_lidar
        })
        .collect()
}

impl FcConnection {
    /// The enumerated candidates without the rig's LiDAR port. A configured
    /// `serial_port` is the operator's pin and is kept as configured. Runs on
    /// the blocking pool: the rule reads the rig and the plugin config, and
    /// may probe a CP2102 port.
    pub(super) async fn without_lidar(&self, candidates: Vec<FcCandidate>) -> Vec<FcCandidate> {
        if !self.cfg.serial_port.is_empty() || candidates.is_empty() {
            return candidates;
        }
        let profile = self.cfg.profile.clone();
        let task = tokio::task::spawn_blocking(move || {
            let lidar = LidarPort::load();
            drop_lidar_ports(&profile, &lidar, candidates, |port| {
                probe_rplidar(port).is_lidar()
            })
        });
        match task.await {
            Ok(kept) => kept,
            Err(e) => {
                tracing::warn!(error = %e, "fc_lidar_exclusion_failed");
                Vec::new()
            }
        }
    }
}
