//! The sensing surface: `/api/v1/clock`, `/api/v1/sensors` (+ the rig write),
//! `/api/v1/storage` and `/api/v1/gnss*`.
//!
//! Every read is served from the sidecar its owning service rewrites (clock,
//! sensors and gnss at 1 Hz, capture storage every 10 s). A route whose
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

    /// A registered sidecar or socket (`clock`, `capture-storage`,
    /// `sensor-bus`) under this run dir, where the path registry places it.
    pub(crate) fn registered(&self, id: &'static str) -> PathBuf {
        self.run_dir.join(run_relative(id))
    }

    pub(crate) fn rig(&self) -> PathBuf {
        self.etc_dir.join("rig.json")
    }
}

/// Where the path registry places a sidecar or socket relative to the run
/// directory (`clock` → `sensing/clock.json`). An unregistered id is taken as
/// a file name directly under it, as `ados_protocol::paths::run_path` does.
pub(crate) fn run_relative(id: &'static str) -> &'static str {
    use ados_protocol::paths;
    paths::sidecar(id)
        .map(|s| s.path)
        .or_else(|| paths::socket(id).map(|s| s.path))
        .and_then(|p| p.strip_prefix(paths::RUN_DIR_DEFAULT))
        .map_or(id, |p| p.trim_start_matches('/'))
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

/// The resolved agent profile in the underscore form the unit table keys on
/// (`ground_station`), not the hyphen wire form the resolver returns.
pub(crate) fn profile(state: &AppState) -> String {
    let configured = crate::config::PairingConfig::cached(&state.pairing_paths.config)
        .agent
        .profile
        .clone();
    unit_profile(&crate::profile::current_profile_and_role(&configured).0)
}

/// The underscore form of a wire profile (`ground-station` → `ground_station`).
pub(crate) fn unit_profile(wire: &str) -> String {
    wire.replace('-', "_")
}

/// Write `bytes` to `path` atomically (temp file + rename in the same dir).
/// The temp name carries the pid and a per-process counter, so two writers
/// racing on the same target never share (and truncate) one temp file.
pub(crate) fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);

    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(
        ".{}.{}.{}.tmp",
        path.file_name().and_then(|n| n.to_str()).unwrap_or("rig"),
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    if let Err(e) = std::fs::write(&tmp, bytes).and_then(|()| std::fs::rename(&tmp, path)) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_wire_profile_becomes_the_unit_table_form() {
        assert_eq!(unit_profile("ground-station"), "ground_station");
        assert_eq!(unit_profile("drone"), "drone");
        // A ground station runs the FC link, so its sensors table carries `fc`.
        let gs = unit_profile("ground-station");
        assert!(sensors::unit_on("ados-mavlink", &gs));
        assert!(sensors::unit_on("ados-gnss", &gs));
    }

    #[test]
    fn registered_paths_follow_the_registry_under_the_run_dir() {
        assert_eq!(run_relative("clock"), "sensing/clock.json");
        assert_eq!(run_relative("sensor-bus"), "sensing/sensors.sock");
        assert_eq!(
            run_relative("capture-storage"),
            "capture-storage/capture-storage.json"
        );
        assert_eq!(run_relative("gnss"), "gnss.json");
        let p = SensingPaths {
            run_dir: PathBuf::from("/tmp/run"),
            etc_dir: PathBuf::from("/tmp/etc"),
        };
        assert_eq!(
            p.registered("clock"),
            PathBuf::from("/tmp/run/sensing/clock.json")
        );
    }

    #[test]
    fn concurrent_atomic_writes_never_share_a_temp_file() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("etc").join("rig.json");
        let a = vec![b'a'; 64 * 1024];
        let b = vec![b'b'; 64 * 1024];
        std::thread::scope(|s| {
            for body in [&a, &b] {
                let target = &target;
                s.spawn(move || {
                    for _ in 0..50 {
                        write_atomic(target, body).unwrap();
                    }
                });
            }
        });
        // The survivor is one writer's complete body, never a mix, and no
        // temp file is left behind.
        let got = std::fs::read(&target).unwrap();
        assert!(got == a || got == b);
        let left: Vec<_> = std::fs::read_dir(target.parent().unwrap())
            .unwrap()
            .flatten()
            .map(|e| e.file_name())
            .collect();
        assert_eq!(left, [std::ffi::OsString::from("rig.json")]);
    }
}
