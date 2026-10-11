//! The janitor's hands: the filesystem operations, one per category.
//!
//! Every function returns the bytes it actually freed, measured before and after
//! rather than assumed from what it intended to do. That number is what the
//! `janitor.reclaimed` event carries, so a category that silently did nothing
//! reports zero instead of reporting the size of what it meant to delete.
//!
//! Everything is best-effort. A file that cannot be removed is logged at debug
//! and skipped; a directory that does not exist is zero, not an error. The
//! janitor runs hourly on a box whose disk is the problem, so a pass that
//! partially succeeds and says so is worth more than one that raises.

use std::path::{Path, PathBuf};

/// Roots the janitor must never reclaim from, under any rung.
///
/// `/etc/ados` holds the config, the radio keys and the pairing state; `/opt/ados`
/// holds the installed runtime. Losing either turns a full disk into a box that
/// needs reflashing, which is the failure this whole effort exists to stop. The
/// guard is a deny-list checked at the single removal helper rather than a
/// convention followed by each caller, so a future category cannot forget it.
const PROTECTED_ROOTS: &[&str] = &["/etc/ados", "/opt/ados"];

mod apt;
pub use apt::{apt_archive_bytes, reclaim_apt_archives, reclaim_apt_lists};

mod logs;
pub use logs::{plugin_log_trimmable, trim_append_only, trim_plugin_logs};

/// True when `path` lies inside a protected root and must not be touched.
///
/// Compares path components rather than string prefixes, so `/etc/adosxyz` is
/// correctly judged as unprotected while `/etc/ados/pairing.json` is protected —
/// a prefix compare would confuse the two in one direction or the other.
pub fn is_protected(path: &Path) -> bool {
    PROTECTED_ROOTS.iter().any(|root| {
        let root = Path::new(root);
        path == root || path.starts_with(root)
    })
}

/// Remove one file, refusing anything under a protected root. Returns the bytes
/// freed (zero when the file was absent, protected, or could not be removed).
pub fn remove_file_guarded(path: &Path) -> u64 {
    if is_protected(path) {
        tracing::warn!(path = %path.display(), "janitor refused a protected path");
        return 0;
    }
    let len = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    match std::fs::remove_file(path) {
        Ok(()) => len,
        Err(e) => {
            tracing::debug!(path = %path.display(), error = %e, "janitor could not remove file");
            0
        }
    }
}

/// Sum the regular files directly under `dir`, plus one level of files inside
/// its subdirectories. Bounded on purpose: this is a report, not a survey, and a
/// deep walk on an SD card costs more than the number is worth.
pub fn dir_bytes(dir: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    let mut total = 0u64;
    for entry in entries.flatten() {
        let Ok(meta) = entry.metadata() else { continue };
        if meta.is_file() {
            total = total.saturating_add(meta.len());
        } else if meta.is_dir() {
            if let Ok(inner) = std::fs::read_dir(entry.path()) {
                for f in inner.flatten() {
                    if let Ok(m) = f.metadata() {
                        if m.is_file() {
                            total = total.saturating_add(m.len());
                        }
                    }
                }
            }
        }
    }
    total
}

/// How deep a footprint measurement walks. `/var/ados` and `/opt/ados` are a
/// handful of levels; a bound stops a symlinked or pathological tree turning a
/// size report into an unbounded walk on an SD card.
const TREE_MAX_DEPTH: usize = 8;

/// Total bytes of the regular files under `dir`, walking subdirectories to a
/// bounded depth. Symlinks are not followed, so a link into `/` cannot make the
/// agent's footprint read as the whole filesystem.
pub fn tree_bytes(dir: &Path) -> u64 {
    tree_bytes_depth(dir, 0)
}

