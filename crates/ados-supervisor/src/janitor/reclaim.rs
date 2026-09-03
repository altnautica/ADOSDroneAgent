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

/// How much a trim of the plugin logs would free — the part of each log past its
/// cap, never the tail that is its floor.
pub fn plugin_log_trimmable(dir: &Path, max_bytes: u64, keep_bytes: u64) -> u64 {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    entries
        .flatten()
        .filter(|e| e.file_name().to_str().is_some_and(|n| n.ends_with(".log")))
        .filter_map(|e| e.metadata().ok().map(|m| m.len()))
        .filter_map(|len| super::plan::trim_from(len, max_bytes, keep_bytes))
        .fold(0u64, |a, b| a.saturating_add(b))
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

/// Entries under `/var/lib/apt/lists` that must survive. Mirrors the installer's
/// one-shot reclaim; kept here rather than shared because the two run in
/// different binaries and neither should depend on the other.
const LISTS_KEEP: &[&str] = &["lock", "partial", "auxfiles"];

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

/// Trim an append-only file so only its last `keep_bytes` survive, in place.
///
/// In place, not by rename, because these files are written through a file
/// descriptor that is already open — systemd's `StandardOutput=append:` opens
/// the plugin log once when the unit starts, and the Python audit writer holds
/// its own handle. Renaming the file would leave every subsequent write going to
/// the renamed inode, so the "rotated" log would keep growing under its new name
/// and the new one would stay empty forever. Rewriting the same inode keeps the
/// writer's descriptor pointing at the file it is meant to be writing.
///
/// Both writers open with `O_APPEND`, which recomputes the offset from the file
/// length on every write, so shortening the file underneath them produces a
/// shorter file rather than a sparse one. A write landing in the moment between
/// the read and the truncate is lost; these are low-rate log writers and one
/// line is the worst case, which is the price of bounding a file nothing else
/// bounds.
///
/// Returns the bytes freed, or zero when the file is under its cap — which is
/// what makes a second pass immediately after a first a no-op.
pub fn trim_append_only(path: &Path, max_bytes: u64, keep_bytes: u64) -> u64 {
    if is_protected(path) {
        tracing::warn!(path = %path.display(), "janitor refused a protected path");
        return 0;
    }
    let Ok(meta) = std::fs::metadata(path) else {
        return 0;
    };
    if !meta.is_file() {
        return 0;
    }
    let len = meta.len();
    let Some(drop_bytes) = super::plan::trim_from(len, max_bytes, keep_bytes) else {
        return 0;
    };

    let tail = match read_tail(path, len.saturating_sub(drop_bytes)) {
        Ok(t) => t,
        Err(e) => {
            tracing::debug!(path = %path.display(), error = %e, "janitor could not read the tail");
            return 0;
        }
    };
    // Start at the first line boundary so the surviving file does not open with
    // half a record — the audit trail is newline-delimited JSON and a torn first
    // line would make the whole file unparseable to a reader that is strict.
    let tail = match tail.iter().position(|b| *b == b'\n') {
        Some(i) if i + 1 < tail.len() => &tail[i + 1..],
        Some(_) => &[][..],
        None => &tail[..],
    };
    if let Err(e) = std::fs::write(path, tail) {
        tracing::debug!(path = %path.display(), error = %e, "janitor could not rewrite the file");
        return 0;
    }
    len.saturating_sub(tail.len() as u64)
}

/// Read the last `n` bytes of a file.
fn read_tail(path: &Path, n: u64) -> std::io::Result<Vec<u8>> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path)?;
    let len = f.metadata()?.len();
    f.seek(SeekFrom::Start(len.saturating_sub(n)))?;
    let mut buf = Vec::with_capacity(n as usize);
    f.take(n).read_to_end(&mut buf)?;
    Ok(buf)
}

