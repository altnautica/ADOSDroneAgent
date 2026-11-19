//! Replace the first-party extensions this agent no longer runs with the ones
//! that took their place, on every install whatever the Reconstruct toggle.
//!
//! - `com.altnautica.vision-nav` became Navigation (`com.altnautica.navigation`)
//!   on every profile it ran on.
//! - `com.altnautica.world-engine` split into Capture, Reconstruct and
//!   Navigation: a drone or Visor gets Navigation (the pilot reads its scans
//!   and poses) and Capture, a ground station Capture, a workstation or
//!   compute node Reconstruct.
//!
//! Two generations of one job never run side by side: they would claim the
//! same LiDAR and the same MAVLink component ids. So every installed retired
//! extension is disabled before any successor installs. A retired extension is
//! removed (data kept) once all its successors are in; switched back on, when
//! it was on, only if none of them installed (no release in the bundled
//! catalog, or a failed download), so the node keeps working until a later
//! install finishes the swap; and left off when only some installed. A
//! successor is switched on only when a retired extension it replaces was on;
//! otherwise it installs off, its grants recorded. Settings start from the
//! successor's defaults; nothing is carried over.

use std::time::Duration;

use ados_plugin_host::download::DownloadSource;
use ados_plugin_host::{PluginStatus, PluginSupervisor};

use super::{install_extension, ExtensionOutcome, RECONSTRUCT_ID};

pub const VISION_NAV_ID: &str = "com.altnautica.vision-nav";
pub const WORLD_ENGINE_ID: &str = "com.altnautica.world-engine";
pub const NAVIGATION_ID: &str = "com.altnautica.navigation";
pub const CAPTURE_ID: &str = "com.altnautica.capture";

/// Every extension this agent retired, in the order they are handled.
const RETIRED: [&str; 2] = [VISION_NAV_ID, WORLD_ENGINE_ID];

/// The extensions that take `retired`'s place on a node of `profile`; empty
/// for an extension that was not retired.
pub fn successors(retired: &str, profile: &str) -> &'static [&'static str] {
    match retired {
        VISION_NAV_ID => &[NAVIGATION_ID],
        WORLD_ENGINE_ID => match profile {
            "workstation" | "compute" => &[RECONSTRUCT_ID],
            "drone" | "visor" => &[NAVIGATION_ID, CAPTURE_ID],
            _ => &[CAPTURE_ID],
        },
        _ => &[],
    }
}

/// What one retirement did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Retirement {
    /// Every successor is in and the retired extension is gone.
    Replaced {
        retired: &'static str,
        successors: &'static [&'static str],
        outcomes: Vec<ExtensionOutcome>,
    },
    /// Not every successor could be installed; the retired extension stays.
    /// It is back in the state it was found in when none installed, and off
    /// when some did.
    Kept {
        retired: &'static str,
        successors: &'static [&'static str],
        reason: String,
    },
    /// Every successor is in, but removing the retired extension failed.
    NotRemoved {
        retired: &'static str,
        successors: &'static [&'static str],
        reason: String,
    },
}

/// One installed retired extension.
struct Found {
    retired: &'static str,
    successors: &'static [&'static str],
    on: bool,
}

/// What [`retire_all`] did: one [`Retirement`] per retired extension found,
/// and the successors it installed but left off on purpose (every retired
/// extension they replace was off), which a later default-on install must not
/// switch on.
#[derive(Debug, Default)]
pub struct RetireAll {
    pub retirements: Vec<Retirement>,
    pub left_off: Vec<&'static str>,
}

