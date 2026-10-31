//! The per-user `$HOME/.ados` layout.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};

use super::identity::set_mode;
use super::Paths;

impl Paths {
    /// Resolve the layout from `$HOME`. Errors when `$HOME` is unset (a rootless
    /// per-user install has nowhere to land otherwise).
    pub(super) fn resolve() -> Result<Self> {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .filter(|p| !p.as_os_str().is_empty())
            .ok_or_else(|| {
                anyhow!("$HOME is not set; the per-user install needs a home directory")
            })?;
        let ados_home = home.join(".ados");
        Ok(Paths {
            bin: ados_home.join("bin"),
            run: ados_home.join("run"),
            log: ados_home.join("log"),
            config: ados_home.join("config.yaml"),
            profile_conf: ados_home.join("profile.conf"),
            device_id_file: ados_home.join("device-id"),
            pairing: ados_home.join("pairing.json"),
            launch_agents: home.join("Library/LaunchAgents"),
            ados_home,
            home,
        })
    }

    /// The config/identity dir the native control surface reads via `ADOS_ETC_DIR`
    /// (Linux `/etc/ados`). Rooted at `$HOME/.ados` so the Rust installer's
    /// top-level `config.yaml` / `pairing.json` / `device-id` / `profile.conf`
    /// resolve to the same files the Python CLI reads, with the ground-station
    /// side-files + secrets landing beside them (writable, never root-owned).
    pub(super) fn etc_dir(&self) -> PathBuf {
        self.ados_home.clone()
    }

    /// The vision models dir (`ADOS_MODELS_DIR`, Linux `/opt/ados/models/vision`).
    pub(super) fn models_dir(&self) -> PathBuf {
        self.ados_home.join("models")
    }

    /// The recordings dir (`ADOS_RECORDINGS_DIR`, Linux `/var/ados/recordings`).
    pub(super) fn recordings_dir(&self) -> PathBuf {
        self.ados_home.join("recordings")
    }

    /// The WFB key dir (`ADOS_WFB_KEY_DIR`, Linux `/etc/ados/wfb`).
    pub(super) fn wfb_key_dir(&self) -> PathBuf {
        self.ados_home.join("wfb-keys")
    }

    /// The owner-only secrets dir (holds the setup token under `ADOS_SETUP_TOKEN_PATH`).
    fn secrets_dir(&self) -> PathBuf {
        self.ados_home.join("secrets")
    }

    /// The setup token path (`ADOS_SETUP_TOKEN_PATH`, Linux `/etc/ados/secrets/setup-token`).
    pub(super) fn setup_token(&self) -> PathBuf {
        self.secrets_dir().join("setup-token")
    }

    /// The logging store's parent dir (the DB lands at `logd/logs.db`).
    fn logd_dir(&self) -> PathBuf {
        self.ados_home.join("logd")
    }

    /// The install-result contract path (`ADOS_INSTALL_RESULT`, Linux
    /// `/var/lib/ados/install-result.json`). The control surface reads its version
    /// from here; `ados status` and the CLI read it for the recorded outcome.
    pub(super) fn install_result(&self) -> PathBuf {
        self.ados_home.join("install-result.json")
    }

    /// The plugin lifecycle layout (`ados_plugin_host::Paths`), every entry
    /// under the per-user home. The single source for both the `ADOS_PLUGIN_*`
    /// environment the daemons read ([`build_env`]) and the controller the
    /// installer's own World Engine step drives, so the two cannot disagree.
    /// Plugin LaunchAgents land beside the core ones in `~/Library/LaunchAgents`.
    pub(super) fn plugin_paths(&self, runner: &Path) -> ados_plugin_host::Paths {
        ados_plugin_host::Paths {
            install_dir: self.ados_home.join("plugins"),
            unit_dir: self.launch_agents.clone(),
            state_path: self.ados_home.join("plugin-state/plugin-state.json"),
            log_dir: self.log.join("plugins"),
            control_dir: self.run.join("plugin-host"),
            loopback_guard_state: self
                .run
                .join(ados_protocol::plugin_loopback_guard::SIDECAR_NAME),
            socket_dir: self.run.join("plugins"),
            token_secret: self.secrets_dir().join("plugin-token-secret"),
            runner: runner.to_path_buf(),
            run_dir: self.run.clone(),
            data_root: self.ados_home.join("plugin-data"),
            device_id_file: self.device_id_file.clone(),
        }
    }

    /// The per-plugin config store the plugin host reads and writes
    /// (`ADOS_PLUGIN_CONFIG_PATH`, Linux `/etc/ados/plugin-config.json`).
    pub(super) fn plugin_config(&self) -> PathBuf {
        self.ados_home.join("plugin-config.json")
    }

    /// Create the directories the install writes into.
    pub(super) fn ensure_dirs(&self) -> Result<()> {
        for dir in [
            &self.ados_home,
            &self.bin,
            &self.run,
            &self.log,
            &self.launch_agents,
            // The logging store's DB dir, created explicitly so a create failure
            // surfaces here rather than being swallowed by the daemon's own
            // best-effort `create_dir_all`.
            &self.logd_dir(),
            // The remaining home-rooted dirs the native control surface reads via
            // their ADOS_* overrides, so no route falls back to a root-owned Linux
            // path (`/opt/ados/models`, `/var/ados/recordings`, `/etc/ados/wfb`).
            &self.models_dir(),
            &self.recordings_dir(),
            &self.wfb_key_dir(),
            &self.secrets_dir(),
            // The plugin lifecycle tree (see `plugin_paths`): installs, state,
            // logs, the per-plugin sockets, the host's control dir, and each
            // HTTP-serving plugin's socket dir.
            &self.ados_home.join("plugins"),
            &self.ados_home.join("plugin-state"),
            &self.log.join("plugins"),
            &self.run.join("plugins"),
            &self.run.join("plugin-host"),
            &self.run.join("plugin-http"),
        ] {
            std::fs::create_dir_all(dir)
                .with_context(|| format!("create {} failed", dir.display()))?;
        }
        // The secrets dir holds owner-only material; lock it down.
        set_mode(&self.secrets_dir(), 0o700);
        Ok(())
    }
}
