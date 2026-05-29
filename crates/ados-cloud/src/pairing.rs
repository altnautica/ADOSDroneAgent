//! Pairing-state reader.
//!
//! The cloud loops gate on whether the agent is paired and use the pairing API
//! key for `X-ADOS-Key` auth. The pairing state is owned by the API process and
//! persisted to `/etc/ados/pairing.json`; the cloud relay reads it (re-reading
//! per loop tick so a pair/unpair transition is observed). Mirrors the slice of
//! `ados.core.pairing.PairingManager` the loops use: `is_paired`, `api_key`.

use std::path::{Path, PathBuf};

use serde::Deserialize;

/// Canonical pairing-state path. Mirrors `PAIRING_JSON`.
pub const PAIRING_JSON: &str = "/etc/ados/pairing.json";

/// The pairing-state document. Only the fields the loops read are typed; every
/// other field is tolerated.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct PairingState {
    #[serde(default)]
    pub paired: bool,
    #[serde(default)]
    pub api_key: Option<String>,
    #[serde(default)]
    pub owner_id: Option<String>,
}

impl PairingState {
    /// Read the current pairing state from the canonical path (or the
    /// `ADOS_PAIRING_JSON` override). A missing or unparseable file is treated
    /// as unpaired, never an error — the relay must keep running.
    pub fn load() -> Self {
        let path = std::env::var("ADOS_PAIRING_JSON").unwrap_or_else(|_| PAIRING_JSON.to_string());
        Self::load_from(Path::new(&path))
    }

    /// Read from an explicit path (testable). Unpaired on absence / parse error.
    pub fn load_from(path: &Path) -> Self {
        match std::fs::read_to_string(path) {
            Ok(text) => serde_json::from_str(&text).unwrap_or_default(),
            Err(_) => PairingState::default(),
        }
    }

    /// Whether the agent is paired.
    pub fn is_paired(&self) -> bool {
        self.paired
    }

    /// The active API key, or `None` when unpaired (matching the Python
    /// `api_key` property, which gates the key on `paired`).
    pub fn api_key(&self) -> Option<&str> {
        if self.paired {
            self.api_key.as_deref()
        } else {
            None
        }
    }
}

/// The default pairing path as a `PathBuf`.
pub fn default_path() -> PathBuf {
    PathBuf::from(PAIRING_JSON)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn temp_json(name: &str, body: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("ados-pairing-{}-{}.json", std::process::id(), name));
        let mut f = std::fs::File::create(&p).unwrap();
        f.write_all(body.as_bytes()).unwrap();
        p
    }

    #[test]
    fn paired_state_exposes_api_key() {
        let path = temp_json(
            "paired",
            r#"{"paired": true, "api_key": "k-123", "owner_id": "u1"}"#,
        );
        let s = PairingState::load_from(&path);
        assert!(s.is_paired());
        assert_eq!(s.api_key(), Some("k-123"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn unpaired_state_hides_api_key() {
        let path = temp_json("unpaired", r#"{"paired": false, "api_key": "k-stale"}"#);
        let s = PairingState::load_from(&path);
        assert!(!s.is_paired());
        // The key is gated on paired, matching the Python property.
        assert_eq!(s.api_key(), None);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn missing_file_is_unpaired() {
        let s = PairingState::load_from(Path::new("/nonexistent/ados/pairing.json"));
        assert!(!s.is_paired());
        assert_eq!(s.api_key(), None);
    }
}
