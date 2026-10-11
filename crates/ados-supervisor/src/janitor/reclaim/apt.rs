//! apt cache reclaim: downloaded archives and package lists.

use super::*;

/// Empty the apt archive cache: the `.deb` files apt downloaded to install
/// something, which nothing re-reads afterwards. This is what `apt-get clean`
/// does, done directly so a running apt elsewhere on the box cannot make the
/// janitor wait on a lock.
pub fn reclaim_apt_archives(dir: &Path) -> u64 {
    let mut freed = 0u64;
    for sub in [dir.to_path_buf(), dir.join("partial")] {
        let Ok(entries) = std::fs::read_dir(&sub) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if name.ends_with(".deb") || name.ends_with(".ddeb") || name.ends_with(".deb.partial") {
                freed = freed.saturating_add(remove_file_guarded(&entry.path()));
            }
        }
    }
    freed
}

/// The bytes the apt archive cache is holding, without removing them. Same
/// selection as [`reclaim_apt_archives`], so the reported figure is what a pass
/// would actually take.
pub fn apt_archive_bytes(dir: &Path) -> u64 {
    let mut total = 0u64;
    for sub in [dir.to_path_buf(), dir.join("partial")] {
        let Ok(entries) = std::fs::read_dir(&sub) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if name.ends_with(".deb") || name.ends_with(".ddeb") || name.ends_with(".deb.partial") {
                total = total.saturating_add(entry.metadata().map(|m| m.len()).unwrap_or(0));
            }
        }
    }
    total
}

/// Entries under `/var/lib/apt/lists` that must survive. Mirrors the installer's
/// one-shot reclaim; kept here rather than shared because the two run in
/// different binaries and neither should depend on the other.
pub(super) const LISTS_KEEP: &[&str] = &["lock", "partial", "auxfiles"];

/// Give up the apt package index. The next apt invocation must `apt-get update`
/// first; every apt path the agent owns already does, and a human is told to by
/// apt itself. Only reached at the escalated rungs.
pub fn reclaim_apt_lists(dir: &Path) -> u64 {
    if !dir.is_dir() {
        return 0;
    }
    let before = dir_bytes(dir);
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if LISTS_KEEP.contains(&name) {
            continue;
        }
        let path = entry.path();
        if is_protected(&path) {
            continue;
        }
        let removed = if path.is_dir() {
            std::fs::remove_dir_all(&path)
        } else {
            std::fs::remove_file(&path)
        };
        if let Err(e) = removed {
            tracing::debug!(path = %path.display(), error = %e, "janitor could not prune apt list");
        }
    }
    if let Ok(entries) = std::fs::read_dir(dir.join("partial")) {
        for entry in entries.flatten() {
            let _ = std::fs::remove_file(entry.path());
        }
    }
    before.saturating_sub(dir_bytes(dir))
}
