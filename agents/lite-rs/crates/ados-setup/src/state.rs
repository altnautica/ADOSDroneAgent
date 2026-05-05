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
    /// matching the Python implementation's leniency.
    pub fn load(&self) -> Result<PersistedState, StateError> {
        if !self.path.exists() {
            return Ok(PersistedState::default());
        }
        let raw = match std::fs::read_to_string(&self.path) {
            Ok(s) => s,
            Err(_) => return Ok(PersistedState::default()),
        };
        if raw.trim().is_empty() {
            return Ok(PersistedState::default());
        }
        let wire: WireState = match serde_json::from_str(&raw) {
            Ok(w) => w,
            Err(_) => return Ok(PersistedState::default()),
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

    /// Persist via the shared atomic-write helper. Compact JSON, sorted
    /// keys, trailing newline — byte-for-byte equivalent to Python's
    /// `json.dumps(obj, sort_keys=True)` followed by `fh.write("\n")`.
    pub fn save(&self, state: &PersistedState) -> Result<(), StateError> {
        let wire = WireState {
            setup_finalized: state.finalized,
            skipped_steps: state.skipped_steps.iter().cloned().collect(),
        };
        let value = serde_json::to_value(&wire)?;
        let mut out = serde_json::to_string(&sort_value(&value))?;
        out.push('\n');
        crate::atomic::atomic_write(&self.path, out.as_bytes(), 0o644)?;
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
        let mut state = PersistedState::default();
        state.finalized = true;
        state.skipped_steps.insert("hardware_check".into());
        state.skipped_steps.insert("video".into());
        store.save(&state).unwrap();
        let loaded = store.load().unwrap();
        assert_eq!(loaded, state);
    }

    #[test]
    fn save_emits_sorted_keys_and_trailing_newline() {
        let dir = tempfile::tempdir().unwrap();
        let store = StateStore::new(dir.path().join("state.json"));
        let mut state = PersistedState::default();
        state.finalized = true;
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
}
