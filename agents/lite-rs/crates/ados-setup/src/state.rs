//! Persistent setup state — Python-byte-for-byte compatible.
//!
//! Mirrors `src/ados/setup/state.py` from the Python full agent. Uses the
//! same path conventions (`/var/lib/ados/setup/state.json`), the same
//! schema (`{ "setup_finalized": bool, "skipped_steps": [sorted list] }`),
//! the same encoding (JSON with `sort_keys=True` + trailing newline), and
//! the same drop-unknown-step-ids filter on load.
//!
//! Because both halves write the same path with the same byte format, an
//! operator can swap between the Python full agent and the Rust lite
//! agent on the same board without losing setup state.

use std::collections::BTreeSet;
use std::io;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum StateError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
}

/// Step ids the wizard is allowed to persist as skipped. Anything else
/// in `skipped_steps` is dropped on load (mirrors the Python `_KNOWN_STEP_IDS`
/// filter) so a wizard that retired a step never has to cope with stale
/// entries written by a previous build.
pub const KNOWN_STEP_IDS: &[&str] = &[
    "welcome",
    "profile",
    "hardware_check",
    "cloud_choice",
    "pair",
    "mavlink",
    "video",
    "ground_receiver",
    "remote_access",
    "finish",
];

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PersistedState {
    pub finalized: bool,
    pub skipped_steps: BTreeSet<String>,
}

/// Wire format on disk. Field names match the Python agent
/// (`setup_finalized`, `skipped_steps`) so a byte-for-byte cross-load
/// works in both directions.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct WireState {
    #[serde(default)]
    setup_finalized: bool,
    #[serde(default)]
    skipped_steps: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct StateStore {
    path: PathBuf,
}

impl StateStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// Load the persisted state. Returns `Default` when the file does not
    /// yet exist (first boot is not an error). Corrupt files are also
    /// surfaced as defaults — the next write will overwrite cleanly,
    /// matching the Python implementation's leniency. The failure mode
    /// is no longer silent: permission and read errors emit a single
    /// `tracing::error!` with the path; corrupt JSON emits an error with
    /// a remediation hint so an operator tailing journalctl sees why the
    /// wizard suddenly walks them through a step they previously skipped.
    pub fn load(&self) -> Result<PersistedState, StateError> {
        if !self.path.exists() {
            return Ok(PersistedState::default());
        }
        let raw = match std::fs::read_to_string(&self.path) {
            Ok(s) => s,
            Err(e) => {
                if e.kind() == std::io::ErrorKind::NotFound {
                    return Ok(PersistedState::default());
                }
                tracing::error!(
                    path = %self.path.display(),
                    error = %e,
                    "setup state read failed; falling back to wizard defaults"
                );
                return Ok(PersistedState::default());
            }
        };
        if raw.trim().is_empty() {
            return Ok(PersistedState::default());
        }
        let wire: WireState = match serde_json::from_str(&raw) {
            Ok(w) => w,
            Err(e) => {
                tracing::error!(
                    path = %self.path.display(),
                    error = %e,
                    "setup state file is corrupt; falling back to wizard defaults — \
                     delete the file to let the wizard re-finalize cleanly"
                );
                return Ok(PersistedState::default());
            }
        };
        let known: BTreeSet<&str> = KNOWN_STEP_IDS.iter().copied().collect();
        let cleaned: BTreeSet<String> = wire
            .skipped_steps
            .into_iter()
            .filter(|s| known.contains(s.as_str()))
            .collect();
        Ok(PersistedState {
            finalized: wire.setup_finalized,
            skipped_steps: cleaned,
        })
    }

    /// Persist via the shared atomic-write helper. Sorted keys, Python-default
    /// separators (`, ` between items and `: ` between key/value), trailing
    /// newline — byte-for-byte equivalent to Python's `json.dump(obj, fh,
    /// sort_keys=True)` followed by `fh.write("\n")`. The full agent in
    /// `src/ados/setup/state.py` writes the same byte sequence, so a
    /// state.json round-trips between Python and Rust without producing
    /// a diff that would confuse anyone tail-reading the file.
    pub fn save(&self, state: &PersistedState) -> Result<(), StateError> {
        let wire = WireState {
            setup_finalized: state.finalized,
            skipped_steps: state.skipped_steps.iter().cloned().collect(),
        };
        let value = sort_value(&serde_json::to_value(&wire)?);
        let mut buf = Vec::new();
        let mut ser =
            serde_json::Serializer::with_formatter(&mut buf, PythonDefaultFormatter);
        Serialize::serialize(&value, &mut ser)?;
        buf.push(b'\n');
        crate::atomic::atomic_write(&self.path, &buf, 0o644)?;
        Ok(())
    }

    pub fn mark_finalized(&self) -> Result<(), StateError> {
        let mut state = self.load()?;
        state.finalized = true;
        self.save(&state)
    }

    pub fn mark_skipped(&self, step_id: &str) -> Result<(), StateError> {
        let mut state = self.load()?;
        state.skipped_steps.insert(step_id.to_string());
        self.save(&state)
    }

    pub fn clear_skipped(&self, step_id: &str) -> Result<(), StateError> {
        let mut state = self.load()?;
        state.skipped_steps.remove(step_id);
        self.save(&state)
    }

    pub fn reset(&self) -> Result<(), StateError> {
        self.save(&PersistedState::default())
    }
}

