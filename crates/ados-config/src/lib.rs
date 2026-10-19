//! Loud-fail config loading for the agent services.
//!
//! Every service reads a slice of the agent config (`/etc/ados/config.yaml`) plus
//! the odd JSON sidecar. A malformed file must never SILENTLY default a service to
//! disabled: that produces a status surface that lies — a genuinely-off service
//! looks identical to a mis-parsed one, so a real fault hides behind a
//! permanently-wrong baseline. These helpers replace the `from_str(...)
//! .unwrap_or_default()` pattern: they log the exact parser error (its message
//! names the offending field), optionally hand it back so the service can surface
//! a `config_error` on its heartbeat, and then fall back to defaults so a partial
//! or typo'd file degrades gracefully instead of crash-looping.
//!
//! `yaml_*` parse the YAML config slices (serde_norway); `json_*` parse JSON
//! sidecars (serde_json). The `load_*` variants read the file first: a MISSING
//! file is the normal fresh-node case (a quiet debug line, no surfaced error); a
//! PRESENT-but-malformed file is the loud one.
//!
//! `write_config_status` publishes a service's surfaced error (or its absence) to
//! a per-service status sidecar so a remote Health surface can show a malformed
//! config, not just the log.
//!
//! [`node_profile`] and [`resolve_profile`] resolve the node profile
//! (`drone` / `ground-station` / `workstation` / `compute` / `visor`) from `agent.profile`
//! and `profile.conf`, the one resolution every service shares.

pub mod log_store;
mod profile;
pub mod schema;
pub mod side_effects;
pub mod simulation;

pub use profile::{
    ground_station_role, mesh_role_path, node_profile, node_profile_at, normalize_profile,
    profile_conf_path, read_mesh_role, read_profile_conf_value, resolve_profile, MESH_ROLE_PATH,
    PROFILE_CONF,
};

use std::collections::HashMap;
use std::fmt::Display;
use std::path::Path;
use std::sync::LazyLock;

use parking_lot::Mutex;

use serde::de::DeserializeOwned;

/// The last error logged per config slice. A malformed file is parsed on every
/// request by the hot-path readers, so the error is logged once per distinct
/// `(slice, error)` and again only after it changes or the slice parses clean.
static REPORTED: LazyLock<Mutex<HashMap<String, String>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Log a parse failure for `what`, once per distinct error message.
fn report_error(what: &str, msg: &str) {
    let mut seen = REPORTED.lock();
    if seen.get(what).map(String::as_str) == Some(msg) {
        return;
    }
    seen.insert(what.to_string(), msg.to_string());
    drop(seen);
    tracing::error!(
        config = what,
        error = %msg,
        "config parse failed; using defaults until the config is valid"
    );
}

/// Forget a logged failure for `what` once it parses clean, so a recurrence is
/// logged again.
fn clear_reported(what: &str) {
    let mut seen = REPORTED.lock();
    if !seen.is_empty() {
        seen.remove(what);
    }
}

fn report<T, E>(parsed: Result<T, E>, what: &str) -> (T, Option<String>)
where
    T: Default,
    E: Display,
{
    match parsed {
        Ok(v) => {
            clear_reported(what);
            (v, None)
        }
        Err(e) => {
            let msg = e.to_string();
            report_error(what, &msg);
            (T::default(), Some(msg))
        }
    }
}

/// Read a config file, logging the missing (a quiet debug) and unreadable (a
/// warn) cases, and returning the text only when present + readable. A missing
/// file is the normal fresh-node case, so it is never surfaced as an error.
fn read_config_text(path: &Path, what: &str) -> Option<String> {
    match std::fs::read_to_string(path) {
        Ok(text) => Some(text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            tracing::debug!(config = what, path = %path.display(), "config file absent; using defaults");
            None
        }
        Err(e) => {
            tracing::warn!(
                config = what,
                path = %path.display(),
                error = %e,
                "config file unreadable; using defaults"
            );
            None
        }
    }
}

