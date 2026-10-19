//! The capture category: capture sessions under the capture root, held to a
//! share of their volume and to a free-space floor.
//!
//! A session is a directory `<root>/<session_id>/`. A `.uploaded` marker inside
//! makes it evictable; a `.recording` marker marks the session being written,
//! which is never evicted. Over the budget or under the floor, uploaded sessions
//! go first, oldest first; sessions that were never uploaded go only when
//! `storage.capture_evict_unuploaded` is on, again oldest first.
//!
//! Every pass mirrors the result into `capture-storage.json`, the document a
//! new capture session is admitted against. The write probe measures the
//! capture root's sustained write speed once shortly after start and every six
//! hours after that, on a blocking thread.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, UNIX_EPOCH};

use ados_config::capture_storage::{
    self as cs, CaptureState, CaptureStorageConfig, CaptureStorageSidecar, CaptureUsage, SysRoots,
    Volume, WriteProbe,
};

use super::reclaim;

/// Bytes the write probe writes.
pub const PROBE_BYTES: u64 = 64 * 1024 * 1024;
/// Delay after supervisor start before the first probe.
pub const PROBE_FIRST_DELAY: Duration = Duration::from_secs(60);
/// Interval between probes.
pub const PROBE_INTERVAL: Duration = Duration::from_secs(6 * 3600);

/// The profiles that record capture sessions.
pub fn active_for(profile: &str) -> bool {
    matches!(profile, "drone" | "visor")
}

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

/// Every session under `root`, oldest first. Dot-named entries (the write
/// probe, scratch) and anything that is not a directory are not sessions.
pub fn scan_sessions(root: &Path) -> Vec<Session> {
    let Ok(dir) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut out: Vec<Session> = dir
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') || !e.file_type().ok()?.is_dir() {
                return None;
            }
            let path = e.path();
            let born = e
                .metadata()
                .ok()
                .and_then(|m| m.created().or_else(|_| m.modified()).ok())
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            Some(Session {
                bytes: reclaim::tree_bytes(&path),
                uploaded: path.join(cs::UPLOADED_MARKER).exists(),
                recording: path.join(cs::RECORDING_MARKER).exists(),
                name,
                born,
            })
        })
        .collect();
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

/// Remove one session directory, returning the bytes freed. A session that
/// started recording since the scan, or sits under a protected root, stays.
fn remove_session(root: &Path, session: &Session) -> u64 {
    let path = root.join(&session.name);
    if reclaim::is_protected(&path) || path.join(cs::RECORDING_MARKER).exists() {
        return 0;
    }
    match std::fs::remove_dir_all(&path) {
        Ok(()) => session.bytes,
        Err(e) => {
            tracing::debug!(path = %path.display(), error = %e, "janitor could not remove capture session");
            session.bytes.saturating_sub(reclaim::tree_bytes(&path))
        }
    }
}

fn current_volume(root: &Path, sys: &SysRoots) -> Option<Volume> {
    cs::volume_for(root, &cs::list_volumes(sys)).cloned()
}

/// The outcome of one capture pass.
pub struct CapturePass {
    pub freed: u64,
    pub root: PathBuf,
    pub sidecar: CaptureStorageSidecar,
}

/// One capture pass: resolve the root, evict when `evict` and over the budget
/// or under the floor, and build the sidecar from what is left.
pub fn run_pass(
    cfg: &CaptureStorageConfig,
    sys: &SysRoots,
    evict: bool,
    write_probe: Option<WriteProbe>,
    now_ns: u64,
) -> CapturePass {
    let root = cs::resolve_capture_root(cfg, sys);
    let mut sessions = scan_sessions(&root);
    let mut volume = current_volume(&root, sys);
    let mut freed = 0u64;
    if evict {
        let used = sessions.iter().fold(0u64, |a, s| a.saturating_add(s.bytes));
        let budget = volume
            .as_ref()
            .map_or(0, |v| cfg.budget_bytes(v.total_bytes));
        let need = excess_bytes(
            used,
            budget,
            volume.as_ref().map(|v| v.free_bytes),
            cfg.floor_free_bytes(),
        );
        let picked = plan_evictions(&sessions, need, cfg.capture_evict_unuploaded);
        if !picked.is_empty() {
            for i in picked {
                freed = freed.saturating_add(remove_session(&root, &sessions[i]));
            }
            sessions = scan_sessions(&root);
            volume = current_volume(&root, sys);
        }
    }
    let sidecar = build_sidecar(cfg, &root, volume, &sessions, write_probe, now_ns);
    CapturePass {
        freed,
        root,
        sidecar,
    }
}