fn tree_bytes_depth(dir: &Path, depth: usize) -> u64 {
    if depth > TREE_MAX_DEPTH {
        return 0;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    let mut total = 0u64;
    for entry in entries.flatten() {
        // `symlink_metadata` does not follow the link, so a symlinked directory
        // contributes its own (tiny) size rather than whatever it points at.
        let Ok(meta) = entry.metadata() else { continue };
        let Ok(link_meta) = entry.path().symlink_metadata() else {
            continue;
        };
        if link_meta.file_type().is_symlink() {
            continue;
        }
        if meta.is_file() {
            total = total.saturating_add(meta.len());
        } else if meta.is_dir() {
            total = total.saturating_add(tree_bytes_depth(&entry.path(), depth + 1));
        }
    }
    total
}

/// Reclaim oldest-first from a pre-enumerated set of entries until the total is
/// at or under `cap`, never taking anything in `protected`. Returns the bytes
/// freed.
///
/// Takes the entries rather than a name predicate because the two categories
/// that need this enumerate differently: quarantined stores are flat files under
/// one directory, recordings span the flat on-demand captures AND mediamtx's
/// per-path segment directories (see [`recording_entries`]). Names are relative
/// to `dir`, so `dir.join(name)` resolves either shape.
///
/// Oldest-first because within one category the oldest item is the least likely
/// to be wanted, and because it makes the outcome predictable: an operator can
/// say which recording will go next without reading this code.
///
/// A category can end up still over its cap — that is what the floor is for, and
/// a single quarantined store larger than the whole quarantine cap is the case
/// that actually happens on these boxes. The caller reports the residue rather
/// than looping.
pub fn reclaim_entries_to_cap_oldest_first(
    dir: &Path,
    mut entries: Vec<(String, i64)>,
    cap: u64,
    protected: &std::collections::BTreeSet<String>,
) -> u64 {
    let mut total: u64 = entries
        .iter()
        .filter_map(|(name, _)| std::fs::metadata(dir.join(name)).ok().map(|m| m.len()))
        .fold(0u64, |a, b| a.saturating_add(b));
    if total <= cap {
        return 0;
    }

    // Oldest first; a tie falls back to the name so the order is deterministic.
    entries.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| a.0.cmp(&b.0)));

    let mut freed = 0u64;
    for (name, _) in &entries {
        if total <= cap {
            break;
        }
        if protected.contains(name) {
            continue;
        }
        let gone = remove_file_guarded(&dir.join(name));
        if gone > 0 {
            total = total.saturating_sub(gone);
            freed = freed.saturating_add(gone);
        }
    }
    freed
}

/// `(name, mtime_unix)` for every entry directly under `dir` whose name matches
/// `keep`. An unreadable mtime sorts as epoch, which makes it a reclaim
/// candidate before anything datable — the safe direction for a file the
/// filesystem cannot date, since the alternative is treating it as newest and
/// never reclaiming it.
pub fn entries_with_mtime(dir: &Path, keep: impl Fn(&str) -> bool) -> Vec<(String, i64)> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_str()?.to_string();
            if !keep(&name) {
                return None;
            }
            let mtime = e
                .metadata()
                .ok()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            Some((name, mtime))
        })
        .collect()
}

/// True when `name` is one of the capture files the janitor owns. Anything else
/// in the recordings tree belongs to whoever put it there.
fn is_capture_name(name: &str) -> bool {
    name.ends_with(".mp4") || name.ends_with(".mkv") || name.ends_with(".ts")
}

/// `(name, mtime_unix)` for every capture file the janitor may consider, named
/// RELATIVE to `dir` so `dir.join(name)` resolves.
///
/// **Two layouts live under the recordings root at once.** The on-demand ground
/// recorder writes flat `<stamp>.mp4` captures into the root; mediamtx's native
/// recorder writes a continuous segment stream one level down, under
/// `<root>/<stream>/<stamp>.mp4`, because its record path carries `%path`. A
/// flat-only scan sees nothing of the second layout, which would leave the
/// footprint measurement (a bounded tree walk) reporting recordings permanently
/// over their cap while every reclaim found nothing to take — a janitor that
/// says the disk is full and does nothing about it.
///
/// One level, not a walk: `%path` is a single segment, and an unbounded descent
/// on an SD card costs more than the generality is worth.
pub fn recording_entries(dir: &Path) -> Vec<(String, i64)> {
    let mut out = entries_with_mtime(dir, is_capture_name);
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        if !meta.is_dir() {
            continue;
        }
        let Some(stream) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        for (name, mtime) in entries_with_mtime(&entry.path(), is_capture_name) {
            out.push((format!("{stream}/{name}"), mtime));
        }
    }
    out
}

