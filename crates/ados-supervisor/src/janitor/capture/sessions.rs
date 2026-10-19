//! Capture sessions on disk: enumeration with a size cache, the eviction plan,
//! and removal that reports progress as it goes.
//!
//! A session directory carries `.recording` from the moment it starts until
//! after it finishes, and `manifest.json` once it has finished. Either marker
//! makes it a session (counted toward usage); a directory with neither is not
//! the janitor's. A `.recording` session written to within [`ACTIVE_WINDOW_S`]
//! is active and never evicted; one silent for longer crashed mid-capture and
//! is treated as an un-uploaded session.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use ados_config::capture_storage as cs;

use super::super::reclaim;

/// How recently a `.recording` session must have been written to count as
/// still recording, in seconds.
pub const ACTIVE_WINDOW_S: i64 = 600;

/// How deep the newest-write walk descends into a session tree.
const MTIME_MAX_DEPTH: usize = 8;

/// One session directory under the capture root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    pub name: String,
    /// Creation time (modification time where the filesystem has none), unix s.
    pub born: i64,
    pub bytes: u64,
    pub uploaded: bool,
    /// `.recording` is present.
    pub recording: bool,
    /// `.recording` is present and the tree was written within
    /// [`ACTIVE_WINDOW_S`]: the session is being captured. Never evicted.
    pub active: bool,
}

/// Measured sizes of settled (not recording) sessions, keyed by path, so the
/// short-cadence refresh walks only the session being written. A re-measuring
/// pass clears it and measures everything afresh.
pub type SizeCache = HashMap<PathBuf, u64>;

fn unix_s(t: SystemTime) -> i64 {
    t.duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// The newest modification time (unix s) over `dir` and everything under it,
/// to a bounded depth, symlinks not followed.
pub fn newest_write(dir: &Path) -> i64 {
    fn walk(dir: &Path, depth: usize, newest: &mut i64) {
        if let Ok(m) = std::fs::symlink_metadata(dir).and_then(|m| m.modified()) {
            *newest = (*newest).max(unix_s(m));
        }
        if depth >= MTIME_MAX_DEPTH {
            return;
        }
        let Ok(rd) = std::fs::read_dir(dir) else {
            return;
        };
        for e in rd.flatten() {
            let Ok(ft) = e.file_type() else { continue };
            if ft.is_dir() {
                walk(&e.path(), depth + 1, newest);
            } else if let Ok(m) = e.metadata().and_then(|m| m.modified()) {
                *newest = (*newest).max(unix_s(m));
            }
        }
    }
    let mut newest = 0;
    walk(dir, 0, &mut newest);
    newest
}

/// Whether the session at `path` is being recorded at `now_unix`: it carries
/// `.recording` and was written within [`ACTIVE_WINDOW_S`].
pub fn is_active(path: &Path, now_unix: i64) -> bool {
    path.join(cs::RECORDING_MARKER).exists()
        && now_unix.saturating_sub(newest_write(path)) <= ACTIVE_WINDOW_S
}

/// Every session under `root`, oldest first: directories holding
/// `manifest.json` or `.recording`. Dot-named entries (the write probe,
/// scratch), plain files and directories with neither marker are left alone
/// and not counted. Sizes of settled sessions come from `cache` when present
/// and are stored in it; `.recording` sessions are always re-measured; entries
/// for sessions that are gone are dropped. `keepalive` runs after each session
/// is measured, so a long walk keeps reporting progress.
pub fn scan_sessions(
    root: &Path,
    cache: &mut SizeCache,
    now_unix: i64,
    keepalive: &dyn Fn(),
) -> Vec<Session> {
    let Ok(dir) = std::fs::read_dir(root) else {
        cache.clear();
        return Vec::new();
    };
    let mut seen: SizeCache = HashMap::new();
    let mut out: Vec<Session> = dir
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') || !e.file_type().ok()?.is_dir() {
                return None;
            }
            let path = e.path();
            let recording = path.join(cs::RECORDING_MARKER).exists();
            if !recording && !path.join(cs::MANIFEST_NAME).is_file() {
                return None;
            }
            let born = e
                .metadata()
                .ok()
                .and_then(|m| m.created().or_else(|_| m.modified()).ok())
                .map(unix_s)
                .unwrap_or(0);
            let bytes = match cache.get(&path) {
                Some(b) if !recording => *b,
                _ => reclaim::tree_bytes(&path),
            };
            if !recording {
                seen.insert(path.clone(), bytes);
            }
            let active = recording && is_active(&path, now_unix);
            keepalive();
            Some(Session {
                bytes,
                uploaded: path.join(cs::UPLOADED_MARKER).exists(),
                recording,
                active,
                name,
                born,
            })
        })
        .collect();
    *cache = seen;
    out.sort_by(|a, b| a.born.cmp(&b.born).then_with(|| a.name.cmp(&b.name)));
    out
}