/// The sidecar for `sessions` on `volume`. A volume that could not be measured
/// cannot be shown to have room, so its state is `full`.
pub fn build_sidecar(
    cfg: &CaptureStorageConfig,
    root: &Path,
    volume: Option<Volume>,
    sessions: &[Session],
    write_probe: Option<WriteProbe>,
    now_ns: u64,
) -> CaptureStorageSidecar {
    let mut usage = CaptureUsage {
        used_bytes: sessions.iter().fold(0u64, |a, s| a.saturating_add(s.bytes)),
        budget_bytes: volume
            .as_ref()
            .map_or(0, |v| cfg.budget_bytes(v.total_bytes)),
        budget_pct: cfg.capture_budget_pct,
        floor_free_bytes: cfg.floor_free_bytes(),
        sessions: sessions.len() as u64,
        uploaded: sessions.iter().filter(|s| s.uploaded).count() as u64,
        evict_unuploaded: cfg.capture_evict_unuploaded,
        state: CaptureState::Full,
    };
    if let Some(v) = &volume {
        usage.state = cs::capture_state(&usage, v);
    }
    CaptureStorageSidecar {
        version: cs::SIDECAR_VERSION,
        updated_ns: now_ns,
        capture_root: root.display().to_string(),
        volume,
        capture: usage,
        write_probe,
    }
}

/// Write the sidecar atomically.
pub fn write_sidecar(path: &Path, sidecar: &CaptureStorageSidecar) -> std::io::Result<()> {
    ados_protocol::sidecar::write_json_atomic(path, sidecar, 0o644)
}

/// Write `bytes` to `<root>/.write-probe` with fsync, time it, delete it, and
/// return MB/s (10^6 bytes per second). `None` when the root is absent or not
/// writable.
pub fn measure_write(root: &Path, bytes: u64) -> Option<f64> {
    if !root.is_dir() {
        return None;
    }
    let path = root.join(cs::WRITE_PROBE_NAME);
    let result = write_probe_file(&path, bytes);
    let _ = std::fs::remove_file(&path);
    match result {
        Ok(mbps) => Some(mbps),
        Err(e) => {
            tracing::debug!(path = %path.display(), error = %e, "capture write probe failed");
            None
        }
    }
}

fn write_probe_file(path: &Path, bytes: u64) -> std::io::Result<f64> {
    use std::io::Write;
    const CHUNK: usize = 4 * 1024 * 1024;
    // Incompressible content, so a compressing filesystem cannot flatter the
    // figure.
    let mut buf = vec![0u8; CHUNK];
    let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
    for word in buf.chunks_mut(8) {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        word.copy_from_slice(&x.to_le_bytes()[..word.len()]);
    }
    let start = Instant::now();
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)?;
    let mut left = bytes;
    while left > 0 {
        let n = left.min(CHUNK as u64) as usize;
        file.write_all(&buf[..n])?;
        left -= n as u64;
    }
    file.sync_all()?;
    let secs = start.elapsed().as_secs_f64().max(1e-9);
    Ok(bytes as f64 / 1e6 / secs)
}

/// Whether a probe is due `since_start` after supervisor start, `since_last`
/// after the previous one started.
pub fn probe_due(since_start: Duration, since_last: Option<Duration>) -> bool {
    since_start >= PROBE_FIRST_DELAY && since_last.is_none_or(|d| d >= PROBE_INTERVAL)
}

/// Host CLOCK_MONOTONIC ns.
#[cfg(target_os = "linux")]
pub fn mono_ns() -> u64 {
    use nix::time::{clock_gettime, ClockId};
    clock_gettime(ClockId::CLOCK_MONOTONIC)
        .map(|t| (t.tv_sec() as u64).saturating_mul(1_000_000_000) + t.tv_nsec() as u64)
        .unwrap_or(0)
}

#[cfg(not(target_os = "linux"))]
pub fn mono_ns() -> u64 {
    0
}

mod runtime;
pub use runtime::CaptureRuntime;

#[cfg(test)]
mod tests;