/// How much reclaiming recordings would free at `cutoff_unix`, with the
/// `keep_newest` floor and whatever is in flight at `now_unix` protected.
pub fn recording_reclaimable_bytes(
    dir: &Path,
    cutoff_unix: i64,
    keep_newest: usize,
    now_unix: i64,
) -> u64 {
    let entries = recording_entries(dir);
    let protected = super::plan::protected_recordings(&entries, keep_newest, now_unix);
    super::plan::older_than_excluding(&entries, cutoff_unix, &protected)
        .iter()
        .filter_map(|name| std::fs::metadata(dir.join(name)).ok().map(|m| m.len()))
        .fold(0u64, |a, b| a.saturating_add(b))
}

/// Total bytes of every quarantined store, including the newest. The footprint
/// figure, as distinct from the reclaimable one — the corpse that can never be
/// taken still occupies the card and an operator sizing one needs to see it.
pub fn quarantine_bytes(dir: &Path) -> u64 {
    entries_with_mtime(dir, |name| name.starts_with("logs.db.corrupt-"))
        .iter()
        .filter_map(|(name, _)| std::fs::metadata(dir.join(name)).ok().map(|m| m.len()))
        .fold(0u64, |a, b| a.saturating_add(b))
}

/// How much pruning quarantined stores would free, with the newest `keep`
/// protected.
pub fn quarantine_reclaimable_bytes(dir: &Path, keep: usize) -> u64 {
    let entries = entries_with_mtime(dir, |name| name.starts_with("logs.db.corrupt-"));
    super::plan::beyond_newest(&entries, keep)
        .iter()
        .filter_map(|name| std::fs::metadata(dir.join(name)).ok().map(|m| m.len()))
        .fold(0u64, |a, b| a.saturating_add(b))
}

/// Reclaim recordings older than `cutoff_unix`, keeping the newest
/// `keep_newest` whatever their age and never touching what a capture is
/// writing at `now_unix`.
pub fn reclaim_recordings(dir: &Path, cutoff_unix: i64, keep_newest: usize, now_unix: i64) -> u64 {
    let entries = recording_entries(dir);
    let protected = super::plan::protected_recordings(&entries, keep_newest, now_unix);
    super::plan::older_than_excluding(&entries, cutoff_unix, &protected)
        .iter()
        .map(|name| remove_file_guarded(&dir.join(name)))
        .fold(0u64, |a, b| a.saturating_add(b))
}

/// Reclaim recordings oldest-first until `deficit` bytes have come off the
/// volume, honouring the same floor every other recordings rule honours.
///
/// **The space floor the time window cannot provide.** mediamtx's own
/// `recordDeleteAfter` is a 24h window; a high-bitrate day fills the volume long
/// before it elapses and the recorder then stops dead. This is the guard for
/// that, and it lives HERE rather than in a `runOnRecordSegmentComplete` hook
/// binary on purpose: the janitor already owns oldest-first recordings reclaim
/// with the keep-newest floor and the `janitor.reclaimed` accounting, and a
/// second reclaimer over the same directory would race it with no shared floor,
/// would only fire while segments were completing (so a volume filled by
/// anything else would never trigger it), and would not run at all if mediamtx
/// died mid-write — which is exactly when the volume is worst off.
///
/// The floor can stop this short of the deficit. That is the floor doing its
/// job: the alternative is deleting the capture being written to make room for
/// the capture being written. The caller reports the residue.
pub fn reclaim_recordings_to_free_floor(
    dir: &Path,
    deficit: u64,
    keep_newest: usize,
    now_unix: i64,
) -> u64 {
    if deficit == 0 {
        return 0;
    }
    let mut entries = recording_entries(dir);
    let protected = super::plan::protected_recordings(&entries, keep_newest, now_unix);
    // Oldest first; a tie falls back to the name so the order is deterministic.
    entries.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| a.0.cmp(&b.0)));
    let mut freed = 0u64;
    for (name, _) in &entries {
        if freed >= deficit {
            break;
        }
        if protected.contains(name) {
            continue;
        }
        freed = freed.saturating_add(remove_file_guarded(&dir.join(name)));
    }
    freed
}