/// Bytes that must come off the volume: the larger of the overrun of the
/// budget (`budget == 0` means unknown, never enforced) and the shortfall
/// under the floor (`free == None` means unmeasured, never enforced).
pub fn excess_bytes(used: u64, budget: u64, free: Option<u64>, floor: u64) -> u64 {
    let over = if budget > 0 {
        used.saturating_sub(budget)
    } else {
        0
    };
    let short = free.map_or(0, |f| floor.saturating_sub(f));
    over.max(short)
}

/// Indexes into `sessions` (oldest first) to evict to free `need` bytes:
/// finished uploaded sessions first, then, when `evict_unuploaded`, sessions
/// that were never uploaded, including crashed ones (`.recording`, silent past
/// the active window). An active session is never picked, and neither is a
/// crashed session carrying `.uploaded`.
pub fn plan_evictions(sessions: &[Session], need: u64, evict_unuploaded: bool) -> Vec<usize> {
    let uploaded = sessions
        .iter()
        .enumerate()
        .filter(|(_, s)| !s.recording && s.uploaded);
    let unuploaded = sessions
        .iter()
        .enumerate()
        .filter(|(_, s)| evict_unuploaded && !s.active && !s.uploaded);
    let mut picked = Vec::new();
    let mut freed = 0u64;
    for (i, s) in uploaded.chain(unuploaded) {
        if freed >= need {
            break;
        }
        picked.push(i);
        freed = freed.saturating_add(s.bytes);
    }
    picked
}

/// Entries removed between two `keepalive` calls inside one session tree.
const REMOVE_CHUNK: usize = 256;

/// Remove one session directory, returning the bytes freed. A session that is
/// active at `now_unix` (it may have resumed since the scan), or sits under a
/// protected root, stays. The tree is removed entry by entry with `keepalive`
/// every [`REMOVE_CHUNK`] entries, so a session of many thousand frames cannot
/// hold the monitor pass silent for the length of one `remove_dir_all`.
pub fn remove_session(root: &Path, session: &Session, now_unix: i64, keepalive: &dyn Fn()) -> u64 {
    let path = root.join(&session.name);
    if reclaim::is_protected(&path) || is_active(&path, now_unix) {
        return 0;
    }
    let mut count = 0usize;
    match remove_tree(&path, &mut count, keepalive) {
        Ok(()) => session.bytes,
        Err(e) => {
            tracing::debug!(path = %path.display(), error = %e, "janitor could not remove capture session");
            session.bytes.saturating_sub(reclaim::tree_bytes(&path))
        }
    }
}

/// `remove_dir_all` with a progress callback. Symlinks are removed, never
/// followed.
fn remove_tree(dir: &Path, count: &mut usize, keepalive: &dyn Fn()) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if entry.file_type()?.is_dir() {
            remove_tree(&path, count, keepalive)?;
        } else {
            std::fs::remove_file(&path)?;
        }
        *count += 1;
        if (*count).is_multiple_of(REMOVE_CHUNK) {
            keepalive();
        }
    }
    std::fs::remove_dir(dir)
}