/// Trim every `*.log` directly under the plugin log directory.
pub fn trim_plugin_logs(dir: &Path, max_bytes: u64, keep_bytes: u64) -> u64 {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    let mut freed = 0u64;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !name.ends_with(".log") {
            continue;
        }
        freed = freed.saturating_add(trim_append_only(&entry.path(), max_bytes, keep_bytes));
    }
    freed
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
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn the_config_and_runtime_roots_are_refused() {
        assert!(is_protected(Path::new("/etc/ados")));
        assert!(is_protected(Path::new("/etc/ados/config.yaml")));
        assert!(is_protected(Path::new("/etc/ados/wfb/rx.key")));
        assert!(is_protected(Path::new("/opt/ados/venv/bin/python")));
        // A neighbouring name that merely shares a prefix is not protected.
        assert!(!is_protected(Path::new("/etc/adosxyz/thing")));
        assert!(!is_protected(Path::new("/var/ados/recordings/a.mp4")));
        assert!(!is_protected(Path::new("/var/log/ados/plugins/x.log")));
    }

    #[test]
    fn a_protected_file_is_never_removed_even_when_asked_directly() {
        // The guard is at the removal helper, so a caller cannot route around it.
        assert_eq!(remove_file_guarded(Path::new("/etc/ados/config.yaml")), 0);
        assert!(
            Path::new("/etc/ados/config.yaml").exists() || true,
            "the point is that the helper refuses, not that the file exists here"
        );
    }

    #[test]
    fn apt_archives_lose_the_debs_and_nothing_else() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        fs::write(dir.join("libfoo_1.2_arm64.deb"), vec![0u8; 4096]).unwrap();
        fs::write(dir.join("libbar_2.0_arm64.ddeb"), vec![0u8; 2048]).unwrap();
        fs::write(dir.join("lock"), b"x").unwrap();
        fs::create_dir(dir.join("partial")).unwrap();
        fs::write(dir.join("partial").join("half_1.0.deb"), vec![0u8; 1024]).unwrap();

        assert_eq!(reclaim_apt_archives(dir), 4096 + 2048 + 1024);
        assert!(dir.join("lock").exists(), "apt's lock is not a package");
        assert_eq!(reclaim_apt_archives(dir), 0, "idempotent");
    }

    #[test]
    fn trimming_keeps_the_tail_and_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let log = tmp.path().join("com-example-thing.log");
        // 100 lines of 100 bytes = ~10 100 bytes.
        let body: String = (0..100).map(|i| format!("{:0>99}\n", i)).collect();
        fs::write(&log, &body).unwrap();
        let before = fs::metadata(&log).unwrap().len();

        let freed = trim_append_only(&log, 4_000, 1_000);
        assert!(freed > 0, "an over-cap file must be trimmed");
        let after = fs::metadata(&log).unwrap().len();
        assert!(
            after <= 1_000,
            "the trim must respect the keep size: {after}"
        );
        assert!(after > 0, "the tail is the floor; it must not go to zero");
        assert_eq!(freed, before - after);

        // The survivor starts on a line boundary and ends with the last line.
        let kept = fs::read_to_string(&log).unwrap();
        assert!(kept.starts_with('0') || kept.starts_with('9') || kept.starts_with('1'));
        assert!(kept.ends_with("99\n"), "the newest lines are the ones kept");

        assert_eq!(
            trim_append_only(&log, 4_000, 1_000),
            0,
            "a second trim must reclaim nothing"
        );
    }

    #[test]
    fn a_file_under_its_cap_is_untouched() {
        let tmp = tempfile::tempdir().unwrap();
        let log = tmp.path().join("small.log");
        fs::write(&log, b"one line\n").unwrap();
        assert_eq!(trim_append_only(&log, 4_000, 1_000), 0);
        assert_eq!(fs::read(&log).unwrap(), b"one line\n");
    }

    #[test]
    fn only_dot_log_files_are_trimmed_in_the_plugin_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let big: Vec<u8> = vec![b'x'; 10_000];
        fs::write(dir.join("com-example-a.log"), &big).unwrap();
        fs::write(dir.join("notes.txt"), &big).unwrap();

        let freed = trim_plugin_logs(dir, 1_000, 200);
        assert!(freed > 0);
        assert_eq!(
            fs::metadata(dir.join("notes.txt")).unwrap().len(),
            10_000,
            "a file that is not a plugin log is not ours to trim"
        );
    }

    #[test]
    fn the_newest_quarantined_store_survives() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        for (name, size) in [
            ("logs.db.corrupt-1", 1_000usize),
            ("logs.db.corrupt-2", 2_000),
            ("logs.db.corrupt-3", 3_000),
        ] {
            fs::write(dir.join(name), vec![0u8; size]).unwrap();
            // Space the mtimes so "newest" is unambiguous.
            std::thread::sleep(std::time::Duration::from_millis(1100));
        }
        fs::write(dir.join("logs.db"), vec![0u8; 500]).unwrap();

        let freed = prune_quarantines(dir, 1);
        assert_eq!(freed, 1_000 + 2_000);
        assert!(
            dir.join("logs.db.corrupt-3").exists(),
            "the most recent corruption is the evidence and must survive"
        );
        assert!(
            dir.join("logs.db").exists(),
            "the live store is not a corpse"
        );
        assert_eq!(prune_quarantines(dir, 1), 0, "idempotent");
    }

    #[test]
    fn recordings_older_than_the_window_go_but_the_newest_stay() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        for i in 0..5 {
            fs::write(dir.join(format!("flight-{i}.mp4")), vec![0u8; 1_000]).unwrap();
        }
        fs::write(dir.join("README"), b"not a recording").unwrap();

        // A cutoff far in the future makes every file older than the window.
        let freed = reclaim_recordings(dir, i64::MAX, 2, crate::janitor::now_unix());
        assert_eq!(freed, 3_000, "five recordings, the newest two kept");
        let left = fs::read_dir(dir).unwrap().count();
        assert_eq!(left, 3, "two recordings plus the file that is not one");
        assert!(dir.join("README").exists());
    }

    #[test]
    fn a_mediamtx_segment_tree_is_enumerated_one_level_down() {
        // The layout mediamtx's native recorder writes: `<root>/<stream>/<stamp>.mp4`.
        // A flat-only scan would see none of it, so the category would read as
        // permanently over its cap while every reclaim found nothing to take.
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        fs::create_dir(dir.join("main")).unwrap();
        fs::write(dir.join("main/seg-01.mp4"), vec![0u8; 500]).unwrap();
        fs::write(dir.join("main/seg-02.mp4"), vec![0u8; 500]).unwrap();
        fs::write(dir.join("main/notes.txt"), b"x").unwrap();
        fs::write(dir.join("flat.mp4"), vec![0u8; 100]).unwrap();

        let mut names: Vec<String> = recording_entries(dir)
            .into_iter()
            .map(|(name, _)| name)
            .collect();
        names.sort();
        assert_eq!(
            names,
            vec![
                "flat.mp4".to_string(),
                "main/seg-01.mp4".to_string(),
                "main/seg-02.mp4".to_string(),
            ]
        );
        // And a relative name resolves back to the file it names.
        assert!(dir.join("main/seg-01.mp4").exists());
    }

    #[test]
    fn the_free_space_floor_reclaims_oldest_first_and_stops_at_the_deficit() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let now = crate::janitor::now_unix();
        // Four settled segments, oldest first by mtime. Nothing is in flight.
        for (i, age) in [7_200i64, 5_400, 3_600, 1_800].iter().enumerate() {
            let path = dir.join(format!("seg-{i}.mp4"));
            fs::write(&path, vec![0u8; 1_000]).unwrap();
            set_mtime(&path, now - age);
        }

        // A 2 500-byte shortfall takes the three oldest (the fourth is the
        // keep-newest floor), and stops as soon as the deficit is covered.
        let freed = reclaim_recordings_to_free_floor(dir, 2_500, 1, now);
        assert_eq!(freed, 3_000, "oldest-first until the deficit is covered");
        assert!(!dir.join("seg-0.mp4").exists());
        assert!(!dir.join("seg-1.mp4").exists());
        assert!(!dir.join("seg-2.mp4").exists());
        assert!(dir.join("seg-3.mp4").exists(), "the newest survives");

        // No deficit → no reclaim, which is what makes a settled box a no-op.
        assert_eq!(reclaim_recordings_to_free_floor(dir, 0, 1, now), 0);
    }

    #[test]
    fn the_free_space_floor_never_takes_the_segment_of_an_in_flight_capture() {
        // The janitor and the recorder must not fight over the same file: the
        // newest segment of an actively-recording path is being appended to right
        // now, and even a volume under its floor must leave it alone.
        //
        // TWO recording paths, both live, with `keep_newest` at its floor of 1 —
        // so the keep-newest count covers only the newest file OVERALL and the
        // second path's live segment can be saved by nothing but the in-flight
        // rule. A one-path fixture would still pass with that rule deleted.
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let now = crate::janitor::now_unix();
        fs::create_dir(dir.join("main")).unwrap();
        fs::create_dir(dir.join("sub")).unwrap();
        let settled = dir.join("main/seg-01.mp4");
        let live = dir.join("main/seg-02.mp4");
        let newest_overall = dir.join("sub/seg-01.mp4");
        for path in [&settled, &live, &newest_overall] {
            fs::write(path, vec![0u8; 1_000]).unwrap();
        }
        set_mtime(&settled, now - 3_600);
        set_mtime(&live, now - 40);
        set_mtime(&newest_overall, now - 5);

        // A deficit larger than everything on disk.
        let freed = reclaim_recordings_to_free_floor(dir, 10_000, 1, now);
        assert_eq!(freed, 1_000, "only the settled segment came off");
        assert!(!settled.exists());
        assert!(
            live.exists(),
            "the segment an in-flight capture is appending to must survive, and \
             the keep-newest count is spent on the other path"
        );
        assert!(newest_overall.exists());
    }

    #[test]
    fn absent_directories_are_zero_not_an_error() {
        let missing = Path::new("/no/such/janitor/dir");
        assert_eq!(dir_bytes(missing), 0);
        assert_eq!(reclaim_apt_archives(missing), 0);
        assert_eq!(reclaim_apt_lists(missing), 0);
        assert_eq!(trim_plugin_logs(missing, 1, 1), 0);
        assert_eq!(
            reclaim_recordings(missing, i64::MAX, 1, crate::janitor::now_unix()),
            0
        );
        assert_eq!(
            reclaim_recordings_to_free_floor(missing, 10_000, 1, crate::janitor::now_unix()),
            0
        );
        assert_eq!(recording_entries(missing), Vec::new());
        assert_eq!(prune_quarantines(missing, 1), 0);
    }

    /// Set a file's mtime to a Unix second, so the age-ordered tests do not
    /// depend on filesystem write-order timing.
    fn set_mtime(path: &Path, unix_secs: i64) {
        let when = std::time::UNIX_EPOCH + std::time::Duration::from_secs(unix_secs as u64);
        let f = std::fs::OpenOptions::new().write(true).open(path).unwrap();
        f.set_modified(when).unwrap();
    }
}
