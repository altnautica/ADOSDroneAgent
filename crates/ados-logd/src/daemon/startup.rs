//! Store open, verification and quarantine on startup.

use super::*;

/// Open the store and verify it. On a failed check the file is quarantined
/// (renamed with a timestamp suffix) and a fresh store is created from the
/// embedded schema, so a corrupt history cache never wedges the daemon. Returns
/// once a healthy store exists at `path`.
///
/// The boot-path guard is `quick_check`, NOT the full `integrity_check`: the
/// full check's per-index cross-validation scales with the store size, so on a
/// multi-hundred-MB store (the retention cap is gigabytes) it can run past the
/// unit's start timeout and wedge the daemon in a restart loop before it ever
/// signals readiness. `quick_check` still catches the gross structural
/// corruption that warrants a recreate, fast enough to keep startup bounded.
pub(super) fn open_and_verify(path: &Path) -> Result<()> {
    // A first open also runs migrations and creates the file + parent dir. A
    // structurally broken file (truncated or a non-SQLite header) fails here,
    // before any query runs: the connection opens but the first PRAGMA against a
    // bad header errors. Treat that the same as a failed structure check below —
    // quarantine the file and recreate — so a broken store self-heals in-process
    // instead of crash-looping the daemon onto the same bad file every restart.
    let conn = match db::open(path) {
        Ok(conn) => conn,
        Err(e) => {
            // Nothing to quarantine if the file does not exist: the open error
            // is then a real environmental fault (a bad parent dir, no space, a
            // permission problem) that a recreate would only hit again. Surface
            // it so the operator sees the true cause.
            if !path.exists() {
                return Err(anyhow::Error::new(e))
                    .with_context(|| format!("open store at {}", path.display()));
            }
            tracing::error!(
                error = %e,
                path = %path.display(),
                "store failed to open; quarantining and recreating"
            );
            quarantine_and_recreate(path)?;
            return Ok(());
        }
    };
    match db::quick_check(&conn) {
        Ok(()) => {
            tracing::info!(path = %path.display(), "store integrity check passed");
            // Reclaim dead copies on a healthy start too, not only when a fresh
            // corruption happens to run the prune. Otherwise a node that
            // corrupted twice under an older build carries both copies — up to
            // two gigabytes of unreadable file — until it corrupts a *third*
            // time, which is exactly the wrong moment to be short of space, and
            // is a decent way to cause that third corruption.
            prune_quarantines(path);
            Ok(())
        }
        Err(e) => {
            drop(conn);
            tracing::error!(
                error = %e,
                path = %path.display(),
                "store failed structure check; quarantining and recreating"
            );
            quarantine_and_recreate(path)?;
            Ok(())
        }
    }
}

/// Rename a broken store aside with a timestamped suffix and create a fresh one
/// from the embedded schema. The caller has already established the file at
/// `path` exists and is unusable.
pub(super) fn quarantine_and_recreate(path: &Path) -> Result<()> {
    let quarantine = path.with_extension(format!("db.corrupt-{}", now_us()));
    std::fs::rename(path, &quarantine).with_context(|| format!("quarantine {}", path.display()))?;
    tracing::warn!(
        quarantine = %quarantine.display(),
        path = %path.display(),
        "quarantined broken store; recreating from the embedded schema"
    );
    prune_quarantines(path);
    // Recreate from the embedded schema. Drop the fresh connection: the writer
    // reopens its own read-write handle, and the verify path only needs the
    // store to exist and be healthy.
    let _ = db::open(path).with_context(|| "recreate store after quarantine")?;
    Ok(())
}

/// How many quarantined stores to keep beside the live one.
///
/// One is enough to post-mortem the corruption that just happened, and the
/// older ones have already been superseded by it. Keeping every one is an
/// unbounded disk leak that nothing else reclaims: a field unit accumulated
/// 1.5 GB of quarantines next to a 934 MB live store, so the dead copies were
/// the larger half of the logging footprint.
pub(super) const QUARANTINE_KEEP: usize = 1;

/// Delete all but the newest [`QUARANTINE_KEEP`] quarantined stores beside
/// `path`.
///
/// Best effort throughout: this runs on the boot path after a corruption has
/// already been survived, so a directory that cannot be read or a file that
/// cannot be removed must never stop the daemon coming up. Ordering is by
/// modification time rather than by the timestamp in the name, because that
/// suffix is a plain integer whose digit count changes and so does not sort
/// lexicographically.
pub(super) fn prune_quarantines(path: &Path) {
    let Some(dir) = path.parent() else { return };
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut found: Vec<(std::time::SystemTime, std::path::PathBuf)> = entries
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().contains("db.corrupt-"))
        .filter_map(|e| {
            let modified = e.metadata().ok()?.modified().ok()?;
            Some((modified, e.path()))
        })
        .collect();
    if found.len() <= QUARANTINE_KEEP {
        return;
    }
    found.sort_by_key(|(t, _)| *t);
    let excess = found.len() - QUARANTINE_KEEP;
    for (_, victim) in found.into_iter().take(excess) {
        match std::fs::remove_file(&victim) {
            Ok(()) => tracing::info!(
                pruned = %victim.display(),
                "pruned a superseded quarantined store"
            ),
            Err(e) => tracing::warn!(
                error = %e,
                path = %victim.display(),
                "could not prune a quarantined store"
            ),
        }
    }
}
