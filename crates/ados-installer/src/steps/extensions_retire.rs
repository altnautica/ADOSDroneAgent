//! Replace the first-party extensions this agent no longer runs with the ones
//! that took their place, on every install whatever the Reconstruct toggle.
//!
//! - `com.altnautica.vision-nav` became Navigation (`com.altnautica.navigation`)
//!   on every profile it ran on.
//! - `com.altnautica.world-engine` split into Capture, Reconstruct and
//!   Navigation: a drone, Visor or ground station gets Capture in its place (it
//!   was capturing there), a workstation or compute node Reconstruct.
//!
//! Two generations of one job never run side by side: they would claim the
//! same LiDAR and the same MAVLink component ids. So the retired extension is
//! disabled before its successor installs, removed (data kept) once the
//! successor is in, and switched back on if the successor cannot be installed
//! yet (no release in the bundled catalog, or a failed download), so the node
//! keeps working until a later install finishes the swap. Settings start from
//! the successor's defaults; nothing is carried over.

use std::time::Duration;

use ados_plugin_host::download::DownloadSource;
use ados_plugin_host::{PluginStatus, PluginSupervisor};

use super::{install_from_catalog, ExtensionOutcome, RECONSTRUCT_ID};

pub const VISION_NAV_ID: &str = "com.altnautica.vision-nav";
pub const WORLD_ENGINE_ID: &str = "com.altnautica.world-engine";
pub const NAVIGATION_ID: &str = "com.altnautica.navigation";
pub const CAPTURE_ID: &str = "com.altnautica.capture";

/// The extension that takes `retired`'s place on a node of `profile`.
pub fn successor(retired: &str, profile: &str) -> Option<&'static str> {
    match retired {
        VISION_NAV_ID => Some(NAVIGATION_ID),
        WORLD_ENGINE_ID => Some(if matches!(profile, "workstation" | "compute") {
            RECONSTRUCT_ID
        } else {
            CAPTURE_ID
        }),
        _ => None,
    }
}

/// What one retirement did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Retirement {
    /// The successor is in and the retired extension is gone.
    Replaced {
        retired: &'static str,
        successor: &'static str,
        outcome: ExtensionOutcome,
    },
    /// The successor could not be installed; the retired extension stays, in
    /// the state it was found in.
    Kept {
        retired: &'static str,
        successor: &'static str,
        reason: String,
    },
    /// The successor is in, but removing the retired extension failed.
    NotRemoved {
        retired: &'static str,
        successor: &'static str,
        reason: String,
    },
}

/// Retire every extension this agent replaced that is installed here.
pub fn retire_all(
    supervisor: &mut PluginSupervisor,
    catalog_json: &str,
    source: &dyn DownloadSource,
    guard_wait: Duration,
    retry_delay: Duration,
) -> Vec<Retirement> {
    [VISION_NAV_ID, WORLD_ENGINE_ID]
        .into_iter()
        .filter_map(|id| {
            retire(
                supervisor,
                id,
                catalog_json,
                source,
                guard_wait,
                retry_delay,
            )
        })
        .collect()
}

/// Retire `retired` when it is installed; `None` when it is not.
pub fn retire(
    supervisor: &mut PluginSupervisor,
    retired: &'static str,
    catalog_json: &str,
    source: &dyn DownloadSource,
    guard_wait: Duration,
    retry_delay: Duration,
) -> Option<Retirement> {
    if supervisor.discover().is_err() {
        return None;
    }
    let was = supervisor.find_install(retired)?.status;
    let successor = successor(retired, supervisor.profile())?;
    let on = matches!(was, PluginStatus::Enabled | PluginStatus::Running);
    if on {
        if let Err(e) = supervisor.disable(retired) {
            return Some(Retirement::Kept {
                retired,
                successor,
                reason: format!("it could not be stopped for the swap: {e}"),
            });
        }
    }
    let installed = install_from_catalog(
        supervisor,
        successor,
        catalog_json,
        source,
        guard_wait,
        retry_delay,
    );
    let outcome = match installed {
        Ok(ExtensionOutcome::Unavailable(reason)) | Err(reason) => {
            if on {
                if let Err(e) = supervisor.enable(retired) {
                    tracing::warn!(plugin = retired, error = %e, "the retired extension could not be switched back on");
                }
            }
            return Some(Retirement::Kept {
                retired,
                successor,
                reason,
            });
        }
        Ok(outcome) => outcome,
    };
    Some(match supervisor.remove(retired, true) {
        Ok(_) => Retirement::Replaced {
            retired,
            successor,
            outcome,
        },
        Err(e) => Retirement::NotRemoved {
            retired,
            successor,
            reason: e.to_string(),
        },
    })
}

/// Log what a retirement did; returns whether it left the node needing a
/// re-run (a successor that did not install is the release not being out
/// yet, which is not a failure).
pub fn report(r: &Retirement) -> bool {
    match r {
        Retirement::Replaced {
            retired, successor, ..
        } => {
            tracing::info!(
                retired,
                successor,
                "replaced a retired extension; its settings start from defaults"
            );
            false
        }
        Retirement::Kept {
            retired,
            successor,
            reason,
        } => {
            tracing::warn!(
                retired,
                successor,
                "kept a retired extension until its successor can install: {reason}"
            );
            false
        }
        Retirement::NotRemoved {
            retired,
            successor,
            reason,
        } => {
            tracing::warn!(
                retired,
                successor,
                "the successor is in but the retired extension stayed: {reason}"
            );
            true
        }
    }
}

#[cfg(test)]
#[path = "extensions_retire_tests.rs"]
mod tests;
