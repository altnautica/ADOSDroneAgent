//! Capture sessions on disk: enumeration with a size cache, the eviction plan,
//! and removal that reports progress as it goes.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use ados_config::capture_storage as cs;

use super::super::reclaim;

/// One session directory under the capture root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    pub name: String,
    /// Creation time (modification time where the filesystem has none), unix s.
    pub born: i64,
    pub bytes: u64,
    pub uploaded: bool,
    pub recording: bool,
}

/// Measured sizes of settled (not recording) sessions, keyed by path, so the
/// short-cadence refresh walks only the session being written. A re-measuring
/// pass clears it and measures everything afresh.
pub type SizeCache = HashMap<PathBuf, u64>;

/// Every session under `root`, oldest first. A session is a directory holding
/// `manifest.json`; dot-named entries (the write probe, scratch), plain files
/// and directories without a manifest are left alone and not counted. Sizes of
/// settled sessions come from `cache` when present and are stored in it;
/// entries for sessions that are gone are dropped. `keepalive` runs after each
/// session is measured, so a long walk keeps reporting progress.
pub fn scan_sessions(root: &Path, cache: &mut SizeCache, keepalive: &dyn Fn()) -> Vec<Session> {
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
            if !path.join(cs::MANIFEST_NAME).is_file() {
                return None;
            }
            let born = e
                .metadata()
                .ok()
                .and_then(|m| m.created().or_else(|_| m.modified()).ok())
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            let recording = path.join(cs::RECORDING_MARKER).exists();
            let bytes = match cache.get(&path) {
                Some(b) if !recording => *b,
                _ => reclaim::tree_bytes(&path),
            };
            if !recording {
                seen.insert(path.clone(), bytes);
            }
            keepalive();
            Some(Session {
                bytes,
                uploaded: path.join(cs::UPLOADED_MARKER).exists(),
                recording,
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
/// uploaded sessions first, then un-uploaded ones when `evict_unuploaded`.
/// A recording session is never picked.
pub fn plan_evictions(sessions: &[Session], need: u64, evict_unuploaded: bool) -> Vec<usize> {
    let uploaded = sessions
        .iter()
        .enumerate()
        .filter(|(_, s)| !s.recording && s.uploaded);
    let unuploaded = sessions
        .iter()
        .enumerate()
        .filter(|(_, s)| evict_unuploaded && !s.recording && !s.uploaded);
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

/// Remove one session directory, returning the bytes freed. A session that
/// started recording since the scan, or sits under a protected root, stays.
/// The tree is removed entry by entry with `keepalive` every
/// [`REMOVE_CHUNK`] entries, so a session of many thousand frames cannot hold
/// the monitor pass silent for the length of one `remove_dir_all`.
pub fn remove_session(root: &Path, session: &Session, keepalive: &dyn Fn()) -> u64 {
    let path = root.join(&session.name);
    if reclaim::is_protected(&path) || path.join(cs::RECORDING_MARKER).exists() {
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