/// Reclaim quarantined copies of a torn logging store, keeping the newest
/// `keep`. The newest one is the evidence of the most recent corruption and
/// survives every rung.
pub fn prune_quarantines(dir: &Path, keep: usize) -> u64 {
    let entries = entries_with_mtime(dir, |name| name.starts_with("logs.db.corrupt-"));
    let doomed = super::plan::beyond_newest(&entries, keep);
    doomed
        .iter()
        .map(|name| remove_file_guarded(&dir.join(name)))
        .fold(0u64, |a, b| a.saturating_add(b))
}

/// Vacuum the persistent journal down to `target_bytes`.
///
/// `journalctl --vacuum-size` rather than editing `journald.conf`: the cap in
/// the config drop-in is the steady-state policy an operator can read, and
/// rewriting it from a background reconciler would leave the box's stated policy
/// drifting from the installed one every time the disk got tight. The vacuum
/// reclaims now and leaves the policy alone.
#[cfg(target_os = "linux")]
pub async fn vacuum_journal(journal_dir: &Path, target_bytes: u64) -> u64 {
    let before = dir_bytes(journal_dir);
    if before == 0 {
        // No persistent journal on this box (volatile storage): nothing to do.
        return 0;
    }
    let arg = format!("--vacuum-size={target_bytes}");
    let out = tokio::process::Command::new("journalctl")
        .arg(&arg)
        .output()
        .await;
    match out {
        Ok(o) if o.status.success() => {}
        Ok(o) => {
            tracing::debug!(code = ?o.status.code(), "journal vacuum reported a non-zero status");
        }
        Err(e) => {
            tracing::debug!(error = %e, "journalctl not available for the vacuum");
            return 0;
        }
    }
    before.saturating_sub(dir_bytes(journal_dir))
}

#[cfg(not(target_os = "linux"))]
pub async fn vacuum_journal(_journal_dir: &Path, _target_bytes: u64) -> u64 {
    0
}

/// Free space on the filesystem holding `path`, as a percentage. `None` when it
/// cannot be read — which the plan treats as "do not escalate", never as zero.
#[cfg(target_os = "linux")]
pub fn free_pct(path: &Path) -> Option<f64> {
    let st = nix::sys::statvfs::statvfs(path).ok()?;
    let block = st.fragment_size() as u64;
    let total = st.blocks() as u64 * block;
    if total == 0 {
        return None;
    }
    let avail = st.blocks_available() as u64 * block;
    Some(avail as f64 * 100.0 / total as f64)
}

#[cfg(not(target_os = "linux"))]
pub fn free_pct(_path: &Path) -> Option<f64> {
    None
}

/// Free space on the filesystem holding `path`, in BYTES. `None` when it cannot
/// be read — which the plan treats as "do not act", never as zero.
///
/// Bytes as well as a percentage because the recording floor is an absolute:
/// the recorder stops when there is no room for the next fragment, and "8% free"
/// is a different amount of room on a 16 GB card than on a 512 GB one.
#[cfg(target_os = "linux")]
pub fn free_bytes(path: &Path) -> Option<u64> {
    let st = nix::sys::statvfs::statvfs(path).ok()?;
    let block = st.fragment_size() as u64;
    Some(st.blocks_available() as u64 * block)
}

#[cfg(not(target_os = "linux"))]
pub fn free_bytes(_path: &Path) -> Option<u64> {
    None
}

/// Resolve a path that honours `ADOS_VAR_DIR`-style test redirection through an
/// environment variable, falling back to the on-box default.
pub fn path_from_env(var: &str, default: &str) -> PathBuf {
    std::env::var_os(var)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(default))
}

#[cfg(test)]
mod tests;