fn sort_value(value: &serde_json::Value) -> serde_json::Value {
    use serde_json::Value;
    match value {
        Value::Object(map) => {
            let mut sorted = serde_json::Map::new();
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            for key in keys {
                sorted.insert(key.clone(), sort_value(&map[key]));
            }
            Value::Object(sorted)
        }
        Value::Array(arr) => Value::Array(arr.iter().map(sort_value).collect()),
        _ => value.clone(),
    }
}

/// Custom serde_json formatter that emits Python's default `json.dump`
/// separators: `", "` between items and `": "` between a key and its
/// value. serde_json's default formatter uses `","` and `":"` (no
/// spaces); the standard library's `to_string_pretty` adds newlines and
/// indentation, which Python only produces when `indent=` is set. This
/// formatter sits in the gap so the on-disk bytes match what Python
/// writes by default with `json.dump(obj, fh, sort_keys=True)`.
///
/// Scope: this is intentionally narrow. Pairing-state writes
/// (`pairing.rs`) keep using `to_string_pretty`; only `state.json` needs
/// the byte-for-byte cross-compat with the Python full agent.
struct PythonDefaultFormatter;

impl serde_json::ser::Formatter for PythonDefaultFormatter {
    fn begin_array_value<W>(&mut self, writer: &mut W, first: bool) -> io::Result<()>
    where
        W: ?Sized + io::Write,
    {
        if first {
            Ok(())
        } else {
            writer.write_all(b", ")
        }
    }

    fn begin_object_key<W>(&mut self, writer: &mut W, first: bool) -> io::Result<()>
    where
        W: ?Sized + io::Write,
    {
        if first {
            Ok(())
        } else {
            writer.write_all(b", ")
        }
    }

    fn begin_object_value<W>(&mut self, writer: &mut W) -> io::Result<()>
    where
        W: ?Sized + io::Write,
    {
        writer.write_all(b": ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_returns_default_when_file_missing() {
        let dir = tempfile::tempdir().unwrap();
        let store = StateStore::new(dir.path().join("state.json"));
        let state = store.load().unwrap();
        assert!(!state.finalized);
        assert!(state.skipped_steps.is_empty());
    }

    #[test]
    fn save_then_load_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let store = StateStore::new(dir.path().join("state.json"));
        let mut state = PersistedState {
            finalized: true,
            ..Default::default()
        };
        state.skipped_steps.insert("hardware_check".into());
        state.skipped_steps.insert("video".into());
        store.save(&state).unwrap();
        let loaded = store.load().unwrap();
        assert_eq!(loaded, state);
    }

    #[test]
    fn save_emits_python_default_byte_exact_output() {
        // The doc comment on `save()` claims the on-disk bytes match
        // Python's `json.dump(obj, fh, sort_keys=True)` exactly. Pin
        // that down: any future serializer swap that introduces compact
        // separators (`,`/`:`) or pretty newlines would break this
        // assertion before any operator notices a state.json diff.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let store = StateStore::new(&path);
        let mut state = PersistedState {
            finalized: true,
            ..Default::default()
        };
        // BTreeSet in PersistedState already iterates in sorted order;
        // the wire-side `sort_value` re-sorts as a defense-in-depth
        // guard. Pick two known step ids that will clearly land in
        // alphabetical order on disk.
        state.skipped_steps.insert("welcome".into());
        state.skipped_steps.insert("hardware_check".into());
        store.save(&state).unwrap();
        let raw = std::fs::read(&path).unwrap();
        // Python: json.dump({"setup_finalized": True, "skipped_steps":
        //   ["hardware_check", "welcome"]}, fh, sort_keys=True) +
        //   fh.write("\n")
        let expected: &[u8] =
            b"{\"setup_finalized\": true, \"skipped_steps\": [\"hardware_check\", \"welcome\"]}\n";
        assert_eq!(
            raw, expected,
            "state.json bytes must match Python json.dump default separators verbatim — \
             got {:?}",
            String::from_utf8_lossy(&raw)
        );
    }

    #[test]
    fn save_emits_sorted_keys_and_trailing_newline() {
        let dir = tempfile::tempdir().unwrap();
        let store = StateStore::new(dir.path().join("state.json"));
        let mut state = PersistedState {
            finalized: true,
            ..Default::default()
        };
        state.skipped_steps.insert("video".into());
        state.skipped_steps.insert("hardware_check".into());
        store.save(&state).unwrap();
        let raw = std::fs::read_to_string(dir.path().join("state.json")).unwrap();
        // Python writes: {"setup_finalized": true, "skipped_steps": ["hardware_check", "video"]}\n
        // with sort_keys=True. setup_finalized comes before skipped_steps alphabetically.
        let setup_idx = raw.find("setup_finalized").expect("contains setup_finalized");
        let skipped_idx = raw.find("skipped_steps").expect("contains skipped_steps");
        assert!(setup_idx < skipped_idx, "keys are not alphabetically sorted: {raw}");
        // Inner array is sorted.
        let hw_idx = raw.find("hardware_check").expect("contains hardware_check");
        let video_idx = raw.find("video").expect("contains video");
        assert!(hw_idx < video_idx, "skipped_steps array is not sorted: {raw}");
        assert!(raw.ends_with('\n'), "missing trailing newline");
    }