/// Parse a YAML config slice, returning `T::default()` and logging a loud error
/// (with the exact serde message, which names the offending field) on failure.
/// `what` names the slice for the log line (e.g. `"video"`, `"radio"`).
pub fn yaml_or_default<T: DeserializeOwned + Default>(text: &str, what: &str) -> T {
    report(serde_norway::from_str::<T>(text), what).0
}

/// Like [`yaml_or_default`] but also returns the error message so a service can
/// surface it as a `config_error` on its status/heartbeat. `None` on success.
pub fn yaml_reporting<T: DeserializeOwned + Default>(
    text: &str,
    what: &str,
) -> (T, Option<String>) {
    report(serde_norway::from_str::<T>(text), what)
}

/// Read + parse a YAML config file, `T::default()` on any failure (see the module
/// docs for the missing-vs-malformed distinction).
pub fn load_yaml_or_default<T: DeserializeOwned + Default>(path: &Path, what: &str) -> T {
    load_yaml_reporting(path, what).0
}

/// Like [`load_yaml_or_default`] but also returns the parse-error message. A
/// missing/unreadable file reports `None` (a fresh node is not a fault); only a
/// present-but-malformed file surfaces the error.
pub fn load_yaml_reporting<T: DeserializeOwned + Default>(
    path: &Path,
    what: &str,
) -> (T, Option<String>) {
    match read_config_text(path, what) {
        Some(text) => yaml_reporting(&text, what),
        None => (T::default(), None),
    }
}

/// Parse a JSON sidecar, returning `T::default()` and logging a loud error on
/// failure (the sidecar sibling of [`yaml_or_default`]).
pub fn json_or_default<T: DeserializeOwned + Default>(text: &str, what: &str) -> T {
    report(serde_json::from_str::<T>(text), what).0
}

/// Read + parse a JSON sidecar file, `T::default()` on any failure.
pub fn load_json_or_default<T: DeserializeOwned + Default>(path: &Path, what: &str) -> T {
    match read_config_text(path, what) {
        Some(text) => report(serde_json::from_str::<T>(&text), what).0,
        None => T::default(),
    }
}

/// The schema version this build writes for the config-status sidecar. The
/// reader (the cloud heartbeat glob) checks it against this value. Must match the
/// `config-status` entry in the shared contract registry.
pub const CONFIG_STATUS_SIDECAR_VERSION: u16 = 1;

/// Publish this service's config-status sidecar so a remote surface (the fleet
/// Health view) can show a malformed-config fault instead of it hiding behind a
/// silently-defaulted service. Writes `<run-dir>/config-status-<service>.json`
/// (run dir per `ados_protocol::paths::run_dir`) with `{ "version", "service", "error",
/// "generated_at_ms" }`, where `error` is the exact parser message (from the
/// `*_reporting` helpers) or `null` when the config is valid, and
/// `generated_at_ms` is the epoch-ms write time.
///
/// The write is atomic (`ados_protocol::sidecar::write_atomic`) and TOTALLY best-effort: any IO or
/// serialize failure is logged at most and swallowed, so a read-only or missing
/// run dir never panics and never blocks service startup.
pub fn write_config_status(service: &str, error: Option<&str>) {
    if let Err(e) = try_write_config_status(service, error) {
        tracing::warn!(
            service = service,
            error = %e,
            "could not publish config-status sidecar; continuing"
        );
    }
}

fn try_write_config_status(service: &str, error: Option<&str>) -> std::io::Result<()> {
    write_config_status_in(&ados_protocol::paths::run_dir(), service, error)
}

