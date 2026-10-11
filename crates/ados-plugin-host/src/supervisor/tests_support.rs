//! Path layouts for the crate's own tests.

use super::*;

/// The FHS defaults, independent of the test process environment.
pub(crate) fn fhs_paths() -> Paths {
    Paths {
        install_dir: PathBuf::from(PLUGINS_INSTALL_DIR),
        unit_dir: PathBuf::from(PLUGIN_UNIT_DIR),
        state_path: PathBuf::from(state::PLUGIN_STATE_PATH),
        log_dir: PathBuf::from(PLUGIN_LOG_DIR),
        control_dir: PathBuf::from(crate::control::DEFAULT_CONTROL_DIR),
        loopback_guard_state: PathBuf::from("/run/ados/plugin-loopback-guard.json"),
        socket_dir: PathBuf::from(crate::server::DEFAULT_SOCKET_DIR),
        token_secret: PathBuf::from(crate::token_secret::PLUGIN_TOKEN_SECRET_PATH),
        runner: PathBuf::from(PLUGIN_RUNNER_BINARY),
        run_dir: PathBuf::from(DEFAULT_RUN_DIR),
        data_root: PathBuf::from(PLUGIN_DATA_DIR),
        device_id_file: PathBuf::from(ados_protocol::identity::DEVICE_ID_FILE),
    }
}

/// Every path under `dir`.
pub(crate) fn paths_in(dir: &Path) -> Paths {
    Paths {
        install_dir: dir.join("plugins"),
        unit_dir: dir.join("units"),
        state_path: dir.join("state/plugin-state.json"),
        log_dir: dir.join("logs"),
        control_dir: dir.join("plugin-host"),
        loopback_guard_state: dir.join("plugin-loopback-guard.json"),
        socket_dir: dir.join("sockets"),
        token_secret: dir.join("secrets/plugin-token-secret"),
        runner: dir.join("bin/ados-plugin-runner"),
        run_dir: dir.join("run"),
        data_root: dir.join("plugin-data"),
        device_id_file: dir.join("device-id"),
    }
}
