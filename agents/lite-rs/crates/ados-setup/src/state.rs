//! Persistent setup state at `/etc/ados/setup-state.yaml`.
//!
//! Simple atomic-write wrapper around two fields the wizard cares about:
//!
//! - `finalized` — whether the operator clicked "Finish" on the wizard
//! - `skipped_steps` — set of step ids the operator chose to defer
//!
//! Atomic write semantics: write to a sibling tempfile + rename. The
//! kernel guarantees an observer either sees the old file or the new
//! file, never a torn mix.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum StateError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("yaml error: {0}")]
    Yaml(#[from] serde_yaml::Error),
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct PersistedState {
    #[serde(default)]
    pub finalized: bool,
    #[serde(default)]
    pub skipped_steps: BTreeSet<String>,
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
    /// yet exist — first-boot is not an error.
    pub fn load(&self) -> Result<PersistedState, StateError> {
        if !self.path.exists() {
            return Ok(PersistedState::default());
        }
        let raw = std::fs::read_to_string(&self.path)?;
        if raw.trim().is_empty() {
            return Ok(PersistedState::default());
        }
        Ok(serde_yaml::from_str(&raw)?)
    }

    /// Persist the state via tempfile + rename.
    pub fn save(&self, state: &PersistedState) -> Result<(), StateError> {
        let parent = self
            .path
            .parent()
            .unwrap_or_else(|| Path::new("."));
        std::fs::create_dir_all(parent)?;
        let tmp = parent.join(format!(
            ".setup-state.yaml.{}.tmp",
            std::process::id()
        ));
        let serialized = serde_yaml::to_string(state)?;
        std::fs::write(&tmp, serialized)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o644)).ok();
        }
        std::fs::rename(&tmp, &self.path)?;
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

    pub fn reset(&self) -> Result<(), StateError> {
        self.save(&PersistedState::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_returns_default_when_file_missing() {
        let dir = tempfile::tempdir().unwrap();
        let store = StateStore::new(dir.path().join("setup-state.yaml"));
        let state = store.load().unwrap();
        assert!(!state.finalized);
        assert!(state.skipped_steps.is_empty());
    }

    #[test]
    fn save_then_load_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let store = StateStore::new(dir.path().join("setup-state.yaml"));
        let mut state = PersistedState::default();
        state.finalized = true;
        state.skipped_steps.insert("hardware_check".into());
        state.skipped_steps.insert("video".into());
        store.save(&state).unwrap();
        let loaded = store.load().unwrap();
        assert_eq!(loaded, state);
    }

    #[test]
    fn mark_finalized_persists() {
        let dir = tempfile::tempdir().unwrap();
        let store = StateStore::new(dir.path().join("setup-state.yaml"));
        store.mark_finalized().unwrap();
        let loaded = store.load().unwrap();
        assert!(loaded.finalized);
    }

    #[test]
    fn mark_skipped_accumulates() {
        let dir = tempfile::tempdir().unwrap();
        let store = StateStore::new(dir.path().join("setup-state.yaml"));
        store.mark_skipped("video").unwrap();
        store.mark_skipped("remote_access").unwrap();
        let loaded = store.load().unwrap();
        assert!(loaded.skipped_steps.contains("video"));
        assert!(loaded.skipped_steps.contains("remote_access"));
    }

    #[test]
    fn reset_clears_state() {
        let dir = tempfile::tempdir().unwrap();
        let store = StateStore::new(dir.path().join("setup-state.yaml"));
        store.mark_finalized().unwrap();
        store.mark_skipped("video").unwrap();
        store.reset().unwrap();
        let loaded = store.load().unwrap();
        assert!(!loaded.finalized);
        assert!(loaded.skipped_steps.is_empty());
    }
}