/// The run-dir-injectable core of [`write_config_status`].
fn write_config_status_in(dir: &Path, service: &str, error: Option<&str>) -> std::io::Result<()> {
    let generated_at_ms: i64 = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);

    let body = serde_json::json!({
        "version": CONFIG_STATUS_SIDECAR_VERSION,
        "service": service,
        "error": error,
        "generated_at_ms": generated_at_ms,
    });
    let final_path = dir.join(format!("config-status-{service}.json"));
    ados_protocol::sidecar::write_json_atomic(&final_path, &body, 0o644)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Debug, Default, PartialEq, Deserialize)]
    struct Sample {
        #[serde(default)]
        enabled: bool,
        #[serde(default)]
        count: u32,
    }

    #[test]
    fn yaml_parses_a_valid_slice() {
        let v: Sample = yaml_or_default("enabled: true\ncount: 5\n", "test");
        assert_eq!(
            v,
            Sample {
                enabled: true,
                count: 5
            }
        );
    }

    #[test]
    fn yaml_reporting_defaults_and_surfaces_the_error_on_a_bad_type() {
        // `count` wants a number; a string is a parse error, not a silent default.
        let (v, err): (Sample, _) = yaml_reporting("count: not-a-number\n", "test");
        assert_eq!(v, Sample::default());
        assert!(err.is_some());
    }

    #[test]
    fn yaml_reporting_reports_no_error_on_success() {
        let (v, err): (Sample, _) = yaml_reporting("enabled: true\n", "test");
        assert!(v.enabled);
        assert!(err.is_none());
    }

    #[test]
    fn load_missing_file_is_a_quiet_default_no_error() {
        let (v, err): (Sample, _) =
            load_yaml_reporting(Path::new("/nonexistent/ados/config.yaml"), "test");
        assert_eq!(v, Sample::default());
        assert!(err.is_none(), "a missing file is not a fault");
    }

    #[test]
    fn load_malformed_present_file_is_default_with_a_surfaced_error() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.yaml");
        std::fs::write(&p, "count: [this, is, not, a, number]\n").unwrap();
        let (v, err): (Sample, _) = load_yaml_reporting(&p, "test");
        assert_eq!(v, Sample::default());
        assert!(
            err.is_some(),
            "a present-but-malformed file surfaces the error"
        );
    }

    #[test]
    fn json_parses_and_defaults_loudly() {
        let ok: Sample = json_or_default(r#"{"enabled":true,"count":9}"#, "test");
        assert_eq!(
            ok,
            Sample {
                enabled: true,
                count: 9
            }
        );
        let bad: Sample = json_or_default("{not json", "test");
        assert_eq!(bad, Sample::default());
    }

    #[test]
    fn write_config_status_publishes_the_sidecar_and_round_trips() {
        let dir = tempfile::tempdir().unwrap();

        let msg = "invalid type: string, expected u32 for field `count`";
        write_config_status_in(dir.path(), "video", Some(msg)).unwrap();

        let p = dir.path().join("config-status-video.json");
        assert!(p.exists(), "the sidecar file should exist");

        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        assert_eq!(
            v["version"].as_u64(),
            Some(CONFIG_STATUS_SIDECAR_VERSION as u64),
            "the writer stamps the sidecar schema version"
        );
        assert_eq!(v["service"], "video");
        assert_eq!(v["error"], msg);
        assert!(
            v["generated_at_ms"].is_i64() || v["generated_at_ms"].is_u64(),
            "generated_at_ms is an epoch-ms integer"
        );

        // The healthy (valid-config) case writes an explicit null error.
        write_config_status_in(dir.path(), "video", None).unwrap();
        let v2: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        assert_eq!(v2["service"], "video");
        assert!(
            v2["error"].is_null(),
            "a valid config publishes a null error"
        );
    }

    /// An `io::Write` sink the test subscriber writes formatted events into.
    #[derive(Clone, Default)]
    struct Captured(std::sync::Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for Captured {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn report_dedupes() {
        let sink = Captured::default();
        let writer = sink.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .with_writer(move || writer.clone())
            .finish();
        let lines = || {
            String::from_utf8(sink.0.lock().clone())
                .unwrap()
                .matches("config parse failed")
                .count()
        };
        tracing::subscriber::with_default(subscriber, || {
            for _ in 0..100 {
                let _: Sample = yaml_or_default("count: not-a-number\n", "dedupe-probe");
            }
            assert_eq!(lines(), 1, "one line for one repeated error");
            let _: Sample = yaml_or_default("count: [1]\n", "dedupe-probe");
            assert_eq!(lines(), 2, "a different error is logged");
            let _: Sample = yaml_or_default("count: 3\n", "dedupe-probe");
            let _: Sample = yaml_or_default("count: [1]\n", "dedupe-probe");
            assert_eq!(lines(), 3, "a recurrence after a clean parse is logged");
        });
    }
}
