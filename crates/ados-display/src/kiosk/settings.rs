//! The kiosk's slice of the agent config and the live settings watch.
//!
//! Readers work on the effective config document (schema defaults overlaid
//! with `config.yaml`) and are defensive: a missing or oddly-typed field means
//! "no opinion", never a crash, so the service starts against a config written
//! by an older or newer agent.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::Value;

/// How often the config file's signature is re-checked for an operator edit
/// to `ground_station.kiosk.enabled` / `resolution`.
pub const CONFIG_POLL: Duration = Duration::from_secs(3);

/// The default target: the agent-served cockpit. Trailing slash on purpose
/// (the path the static mount serves, so no redirect can drop the query);
/// `kiosk=1` tells the cockpit it is the panel's only surface, so it hides its
/// exit control.
pub const DEFAULT_URL: &str = "http://localhost:8080/cockpit/?kiosk=1";
/// Env override of the target URL (below the config value).
pub const ENV_URL_KEY: &str = "ADOS_KIOSK_URL";
/// Env override of the minimal layer (`0`/`false`/empty forces the full one).
pub const ENV_MINIMAL_KEY: &str = "ADOS_KIOSK_MINIMAL_LAYER";

/// The agent config path: `ADOS_CONFIG`, else the shipped location.
pub fn config_path() -> PathBuf {
    std::env::var_os("ADOS_CONFIG")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(crate::panel_conf::DEFAULT_CONFIG_PATH))
}

/// The effective config at `path`. An absent file is the schema defaults; an
/// unreadable or unparseable one is an error.
pub fn load_config(path: &Path) -> Result<Value, String> {
    ados_config::schema::effective(path)
}

/// `ground_station.kiosk.resolution` → the `WIDTHxHEIGHT` mode set on the
/// connected outputs under cage. Anything else (`auto`, unknown) leaves the
/// compositor's choice alone.
pub fn resolution_mode(resolution: &str) -> Option<&'static str> {
    match resolution.trim().to_lowercase().as_str() {
        "720p" => Some("1280x720"),
        "1080p" => Some("1920x1080"),
        _ => None,
    }
}

/// The operator's `ground_station.display.type`, or `auto` when unset or not
/// one of `auto` / `hdmi` / `lcd` / `none`.
pub fn display_selection(config: &Value) -> String {
    let value = config
        .pointer("/ground_station/display/type")
        .and_then(Value::as_str)
        .map(|s| s.trim().to_lowercase());
    match value.as_deref() {
        Some(sel @ ("auto" | "hdmi" | "lcd" | "none")) => sel.to_string(),
        _ => "auto".to_string(),
    }
}

/// `(target_url, minimal_layer)` from `ground_station.kiosk`. A blank URL reads
/// as unset; either may be `None`.
pub fn kiosk_config(config: &Value) -> (Option<String>, Option<bool>) {
    let Some(kiosk) = config.pointer("/ground_station/kiosk") else {
        return (None, None);
    };
    let url = kiosk
        .get("target_url")
        .and_then(Value::as_str)
        .filter(|u| !u.trim().is_empty())
        .map(str::to_string);
    let minimal = kiosk.get("minimal_layer").and_then(Value::as_bool);
    (url, minimal)
}

/// The kiosk config fields that change what the service runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KioskSettings {
    pub enabled: bool,
    /// The `WIDTHxHEIGHT` mode for the configured resolution, or `None` to
    /// leave the compositor's choice alone.
    pub output_mode: Option<&'static str>,
}

/// Read `ground_station.kiosk.enabled` / `resolution`. A missing section or a
/// non-bool `enabled` reads as disabled.
pub fn kiosk_settings(config: &Value) -> KioskSettings {
    let kiosk = config.pointer("/ground_station/kiosk");
    let enabled = kiosk
        .and_then(|k| k.get("enabled"))
        .and_then(Value::as_bool)
        == Some(true);
    let output_mode = kiosk
        .and_then(|k| k.get("resolution"))
        .and_then(Value::as_str)
        .and_then(resolution_mode);
    KioskSettings {
        enabled,
        output_mode,
    }
}

