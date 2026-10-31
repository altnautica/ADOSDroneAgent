//! Atomic promotion of a staged source tree over the live one.

use std::path::{Path, PathBuf};

use anyhow::Context as _;

/// Where a fresh clone is staged before it replaces the live tree (pure).
///
/// A SIBLING of the live tree, never a child of it: a child would move aside
/// with the live tree when the promotion swaps it out.
pub fn staging_dest(repo: &Path) -> PathBuf {
    let name = repo
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "source".to_string());
    repo.with_file_name(format!("{name}.staging"))
}

/// Replace the live source tree with a freshly cloned staging tree.
///
/// Called only after the clone has succeeded. The live tree is renamed aside
/// to `<name>.old` (not deleted) before the staging tree takes its place, and
/// renamed back if that rename fails, so a failed promotion never leaves the
/// node without `scripts/install.sh` and therefore no way to retry its own
/// install. The old tree is removed only once the new one is in place.
pub fn promote_staging(staging: &Path, repo: &Path) -> anyhow::Result<()> {
    let name = repo
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "source".to_string());
    let old = repo.with_file_name(format!("{name}.old"));
    if old.exists() {
        std::fs::remove_dir_all(&old)
            .with_context(|| format!("remove the stale previous tree {}", old.display()))?;
    }
    let had_live = repo.exists();
    if had_live {
        std::fs::rename(repo, &old)
            .with_context(|| format!("move the live source tree {} aside", repo.display()))?;
    }
    if let Err(e) = std::fs::rename(staging, repo) {
        if had_live {
            if let Err(back) = std::fs::rename(&old, repo) {
                tracing::error!(
                    error = %back,
                    path = %repo.display(),
                    "restoring the live source tree failed"
                );
            }
        }
        return Err(e).with_context(|| {
            format!(
                "promote the freshly cloned source tree into {}",
                repo.display()
            )
        });
    }
    if had_live {
        if let Err(e) = std::fs::remove_dir_all(&old) {
            tracing::warn!(error = %e, path = %old.display(), "removing the previous source tree failed");
        }
    }
    Ok(())
}
