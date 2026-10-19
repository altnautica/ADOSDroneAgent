//! The sensing surface: `/api/v1/clock`, `/api/v1/sensors` (+ the rig write),
//! `/api/v1/storage` and `/api/v1/gnss*`.
//!
//! Every read is served from the sidecar its owning service rewrites (clock,
//! sensors and gnss at 1 Hz, capture storage per janitor pass). A route whose
//! backing service is absent or stale answers 200 with `"present": false` and
//! empty values, never stale numbers and never a 404.

pub mod clock;
pub mod gnss;
pub mod sensors;
pub mod storage;

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use ados_protocol::sidecar::{read_fresh_at, Reading};
use serde_json::{Map, Value};

use crate::state::AppState;

/// Freshness window of the 1 Hz sensing sidecars (mirrors `contracts.toml`).
pub(crate) const FRESH: Duration = Duration::from_secs(5);

/// Where the sensing sidecars and the rig live; injectable for tests.
#[derive(Debug, Clone)]
pub(crate) struct SensingPaths {
    pub run_dir: PathBuf,
    /// The agent config directory (`/etc/ados`), home of `rig.json`.
    pub etc_dir: PathBuf,
}

impl SensingPaths {
    pub(crate) fn from_state(state: &AppState) -> Self {
        let etc_dir = state
            .pairing_paths
            .config
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("/etc/ados"));
        SensingPaths {
            run_dir: state.run_dir.clone(),
            etc_dir,
        }
    }

    pub(crate) fn sidecar(&self, file: &str) -> PathBuf {
        self.run_dir.join(file)
    }

    pub(crate) fn rig(&self) -> PathBuf {
        self.etc_dir.join("rig.json")
    }
}

/// A sidecar map when it parsed and was rewritten within `window` (`None` =
/// no window), with its `version` key removed.
pub(crate) fn fresh_map(
    path: &Path,
    window: Option<Duration>,
    now: SystemTime,
) -> Option<Map<String, Value>> {
    match read_fresh_at::<Map<String, Value>>(path, window, now) {
        Reading::Fresh(mut m) => {
            m.remove("version");
            Some(m)
        }
        _ => None,
    }
}

/// `{"present": present, ..rest}`.
pub(crate) fn with_present(present: bool, rest: Map<String, Value>) -> Value {
    let mut out = Map::new();
    out.insert("present".into(), Value::Bool(present));
    out.extend(rest);
    Value::Object(out)
}

/// The resolved agent profile (underscore form).
pub(crate) fn profile(state: &AppState) -> String {
    let configured = crate::config::PairingConfig::cached(&state.pairing_paths.config)
        .agent
        .profile
        .clone();
    crate::profile::current_profile_and_role(&configured).0
}

/// Write `bytes` to `path` atomically (temp file + rename in the same dir).
pub(crate) fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(
        ".{}.tmp",
        path.file_name().and_then(|n| n.to_str()).unwrap_or("rig")
    ));
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)
}