/// `(inode, mtime_ns)` of the config file, or `None` when it is absent. The
/// config writers replace the file atomically, so either half changing means a
/// new file landed.
pub fn config_signature(path: &Path) -> Option<(u64, i128)> {
    let meta = std::fs::metadata(path).ok()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Some((
            meta.ino(),
            i128::from(meta.mtime()) * 1_000_000_000 + i128::from(meta.mtime_nsec()),
        ))
    }
    #[cfg(not(unix))]
    {
        let mtime = meta
            .modified()
            .ok()?
            .duration_since(std::time::UNIX_EPOCH)
            .ok()?;
        Some((0, mtime.as_nanos() as i128))
    }
}

/// Notices an edit to the kiosk settings without a reboot. [`poll`] costs one
/// `stat` when nothing changed; it reloads the config only when the file's
/// signature moved, and reports `true` only when the kiosk settings differ
/// from the ones in force.
///
/// [`poll`]: SettingsWatch::poll
#[derive(Debug)]
pub struct SettingsWatch {
    pub settings: KioskSettings,
    path: PathBuf,
    signature: Option<(u64, i128)>,
    /// How often [`changed`](SettingsWatch::changed) re-checks.
    pub interval: Duration,
}

impl SettingsWatch {
    pub fn new(path: PathBuf, settings: KioskSettings, signature: Option<(u64, i128)>) -> Self {
        Self {
            settings,
            path,
            signature,
            interval: CONFIG_POLL,
        }
    }

    pub fn poll(&mut self) -> bool {
        let signature = config_signature(&self.path);
        if signature == self.signature {
            return false;
        }
        self.signature = signature;
        let config = match load_config(&self.path) {
            Ok(config) => config,
            Err(error) => {
                tracing::warn!(%error, "kiosk_config_reload_failed");
                return false;
            }
        };
        let settings = kiosk_settings(&config);
        if settings == self.settings {
            return false;
        }
        tracing::info!(
            enabled = settings.enabled,
            output_mode = ?settings.output_mode,
            previous_enabled = self.settings.enabled,
            previous_output_mode = ?self.settings.output_mode,
            "kiosk_settings_changed"
        );
        self.settings = settings;
        true
    }

    /// Resolve once the kiosk settings change (checked every
    /// [`interval`](Self::interval)).
    pub async fn changed(&mut self) {
        loop {
            tokio::time::sleep(self.interval).await;
            if self.poll() {
                return;
            }
        }
    }
}

/// The target URL (config, then `env_url`, then [`DEFAULT_URL`]) with
/// `layer=minimal` appended unless the full layer was asked for, and whether
/// it was.
///
/// Minimal is the default on the panel: the full layer's backdrop blur over
/// live video costs the compositor a re-read and re-blur per video frame (about
/// half the browser's CPU on a four-core board) for an effect nobody sees
/// behind a HUD. Only `minimal_layer: false` in config, or `env_minimal` of
/// `0` / `false` / empty, selects the full layer; a set `env_minimal` wins.
pub fn resolve_target_url(
    config: &Value,
    env_url: Option<&str>,
    env_minimal: Option<&str>,
) -> (String, bool) {
    let (cfg_url, cfg_minimal) = kiosk_config(config);
    let mut url = cfg_url
        .or_else(|| env_url.filter(|u| !u.is_empty()).map(str::to_string))
        .unwrap_or_else(|| DEFAULT_URL.to_string());
    let mut minimal = cfg_minimal != Some(false);
    if let Some(env) = env_minimal {
        minimal = !matches!(env.trim(), "0" | "false" | "False" | "");
    }
    if minimal {
        let sep = if url.contains('?') { '&' } else { '?' };
        url = format!("{url}{sep}layer=minimal");
    }
    (url, minimal)
}

/// [`resolve_target_url`] with the process environment.
pub fn resolve_target_url_from_env(config: &Value) -> (String, bool) {
    let env_url = std::env::var(ENV_URL_KEY).ok();
    let env_minimal = std::env::var(ENV_MINIMAL_KEY).ok();
    resolve_target_url(config, env_url.as_deref(), env_minimal.as_deref())
}

#[cfg(test)]
mod tests;