/// Retire every extension this agent replaced that is installed here.
pub fn retire_all(
    supervisor: &mut PluginSupervisor,
    catalog_json: &str,
    source: &dyn DownloadSource,
    guard_wait: Duration,
    retry_delay: Duration,
) -> RetireAll {
    if supervisor.discover().is_err() {
        return RetireAll::default();
    }
    let profile = supervisor.profile().to_string();
    let found: Vec<Found> = RETIRED
        .into_iter()
        .filter_map(|retired| {
            let status = supervisor.find_install(retired)?.status;
            let successors = successors(retired, &profile);
            (!successors.is_empty()).then_some(Found {
                retired,
                successors,
                on: matches!(status, PluginStatus::Enabled | PluginStatus::Running),
            })
        })
        .collect();
    if found.is_empty() {
        return RetireAll::default();
    }

    // Stop every retired extension before any successor installs. One that
    // will not stop calls the whole swap off: its successors would run beside
    // it.
    for (i, f) in found.iter().enumerate() {
        if !f.on {
            continue;
        }
        if let Err(e) = supervisor.disable(f.retired) {
            for earlier in found[..i].iter().filter(|g| g.on) {
                switch_back_on(supervisor, earlier.retired);
            }
            let reason = format!("{} could not be stopped for the swap: {e}", f.retired);
            let retirements = found
                .iter()
                .map(|g| Retirement::Kept {
                    retired: g.retired,
                    successors: g.successors,
                    reason: reason.clone(),
                })
                .collect();
            return RetireAll {
                retirements,
                left_off: Vec::new(),
            };
        }
    }

    // Install each successor once, switched on only when a retired extension
    // it replaces was on.
    let mut installed: Vec<(&'static str, Result<ExtensionOutcome, String>)> = Vec::new();
    let mut left_off = Vec::new();
    for f in &found {
        for &successor in f.successors {
            if installed.iter().any(|(id, _)| *id == successor) {
                continue;
            }
            let enable = found
                .iter()
                .any(|g| g.on && g.successors.contains(&successor));
            let result = match install_extension(
                supervisor,
                successor,
                catalog_json,
                source,
                guard_wait,
                retry_delay,
                enable,
            ) {
                Ok(ExtensionOutcome::Unavailable(reason)) => Err(reason),
                other => other,
            };
            if !enable && result.is_ok() {
                left_off.push(successor);
            }
            installed.push((successor, result));
        }
    }

    let retirements = found
        .iter()
        .map(|f| settle(supervisor, f, &installed))
        .collect();
    RetireAll {
        retirements,
        left_off,
    }
}

/// Retire every replaced extension, then, when `reconstruct` is set, install
/// Reconstruct switched on, except when the retirement just installed it and
/// left it off on purpose (the World Engine extension it replaces was off).
pub fn retire_then_install(
    supervisor: &mut PluginSupervisor,
    catalog_json: &str,
    source: &dyn DownloadSource,
    guard_wait: Duration,
    retry_delay: Duration,
    reconstruct: bool,
) -> super::NodeExtensions {
    let done = retire_all(supervisor, catalog_json, source, guard_wait, retry_delay);
    let reconstruct = reconstruct.then(|| {
        install_extension(
            supervisor,
            RECONSTRUCT_ID,
            catalog_json,
            source,
            guard_wait,
            retry_delay,
            !done.left_off.contains(&RECONSTRUCT_ID),
        )
    });
    super::NodeExtensions {
        retired: done.retirements,
        reconstruct,
    }
}

/// Finish one retirement from the successors' install results.
fn settle(
    supervisor: &mut PluginSupervisor,
    f: &Found,
    installed: &[(&'static str, Result<ExtensionOutcome, String>)],
) -> Retirement {
    let results: Vec<&Result<ExtensionOutcome, String>> = f
        .successors
        .iter()
        .filter_map(|s| installed.iter().find(|(id, _)| id == s).map(|(_, r)| r))
        .collect();
    let missing: Vec<String> = f
        .successors
        .iter()
        .zip(&results)
        .filter_map(|(s, r)| r.as_ref().err().map(|e| format!("{s}: {e}")))
        .collect();
    if missing.is_empty() {
        let outcomes = results.into_iter().flatten().cloned().collect();
        return match supervisor.remove(f.retired, true) {
            Ok(_) => Retirement::Replaced {
                retired: f.retired,
                successors: f.successors,
                outcomes,
            },
            Err(e) => Retirement::NotRemoved {
                retired: f.retired,
                successors: f.successors,
                reason: e.to_string(),
            },
        };
    }
    let none_in = missing.len() == f.successors.len();
    if none_in && f.on {
        switch_back_on(supervisor, f.retired);
    }
    let state = if none_in {
        "left as it was"
    } else {
        "left off so it never runs beside its installed successor"
    };
    Retirement::Kept {
        retired: f.retired,
        successors: f.successors,
        reason: format!("{} ({state})", missing.join("; ")),
    }
}

fn switch_back_on(supervisor: &mut PluginSupervisor, retired: &str) {
    if let Err(e) = supervisor.enable(retired) {
        tracing::warn!(plugin = retired, error = %e, "the retired extension could not be switched back on");
    }
}

/// Log what a retirement did; returns whether it left the node needing
/// attention: a retired extension still installed runs a generation whose
/// topics this agent no longer reads, so the operator re-runs the installer
/// once the successors can install.
pub fn report(r: &Retirement) -> bool {
    match r {
        Retirement::Replaced {
            retired,
            successors,
            ..
        } => {
            tracing::info!(
                retired,
                successors = ?successors,
                "replaced a retired extension; its settings start from defaults"
            );
            false
        }
        Retirement::Kept {
            retired,
            successors,
            reason,
        } => {
            tracing::warn!(
                retired,
                successors = ?successors,
                "kept a retired extension this agent no longer reads; re-run the installer \
                 once its successors can install: {reason}"
            );
            true
        }
        Retirement::NotRemoved {
            retired,
            successors,
            reason,
        } => {
            tracing::warn!(
                retired,
                successors = ?successors,
                "the successors are in but the retired extension stayed: {reason}"
            );
            true
        }
    }
}

#[cfg(test)]
#[path = "extensions_retire_tests.rs"]
mod tests;
