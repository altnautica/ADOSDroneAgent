//! Runtime file paths + the local reach constants for the config-tunnel
//! service. Runtime state lives in `/run/ados` (tmpfs); use the functions so
//! the `ADOS_RUN_DIR` env override is honoured.

/// Contract E sidecar written by this service: the channel's honest state and
/// received-side counters, ~1 Hz while running plus once on every terminal
/// state entry. Registered in `contracts.toml` as `tunnel-config`.
pub const TUNNEL_CONFIG_STATS_JSON: &str = "/run/ados/tunnel-config.json";

/// Schema version of the `tunnel-config.json` sidecar (its `v` field). Kept in
/// step with the registry in `contracts.toml`; bump both together.
pub const TUNNEL_CONFIG_SIDECAR_VERSION: u16 = 1;

/// The ground-side command socket the injector serves. `ados-control`'s
/// relayed-config route forwards one newline-JSON request here per the GS
/// data-plane command-socket idiom. Use `run_path("tunnel-config-cmd.sock")`
/// so the `ADOS_RUN_DIR` override is honoured.
pub const TUNNEL_CONFIG_CMD_SOCK: &str = "/run/ados/tunnel-config-cmd.sock";

/// The local config surface the drone-side terminator proxies to. It is the
/// native Rust front on `:8080`, which reverse-proxies `/api/config` to the
/// Python handler; an on-box loopback caller is trusted (no key). Fixed and
/// restricted: the terminator only ever calls this exact URL, never an
/// arbitrary reassembled path, so the channel can never become a general
/// command proxy over the radio.
pub const LOCAL_CONFIG_BASE_URL: &str = "http://127.0.0.1:8080";

/// Return the path to a run-dir file, honouring the `ADOS_RUN_DIR` override
/// (`ados_protocol::paths::run_dir`).
pub fn run_path(name: &str) -> String {
    ados_protocol::paths::run_dir()
        .join(name)
        .to_string_lossy()
        .into_owned()
}

/// Atomic JSON write (`ados_protocol::sidecar::write_json_atomic`), so a crash
/// mid-write never leaves a truncated sidecar file.
pub fn write_sidecar(path: &str, value: &serde_json::Value) -> std::io::Result<()> {
    ados_protocol::sidecar::write_json_atomic(std::path::Path::new(path), value, 0o644)
}

#[cfg(test)]
pub(crate) fn test_env_guard() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|p| p.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sidecar_version_matches_registry() {
        assert_eq!(
            TUNNEL_CONFIG_SIDECAR_VERSION,
            ados_protocol::contracts::sidecar_version("tunnel-config").unwrap()
        );
    }

    #[test]
    fn run_path_honours_the_env_override() {
        let _g = test_env_guard();
        std::env::set_var("ADOS_RUN_DIR", "/tmp/tunnel-cfg-test-run");
        assert_eq!(
            run_path("tunnel-config-cmd.sock"),
            "/tmp/tunnel-cfg-test-run/tunnel-config-cmd.sock"
        );
        std::env::remove_var("ADOS_RUN_DIR");
        assert_eq!(run_path("tunnel-config-cmd.sock"), TUNNEL_CONFIG_CMD_SOCK);
    }

    #[test]
    fn write_sidecar_is_atomic_and_readable() {
        let _g = test_env_guard();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tunnel-config.json");
        let path_str = path.to_str().unwrap();
        write_sidecar(path_str, &serde_json::json!({"v": 1})).unwrap();
        let body: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(body["v"], 1);
        assert!(!dir.path().join("tunnel-config.json.tmp").exists());
    }
}
