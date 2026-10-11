//! The revocation watch loop.

use super::*;

/// How often the revocation list is checked for a change, and how often a
/// deferred revocation re-checks the arm state.
pub(super) const REVOCATION_POLL: std::time::Duration = std::time::Duration::from_secs(5);

/// Whether this node's vehicle state can read armed: a drone, or a ground
/// station relaying its drone's state. Elsewhere nothing flies.
pub(super) fn is_vehicle_profile(profile: &str) -> bool {
    matches!(profile, "drone" | "ground_station" | "ground-station")
}

/// Apply `revocations` with the current arm verdict. Logged, never fatal:
/// the next pass retries. Returns whether a disable is still deferred.
pub(super) fn enforce_revocations(paths: &Paths, revocations: &Revocations, armed: bool) -> bool {
    let mut supervisor = production_supervisor(paths);
    match supervisor.apply_revocations(revocations, armed) {
        Ok(sweep) => {
            if !sweep.disabled.is_empty() {
                tracing::warn!(disabled = ?sweep.disabled, "plugins disabled after a revocation");
            }
            !sweep.deferred.is_empty()
        }
        Err(e) => {
            tracing::error!(error = %e, "could not apply the revocation list");
            true
        }
    }
}

/// Read the revocation list, keeping `last_good` when the file cannot be
/// read: running plugins are not changed by a garbled list, while the
/// supervisor refuses new installs and enables of signed plugins until it reads.
pub(super) fn refresh_revocations(path: &Path, last_good: &mut Revocations) {
    match load_revocations(path) {
        Ok(revocations) => *last_good = revocations,
        Err(e) => {
            tracing::error!(error = %e, "plugin_revocation_list_unreadable; keeping the last good list")
        }
    }
}

/// Re-apply the revocation list whenever the file changes (its mtime or its
/// presence), on a fixed poll that never gives up. While the vehicle counts as
/// armed a disable is deferred (each pass warns `plugin_revocation_deferred_armed`
/// per plugin, so every 5 s) and applied on the first pass after disarm.
pub(super) fn spawn_revocation_watch(
    paths: Paths,
    vehicle_armed: Arc<VehicleArmedProbe>,
    mut last_good: Revocations,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let path = PathBuf::from(PLUGIN_REVOCATIONS_PATH);
        let stamp = |p: &Path| std::fs::metadata(p).and_then(|m| m.modified()).ok();
        let mut seen = stamp(&path);
        // The startup pass ran before any heartbeat arrived, so it may have
        // deferred: check again on the first tick.
        let mut pending = true;
        let mut ticker = tokio::time::interval(REVOCATION_POLL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            ticker.tick().await;
            let now = stamp(&path);
            let changed = now != seen;
            if changed {
                seen = now;
                refresh_revocations(&path, &mut last_good);
            }
            if !changed && !pending {
                continue;
            }
            let armed = vehicle_armed.treated_as_armed();
            let (paths, revocations) = (paths.clone(), last_good.clone());
            match tokio::task::spawn_blocking(move || {
                enforce_revocations(&paths, &revocations, armed)
            })
            .await
            {
                Ok(still_pending) => pending = still_pending,
                Err(e) => tracing::error!(error = %e, "revocation enforcement task failed"),
            }
        }
    })
}
