//! Capture session storage: where capture sessions are written, the volume
//! under them, how much of it they may occupy, and whether a new session may
//! start.
//!
//! Everything here is pure or reads through injectable roots ([`SysRoots`]), so
//! the root selection, the volume listing and the admission rule are decided in
//! tests against a fake sysfs and mount table rather than on a node.
//!
//! The supervisor's disk janitor owns the capture category (eviction and the
//! `capture-storage.json` sidecar); the capture writer and the REST surface read
//! the same types from here so the three can never disagree about the shape or
//! the rule.

use std::path::Path;
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

mod root;
mod volumes;
pub use root::{read_sticky, resolve_capture_root, StickyRoot};
pub use volumes::{list_volumes, list_volumes_live, statvfs_space, volume_for, FsSpace, SysRoots};

/// One GiB.
pub const GIB: u64 = 1 << 30;

/// The error code a refused capture session carries.
pub const E_STORAGE_FULL: &str = "E_STORAGE_FULL";

/// Registry id of the sidecar the janitor writes.
pub const SIDECAR_ID: &str = "capture-storage";

/// Schema version of `capture-storage.json`. Kept in step with the registry.
pub const SIDECAR_VERSION: u16 = 1;

/// How often the supervisor rewrites `capture-storage.json`.
pub const SIDECAR_REFRESH: Duration = Duration::from_secs(10);

/// Freshness window of `capture-storage.json` (the registry's `max_age_ms`).
/// An older file says nothing about the volume now.
pub const SIDECAR_MAX_AGE: Duration = Duration::from_secs(30);

/// Where the `auto` choice of an NVMe capture root is persisted.
pub const STICKY_ROOT_FILE: &str = "/var/lib/ados/capture-root";

/// The free space an NVMe volume needs before `auto` will put captures on it.
pub const AUTO_MIN_FREE_BYTES: u64 = 64 * GIB;

/// The capture root used when `auto` finds no qualifying NVMe volume.
pub const FALLBACK_ROOT: &str = "/var/ados/capture";

/// Present inside a session directory once the session has been uploaded.
pub const UPLOADED_MARKER: &str = ".uploaded";

/// Present inside a session directory while it is being recorded.
pub const RECORDING_MARKER: &str = ".recording";

/// The scratch file the write-speed probe writes under the capture root.
pub const WRITE_PROBE_NAME: &str = ".write-probe";

/// Share of the budget at which the state turns `near_full`, in percent.
const NEAR_FULL_BUDGET_PCT: u128 = 90;

/// The `storage.capture_*` keys of the agent config.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CaptureStorageConfig {
    /// `auto`, or an absolute directory.
    pub capture_root: String,
    /// Share of the capture volume's size capture sessions may occupy.
    pub capture_budget_pct: u32,
    /// Free space below which new sessions are refused, in GiB.
    pub capture_floor_free_gib: u64,
    /// Whether sessions that were never uploaded may be evicted.
    pub capture_evict_unuploaded: bool,
}

impl Default for CaptureStorageConfig {
    fn default() -> Self {
        CaptureStorageConfig {
            capture_root: "auto".to_string(),
            capture_budget_pct: 85,
            capture_floor_free_gib: 5,
            capture_evict_unuploaded: false,
        }
    }
}

impl CaptureStorageConfig {
    /// The JSON pointer of the config slice these keys live in.
    pub const POINTER: &'static str = "/storage";

    /// Read the keys from `config_path`. Loud-fail: a malformed file is logged
    /// and returned as the error alongside the defaults.
    pub fn load(config_path: &Path) -> (Self, Option<String>) {
        match crate::schema::load_slice::<Self>(config_path, Self::POINTER) {
            Ok(cfg) => (cfg, None),
            Err(e) => (Self::default(), Some(e)),
        }
    }

    /// The free-space floor in bytes.
    pub fn floor_free_bytes(&self) -> u64 {
        self.capture_floor_free_gib.saturating_mul(GIB)
    }

    /// The budget in bytes for a volume of `total_bytes`.
    pub fn budget_bytes(&self, total_bytes: u64) -> u64 {
        let pct = u128::from(self.capture_budget_pct.min(100));
        (u128::from(total_bytes) * pct / 100) as u64
    }
}

/// A mounted block-device filesystem.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Volume {
    pub mount: String,
    /// Kernel block-device name (`nvme0n1p1`, `mmcblk0p2`).
    pub device: String,
    pub fs: String,
    pub total_bytes: u64,
    pub free_bytes: u64,
    pub rotational: bool,
}

/// Admission state of the capture volume.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CaptureState {
    Ok,
    NearFull,
    Full,
}

/// Capture usage against the budget and the floor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CaptureUsage {
    pub used_bytes: u64,
    pub budget_bytes: u64,
    pub budget_pct: u32,
    pub floor_free_bytes: u64,
    pub sessions: u64,
    pub uploaded: u64,
    pub evict_unuploaded: bool,
    pub state: CaptureState,
}

