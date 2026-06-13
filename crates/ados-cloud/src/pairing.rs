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

/// The pairing code's lifetime in seconds, before an unpaired agent rolls it.
/// Mirrors the Python `CODE_TTL` (24 h). Used to derive the code-expiry epoch the
/// beacon advertises so the GCS can render a countdown clock.
pub const CODE_TTL_SECS: f64 = 24.0 * 60.0 * 60.0;

/// The pairing-state document. Only the fields the loops + beacon read are typed;
/// every other field is tolerated. Mirrors the slice of `pairing.json` the Python
/// `PairingManager` persists.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct PairingState {
    #[serde(default)]
    pub paired: bool,
    #[serde(default)]
    pub api_key: Option<String>,
    #[serde(default)]
    pub owner_id: Option<String>,
    /// The active (unpaired) pairing code the Python manager persists. The beacon
    /// registers this with the cloud; `None` once paired (the manager pops it on
    /// claim).
    #[serde(default)]
    pub pairing_code: Option<String>,
    /// The stable per-attempt API key the Python manager pre-seeds alongside the
    /// code, so the key the beacon registers is the same key `claim()` later
    /// persists as the paired `api_key` (no key drift, no permanent 401).
    #[serde(default)]
    pub pending_api_key: Option<String>,
    /// Epoch SECONDS the current code was created. `code_created_at + CODE_TTL` is
    /// the code's expiry, which the beacon advertises as epoch ms.
    #[serde(default)]
    pub code_created_at: Option<f64>,
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

    /// The active pairing code, or `None` once paired (the Python manager pops the
    /// code on claim). The beacon registers this with the cloud while unpaired;
    /// gating it on `!paired` keeps a stale code off a claimed agent's beacon.
    pub fn pairing_code(&self) -> Option<&str> {
        if self.paired {
            None
        } else {
            self.pairing_code.as_deref()
        }
    }

    /// The stable pending API key the beacon registers, so the key the cloud
    /// freezes at claim time is the one `claim()` persists. `None` when no code
    /// attempt is pending.
    pub fn pending_api_key(&self) -> Option<&str> {
        self.pending_api_key.as_deref()
    }

    /// The code-expiry epoch in MILLISECONDS (`code_created_at + CODE_TTL`), or
    /// `None` when no code has a creation time. Epoch ms because the GCS countdown
    /// reads ms; the on-disk `code_created_at` is epoch seconds.
    pub fn code_expires_at_ms(&self) -> Option<i64> {
        self.code_created_at
            .map(|created| ((created + CODE_TTL_SECS) * 1000.0) as i64)
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

    #[test]
    fn unpaired_state_exposes_the_code_pending_key_and_expiry() {
        // The Python manager's unpaired state: a code + a pending key + a
        // creation time, no `paired` flag.
        let path = temp_json(
            "beacon",
            r#"{"pairing_code":"ABC234","pending_api_key":"ados_pending","code_created_at":1000.0}"#,
        );
        let s = PairingState::load_from(&path);
        assert!(!s.is_paired());
        assert_eq!(s.pairing_code(), Some("ABC234"));
        assert_eq!(s.pending_api_key(), Some("ados_pending"));
        // Expiry = (created + 24h) * 1000 ms = (1000 + 86400) * 1000.
        let expected_ms = ((1000.0 + super::CODE_TTL_SECS) * 1000.0) as i64;
        assert_eq!(s.code_expires_at_ms(), Some(expected_ms));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn paired_state_hides_the_pairing_code() {
        // Once paired the manager pops the code; the accessor must not surface a
        // residual one (a stale code must never ride a claimed agent's beacon).
        let path = temp_json(
            "paired-code",
            r#"{"paired":true,"api_key":"k","pairing_code":"STALE1"}"#,
        );
        let s = PairingState::load_from(&path);
        assert!(s.is_paired());
        assert_eq!(s.pairing_code(), None);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn code_expiry_is_none_without_a_creation_time() {
        let path = temp_json("nocode", r#"{"pairing_code":"ABC234"}"#);
        let s = PairingState::load_from(&path);
        // A code with no creation time has no derivable expiry.
        assert_eq!(s.code_expires_at_ms(), None);
        let _ = std::fs::remove_file(&path);
    }
}
