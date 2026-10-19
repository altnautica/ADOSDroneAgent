//! Settle the discovered primary against the camera roster.
//!
//! The Python probe ranks cameras (real cameras only, CSI then USB then IP)
//! knowing nothing of the `cameras[]` roster. Two roster facts override that
//! ranking: a primary leg that names its local device gets that device when
//! the probe found it, and a device a `capture: true` camera owns is never the
//! primary, because the vision engine holds it open.

use super::{DiscoveredCamera, DiscoveryResult, Primary};

/// The probe's ranking of camera types (`CameraManager.auto_assign`).
const TYPE_RANK: [&str; 3] = ["csi", "usb", "ip"];
/// The probe's `hardware_role` for a real camera (codecs and ISPs are not).
const CAMERA_ROLE: &str = "camera";

/// Whether two device paths name the same node, following symlinks such as
/// `/dev/v4l/by-id/...`.
fn same_device(a: &str, b: &str) -> bool {
    a == b
        || matches!(
            (std::fs::canonicalize(a), std::fs::canonicalize(b)),
            (Ok(x), Ok(y)) if x == y
        )
}

fn primary_of(camera: &DiscoveredCamera) -> Primary {
    Primary {
        device_path: camera.device_path.clone(),
        name: camera.name.clone(),
    }
}

impl DiscoveryResult {
    /// Settle the primary. `configured` is the primary leg's source and
    /// `owned` the device paths of the captured cameras. In order: the
    /// configured local device (a `/dev/` path) when the probe found it and no
    /// captured camera owns it; else the probe's own pick when not owned; else
    /// the next camera in the probe's ranking that is not owned. With no such
    /// camera there is no primary, so the encoder never opens an owned device.
    pub fn assign_primary(&mut self, configured: &str, owned: &[&str]) {
        let free = |path: &str| !owned.iter().any(|o| same_device(o, path));
        let configured = configured.trim();
        if configured.starts_with("/dev/") && free(configured) {
            if let Some(found) = self
                .cameras
                .iter()
                .find(|c| same_device(&c.device_path, configured))
            {
                self.primary = Some(primary_of(found));
                return;
            }
        }
        if self.primary.as_ref().is_some_and(|p| free(&p.device_path)) {
            return;
        }
        self.primary = TYPE_RANK
            .iter()
            .find_map(|kind| {
                self.cameras.iter().find(|c| {
                    c.camera_type == *kind && c.hardware_role == CAMERA_ROLE && free(&c.device_path)
                })
            })
            .map(primary_of);
    }
}

#[cfg(test)]
#[path = "assign_tests.rs"]
mod tests;
