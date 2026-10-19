//! The capture category: capture sessions under the capture root, held to a
//! share of their volume and to a free-space floor.
//!
//! A session is a directory `<root>/<session_id>/` holding `manifest.json`
//! (finished) or `.recording` (started, not yet finished); nothing else under
//! the root is counted or deleted. A `.recording` session written within the
//! last ten minutes is active and never evicted; one silent for longer crashed
//! and is treated as un-uploaded. Over the budget or under the floor, finished
//! uploaded sessions go first, oldest first; sessions that were never uploaded
//! (crashed ones included) go only when `storage.capture_evict_unuploaded` is
//! on, again oldest first.
//!
//! `capture-storage.json` is the document a new capture session is admitted
//! against. It is rewritten every [`cs::SIDECAR_REFRESH`] (measure only) so
//! its freshness proves the supervisor is watching the volume; eviction runs
//! on the janitor interval. The write probe measures the capture root's
//! sustained write speed once shortly after start and every six hours after
//! that, on a blocking thread.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use ados_config::capture_storage::{
    self as cs, CaptureState, CaptureStorageConfig, CaptureStorageSidecar, CaptureUsage, SysRoots,
    Volume, WriteProbe,
};

mod sessions;
pub use sessions::{
    excess_bytes, is_active, plan_evictions, remove_session, scan_sessions, Session, SizeCache,
    ACTIVE_WINDOW_S,
};

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

/// What one capture pass does beyond rewriting the sidecar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PassMode {
    /// The short-cadence rewrite: settled session sizes come from the cache.
    Refresh,
    /// The janitor-interval pass with the janitor disabled: every session is
    /// re-measured, nothing is deleted.
    Remeasure,
    /// The janitor-interval pass: re-measure, then evict to the budget/floor.
    Evict,
}

/// The two clocks a pass reads: host monotonic for the sidecar's
/// `updated_ns`, wall time (unix s) to compare against file mtimes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PassTime {
    pub mono_ns: u64,
    pub unix_s: i64,
}

impl PassTime {
    pub fn now() -> Self {
        PassTime {
            mono_ns: mono_ns(),
            unix_s: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0),
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
    /// Why the capture root could not be created, when it could not.
    pub root_error: Option<String>,
    pub sidecar: CaptureStorageSidecar,
}

/// One capture pass: resolve the root and create it (`mkdir -p`), evict when
/// `mode` is [`PassMode::Evict`] and over the budget or under the floor, and
/// build the sidecar from what is left. A root that cannot be created admits
/// nothing: its sidecar state is `full`. `keepalive` runs between sessions as
/// they are measured and removed, so a long pass keeps the monitor's progress
/// marker (and with it the systemd watchdog) fed.
pub fn run_pass(
    cfg: &CaptureStorageConfig,
    sys: &SysRoots,
    mode: PassMode,
    write_probe: Option<WriteProbe>,
    cache: &mut SizeCache,
    now: PassTime,
    keepalive: &dyn Fn(),
) -> CapturePass {
    let root = cs::resolve_capture_root(cfg, sys);
    let root_error = std::fs::create_dir_all(&root).err().map(|e| e.to_string());
    if mode != PassMode::Refresh {
        cache.clear();
    }
    let mut sessions = scan_sessions(&root, cache, now.unix_s, keepalive);
    let mut volume = current_volume(&root, sys);
    let mut freed = 0u64;
    if mode == PassMode::Evict {
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
                freed = freed.saturating_add(remove_session(
                    &root,
                    &sessions[i],
                    now.unix_s,
                    keepalive,
                ));
                keepalive();
            }
            sessions = scan_sessions(&root, cache, now.unix_s, keepalive);
            volume = current_volume(&root, sys);
        }
    }
    let mut sidecar = build_sidecar(cfg, &root, volume, &sessions, write_probe, now.mono_ns);
    if root_error.is_some() {
        sidecar.capture.state = CaptureState::Full;
    }
    CapturePass {
        freed,
        root,
        root_error,
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