    #[test]
    fn load_drops_unknown_step_ids() {
        // Python writes a `paranoia_step` value that we shouldn't preserve.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        std::fs::write(
            &path,
            r#"{"setup_finalized": false, "skipped_steps": ["video", "paranoia_step", "remote_access"]}"#,
        )
        .unwrap();
        let store = StateStore::new(&path);
        let loaded = store.load().unwrap();
        assert!(loaded.skipped_steps.contains("video"));
        assert!(loaded.skipped_steps.contains("remote_access"));
        assert!(!loaded.skipped_steps.contains("paranoia_step"));
    }

    #[test]
    fn cross_compat_with_python_written_state() {
        // Bytes a Python `mark_finalized()` + `mark_skipped("hardware_check")`
        // would write: sorted top-level keys, sorted array, trailing newline.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let python_bytes =
            br#"{"setup_finalized": true, "skipped_steps": ["hardware_check", "video"]}
"#;
        std::fs::write(&path, python_bytes).unwrap();
        let store = StateStore::new(&path);
        let loaded = store.load().unwrap();
        assert!(loaded.finalized);
        assert!(loaded.skipped_steps.contains("hardware_check"));
        assert!(loaded.skipped_steps.contains("video"));
    }

    #[test]
    fn mark_finalized_persists() {
        let dir = tempfile::tempdir().unwrap();
        let store = StateStore::new(dir.path().join("state.json"));
        store.mark_finalized().unwrap();
        let loaded = store.load().unwrap();
        assert!(loaded.finalized);
    }

    #[test]
    fn mark_skipped_accumulates() {
        let dir = tempfile::tempdir().unwrap();
        let store = StateStore::new(dir.path().join("state.json"));
        store.mark_skipped("video").unwrap();
        store.mark_skipped("remote_access").unwrap();
        let loaded = store.load().unwrap();
        assert!(loaded.skipped_steps.contains("video"));
        assert!(loaded.skipped_steps.contains("remote_access"));
    }

    #[test]
    fn clear_skipped_reverses_a_skip() {
        let dir = tempfile::tempdir().unwrap();
        let store = StateStore::new(dir.path().join("state.json"));
        store.mark_skipped("video").unwrap();
        store.clear_skipped("video").unwrap();
        let loaded = store.load().unwrap();
        assert!(!loaded.skipped_steps.contains("video"));
    }

    #[test]
    fn reset_clears_state() {
        let dir = tempfile::tempdir().unwrap();
        let store = StateStore::new(dir.path().join("state.json"));
        store.mark_finalized().unwrap();
        store.mark_skipped("video").unwrap();
        store.reset().unwrap();
        let loaded = store.load().unwrap();
        assert!(!loaded.finalized);
        assert!(loaded.skipped_steps.is_empty());
    }

    #[test]
    fn corrupt_file_resolves_to_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        std::fs::write(&path, "not valid json {{{").unwrap();
        let store = StateStore::new(&path);
        let loaded = store.load().unwrap();
        assert!(!loaded.finalized);
        assert!(loaded.skipped_steps.is_empty());
    }

    #[test]
    fn corrupt_file_load_does_not_panic_on_error_formatting() {
        // The post-K3 load path emits `tracing::error!` with the path and
        // the parse error. A panic in that formatting code would surface
        // here even though we don't subscribe to the tracing output. Use
        // a payload that is parseable as a JSON value but not as the
        // wire schema (string instead of object) to exercise the second
        // branch.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        std::fs::write(&path, r#""not the right shape""#).unwrap();
        let store = StateStore::new(&path);
        let loaded = store.load().unwrap();
        assert!(!loaded.finalized);
        assert!(loaded.skipped_steps.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn permission_denied_resolves_to_defaults_without_panicking() {
        // Sibling to the same-named test in pairing.rs. chmod 0 on the
        // file must result in `Ok(default)` from `load`, not a panic and
        // not a silently-swallowed error path. The error log is emitted
        // via tracing.
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        std::fs::write(
            &path,
            r#"{"setup_finalized": true, "skipped_steps": ["video"]}"#,
        )
        .unwrap();
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o000);
        std::fs::set_permissions(&path, perms).unwrap();
        let read_attempt = std::fs::read_to_string(&path);
        let denied = matches!(
            read_attempt.as_ref().err().map(|e| e.kind()),
            Some(std::io::ErrorKind::PermissionDenied)
        );
        let store = StateStore::new(&path);
        let loaded = store.load().unwrap();
        if denied {
            assert!(!loaded.finalized);
            assert!(loaded.skipped_steps.is_empty());
        }
        // Restore for tempdir cleanup.
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o644);
        std::fs::set_permissions(&path, perms).unwrap();
    }
}
