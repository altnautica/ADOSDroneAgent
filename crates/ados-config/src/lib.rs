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

use std::fmt::Display;
use std::path::Path;

use serde::de::DeserializeOwned;

fn report<T, E>(parsed: Result<T, E>, what: &str) -> (T, Option<String>)
where
    T: Default,
    E: Display,
{
    match parsed {
        Ok(v) => (v, None),
        Err(e) => {
            let msg = e.to_string();
            tracing::error!(
                config = what,
                error = %msg,
                "config parse failed; using defaults until the config is valid"
            );
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
}
