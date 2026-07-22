//! Runtime file paths for the CRSF lane service. All runtime state lives in
//! `/run/ados` (tmpfs). Use the functions rather than the string literals
//! directly so the `ADOS_RUN_DIR` env override is honoured.

/// Contract E sidecar JSON written by this service: the lane state and link
/// statistics, ~1 Hz while running plus once on every degraded-state entry.
pub const CRSF_STATS_JSON: &str = "/run/ados/crsf-stats.json";

/// Schema version of the `crsf-stats.json` sidecar, surfaced as its `v`
/// field. Bump when the field set changes incompatibly; a reader compares it
/// best-effort via `ados_protocol::sidecar::check_sidecar_version` and reads
/// anyway on a mismatch. Kept in step with the registry in `contracts.toml`.
pub const CRSF_STATS_SIDECAR_VERSION: u16 = 1;

/// Command socket this service listens on for the status query and the
/// programmatic channel injection. One newline-JSON request → one
/// newline-JSON response per connection. Use `run_path("crsf-cmd.sock")` so
/// the `ADOS_RUN_DIR` env override is honoured.
pub const CRSF_CMD_SOCK: &str = "/run/ados/crsf-cmd.sock";

/// Return the run directory, honouring the `ADOS_RUN_DIR` env override.
pub fn run_dir() -> String {
    std::env::var("ADOS_RUN_DIR").unwrap_or_else(|_| "/run/ados".to_string())
}

/// Return the path to a run-dir file, honouring the env override.
pub fn run_path(name: &str) -> String {
    format!("{}/{}", run_dir(), name)
}

/// Atomic JSON write: write to `.tmp` then rename, so a crash mid-write never
/// leaves a truncated sidecar file.
pub fn write_sidecar(path: &str, value: &serde_json::Value) -> std::io::Result<()> {
    let tmp = format!("{}.tmp", path);
    let body = serde_json::to_vec(value)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    std::fs::write(&tmp, &body)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Serialize tests that mutate the `ADOS_RUN_DIR` process-global env var.
/// Shared across this crate's test modules; compiled only for tests.
#[cfg(test)]
pub(crate) fn test_env_guard() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|p| p.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_guard() -> std::sync::MutexGuard<'static, ()> {
        test_env_guard()
    }

    #[test]
    fn crsf_stats_sidecar_version_matches_registry() {
        // The per-file const and the sidecar registry are the two sources of
        // truth for this sidecar's schema version; a drift is caught here.
        assert_eq!(
            CRSF_STATS_SIDECAR_VERSION,
            ados_protocol::contracts::sidecar_version("crsf-stats").unwrap()
        );
    }

    #[test]
    fn run_path_honours_the_env_override() {
        let _g = env_guard();
        std::env::set_var("ADOS_RUN_DIR", "/tmp/crsf-test-run");
        assert_eq!(
            run_path("crsf-cmd.sock"),
            "/tmp/crsf-test-run/crsf-cmd.sock"
        );
        std::env::remove_var("ADOS_RUN_DIR");
        assert_eq!(run_path("crsf-cmd.sock"), CRSF_CMD_SOCK);
    }

    #[test]
    fn write_sidecar_is_atomic_and_readable() {
        let _g = env_guard();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("crsf-stats.json");
        let path_str = path.to_str().unwrap();
        write_sidecar(path_str, &serde_json::json!({"v": 1})).unwrap();
        let body: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(body["v"], 1);
        // No .tmp residue after the rename.
        assert!(!dir.path().join("crsf-stats.json.tmp").exists());
    }
}