/// The last write-speed measurement of the capture root.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WriteProbe {
    /// Sustained write speed with fsync, in MB/s (10^6 bytes per second).
    pub mbps: f64,
    /// Host CLOCK_MONOTONIC ns when the measurement finished.
    pub measured_at_ns: u64,
}

/// `/run/ados/capture-storage.json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CaptureStorageSidecar {
    pub version: u16,
    /// Host CLOCK_MONOTONIC ns of the write.
    pub updated_ns: u64,
    pub capture_root: String,
    /// The volume holding the capture root; null when it could not be measured.
    pub volume: Option<Volume>,
    pub capture: CaptureUsage,
    pub write_probe: Option<WriteProbe>,
}

/// Why a new capture session was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StorageFull {
    pub free_bytes: u64,
    pub floor_free_bytes: u64,
    pub used_bytes: u64,
    pub budget_bytes: u64,
}

impl StorageFull {
    /// The wire error code.
    pub fn code(&self) -> &'static str {
        E_STORAGE_FULL
    }
}

impl std::fmt::Display for StorageFull {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{E_STORAGE_FULL}: {} bytes free (floor {}), {} bytes used (budget {})",
            self.free_bytes, self.floor_free_bytes, self.used_bytes, self.budget_bytes
        )
    }
}

impl std::error::Error for StorageFull {}

/// Whether a new capture session may start on `volume`.
///
/// Refused when free space is under the floor, or when capture already fills
/// its budget and nothing may be evicted to make room (no uploaded session, and
/// un-uploaded sessions are not evictable or there are none).
pub fn admit_new_session(usage: &CaptureUsage, volume: &Volume) -> Result<(), StorageFull> {
    let below_floor = volume.free_bytes < usage.floor_free_bytes;
    let over_budget = usage.budget_bytes > 0 && usage.used_bytes >= usage.budget_bytes;
    let evictable = usage.uploaded > 0 || (usage.evict_unuploaded && usage.sessions > 0);
    if below_floor || (over_budget && !evictable) {
        return Err(StorageFull {
            free_bytes: volume.free_bytes,
            floor_free_bytes: usage.floor_free_bytes,
            used_bytes: usage.used_bytes,
            budget_bytes: usage.budget_bytes,
        });
    }
    Ok(())
}

/// Why the sidecar refused a new session. Both map to `E_STORAGE_FULL`.
#[derive(Debug, Clone, PartialEq)]
pub enum Refused {
    /// The sidecar is missing, unparseable or older than [`SIDECAR_MAX_AGE`]:
    /// nobody has measured the volume recently, so there is no evidence it has
    /// room. Unknown is treated as full, never as ok.
    Unknown(&'static str),
    /// The supervisor's last measurement says `full`.
    Full,
}

impl Refused {
    pub fn code(&self) -> &'static str {
        E_STORAGE_FULL
    }
}

/// Admission for a capture writer, read from `capture-storage.json` at `path`.
///
/// Fresh and not `full` admits and hands back the sidecar (its `capture_root`
/// is where the session goes). A missing, malformed or stale sidecar refuses
/// with [`Refused::Unknown`]: a dead or wedged supervisor must not let a
/// session start on a volume nobody is watching.
pub fn admit_from_sidecar(path: &Path, now: SystemTime) -> Result<CaptureStorageSidecar, Refused> {
    use ados_protocol::sidecar::{read_fresh_at, Reading};
    match read_fresh_at::<CaptureStorageSidecar>(path, Some(SIDECAR_MAX_AGE), now) {
        Reading::Fresh(s) if s.capture.state == CaptureState::Full => Err(Refused::Full),
        Reading::Fresh(s) => Ok(s),
        Reading::Stale(_) => Err(Refused::Unknown("capture-storage.json is stale")),
        _ => Err(Refused::Unknown(
            "capture-storage.json is missing or unreadable",
        )),
    }
}

/// The admission state: `full` when [`admit_new_session`] refuses, `near_full`
/// when free space is under twice the floor or usage is at 90 % of the budget,
/// else `ok`. The `state` field of `usage` is not read.
pub fn capture_state(usage: &CaptureUsage, volume: &Volume) -> CaptureState {
    if admit_new_session(usage, volume).is_err() {
        return CaptureState::Full;
    }
    let low_free = volume.free_bytes < usage.floor_free_bytes.saturating_mul(2);
    let near_budget = usage.budget_bytes > 0
        && u128::from(usage.used_bytes) * 100
            >= u128::from(usage.budget_bytes) * NEAR_FULL_BUDGET_PCT;
    if low_free || near_budget {
        CaptureState::NearFull
    } else {
        CaptureState::Ok
    }
}

#[cfg(test)]
mod sticky_tests;
#[cfg(test)]
mod tests;
