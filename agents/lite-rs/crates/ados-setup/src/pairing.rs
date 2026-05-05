//! Pairing-state persistence at `/etc/ados/pairing.json`.
//!
//! Mirrors `src/ados/core/pairing.py` from the Python full agent. Same
//! file path, same JSON schema, same field names. An operator can swap
//! between the Python full agent and the Rust lite agent on the same
//! board without re-pairing — both halves load the same on-disk shape.
//!
//! Schema fields (all optional except `pairing_code` once written):
//!
//! - `pairing_code`     — short alphanumeric code shown to the operator
//! - `code_created_at`  — epoch seconds when the code was minted
//! - `paired`           — true once the cloud relay claimed the agent
//! - `api_key`          — `ados_<base64url-32>`-shaped per-device API key
//! - `owner_id`         — Convex user id of the operator who claimed it
//! - `paired_at`        — epoch seconds when the claim landed

use std::path::PathBuf;

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use rand::seq::SliceRandom;
use rand::{rngs::OsRng, RngCore};
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Same charset as the Python `core/pairing.py:SAFE_CHARSET` —
/// excludes ambiguous glyphs (0/O, 1/I, L) so an operator reading a code
/// off an OLED or a sticker doesn't mistype.
const SAFE_CHARSET: &[u8] = b"ABCDEFGHJKMNPQRSTUVWXYZ23456789";
const CODE_LENGTH: usize = 6;
const CODE_TTL_SECS: f64 = 900.0; // 15 minutes — matches Python

#[derive(Debug, Error)]
pub enum PairingError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("already paired")]
    AlreadyPaired,
}

/// On-disk shape. `serde(default)` everywhere so a partial file (mid-flow)
/// reads back as the right defaults instead of failing.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct PairingState {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pairing_code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code_created_at: Option<f64>,
    #[serde(default)]
    pub paired: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub paired_at: Option<f64>,
}

impl PairingState {
    pub fn is_paired(&self) -> bool {
        self.paired && self.api_key.is_some()
    }
}

#[derive(Debug, Clone)]
pub struct PairingStore {
    path: PathBuf,
}

impl PairingStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// Load. Missing or corrupt file resolves to defaults — same leniency
    /// the Python implementation has.
    pub fn load(&self) -> Result<PairingState, PairingError> {
        if !self.path.exists() {
            return Ok(PairingState::default());
        }
        let raw = match std::fs::read_to_string(&self.path) {
            Ok(s) => s,
            Err(_) => return Ok(PairingState::default()),
        };
        if raw.trim().is_empty() {
            return Ok(PairingState::default());
        }
        match serde_json::from_str::<PairingState>(&raw) {
            Ok(state) => Ok(state),
            Err(_) => Ok(PairingState::default()),
        }
    }

    /// Save via tempfile + rename. Pretty-printed (indent=2) to match the
    /// Python agent's `json.dumps(state, indent=2)`. Permissions tightened
    /// to 0600 because the file holds the device API key.
    pub fn save(&self, state: &PairingState) -> Result<(), PairingError> {
        let parent = self
            .path
            .parent()
            .unwrap_or_else(|| std::path::Path::new("."));
        std::fs::create_dir_all(parent)?;
        let serialized = serde_json::to_string_pretty(state)?;
        let tmp = parent.join(format!(".pairing.json.{}.tmp", std::process::id()));
        std::fs::write(&tmp, serialized)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
        }
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }

    /// Apply a fresh pair code (operator typed `ados-agent-lite pair CODE`
    /// or installed with `--pair`). Resets paired flag so the cloud relay
    /// can re-claim cleanly.
    pub fn set_code(&self, code: &str) -> Result<(), PairingError> {
        let mut state = self.load()?;
        state.pairing_code = Some(code.to_uppercase());
        state.code_created_at = Some(now_epoch());
        state.paired = false;
        state.api_key = None;
        state.owner_id = None;
        state.paired_at = None;
        self.save(&state)
    }

    /// Record a successful claim from the cloud relay.
    pub fn claim(&self, owner_id: &str, api_key: &str) -> Result<PairingState, PairingError> {
        let mut state = self.load()?;
        if state.is_paired() {
            return Err(PairingError::AlreadyPaired);
        }
        state.paired = true;
        state.api_key = Some(api_key.to_string());
        state.owner_id = Some(owner_id.to_string());
        state.paired_at = Some(now_epoch());
        self.save(&state)?;
        Ok(state)
    }

    /// Forget the pair binding (operator unpair / Mission Control "Remove drone").
    pub fn unpair(&self) -> Result<(), PairingError> {
        self.save(&PairingState::default())
    }

    /// Return the current pair code if it is still within its TTL,
    /// otherwise mint a fresh one and persist it. Mirrors Python's
    /// `PairingManager.get_or_create_code()`. The pair code is what
    /// the operator types into Mission Control's "Add drone" dialog.
    pub fn get_or_create_code(&self) -> Result<String, PairingError> {
        let mut state = self.load()?;
        let now = now_epoch();
        let still_fresh = state
            .pairing_code
            .as_deref()
            .filter(|c| !c.is_empty())
            .and_then(|code| {
                let created = state.code_created_at.unwrap_or(0.0);
                if (now - created) < CODE_TTL_SECS {
                    Some(code.to_string())
                } else {
                    None
                }
            });
        if let Some(code) = still_fresh {
            return Ok(code);
        }
        let code = generate_code();
        state.pairing_code = Some(code.clone());
        state.code_created_at = Some(now);
        // Code rotation does not auto-unpair; only an explicit
        // operator action (set_code on a fresh code, or unpair) wipes
        // the API key. Keeping the existing api_key here lets a
        // re-pairing flow detect the device is already claimed.
        self.save(&state)?;
        Ok(code)
    }
}

/// Generate a 6-char operator-friendly code from the safe charset.
/// Cryptographically random — uses `OsRng`.
pub fn generate_code() -> String {
    let mut rng = OsRng;
    (0..CODE_LENGTH)
        .map(|_| {
            *SAFE_CHARSET
                .choose(&mut rng)
                .expect("SAFE_CHARSET is non-empty")
        })
        .map(|b| b as char)
        .collect()
}

/// Generate an API key matching Python's `"ados_" + secrets.token_urlsafe(32)`.
/// 32 random bytes encoded as URL-safe base64 with no padding.
pub fn generate_api_key() -> String {
    let mut bytes = [0u8; 32];
    OsRng.fill_bytes(&mut bytes);
    let encoded = URL_SAFE_NO_PAD.encode(bytes);
    format!("ados_{encoded}")
}

fn now_epoch() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_returns_default_when_missing() {
        let dir = tempfile::tempdir().unwrap();
        let store = PairingStore::new(dir.path().join("pairing.json"));
        let state = store.load().unwrap();
        assert!(!state.paired);
        assert_eq!(state.api_key, None);
    }

    #[test]
    fn cross_compat_with_python_written_pairing() {
        // Bytes the Python agent writes after a claim — pretty-printed
        // (indent=2). Field order is irrelevant on the read side because
        // serde_json is order-agnostic, but we mirror the Python order
        // here for documentation.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pairing.json");
        let python_json = br#"{
  "pairing_code": "AB23X4",
  "code_created_at": 1735660000.123,
  "paired": true,
  "api_key": "ados_secret-key-32-chars-base64url",
  "owner_id": "user-abc",
  "paired_at": 1735660030.456
}"#;
        std::fs::write(&path, python_json).unwrap();
        let store = PairingStore::new(&path);
        let loaded = store.load().unwrap();
        assert!(loaded.is_paired());
        assert_eq!(loaded.api_key.as_deref(), Some("ados_secret-key-32-chars-base64url"));
        assert_eq!(loaded.owner_id.as_deref(), Some("user-abc"));
        assert_eq!(loaded.pairing_code.as_deref(), Some("AB23X4"));
    }

    #[test]
    fn set_code_resets_paired_flag() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pairing.json");
        let store = PairingStore::new(&path);
        // Pre-existing claimed state.
        let mut prior = PairingState::default();
        prior.paired = true;
        prior.api_key = Some("ados_old".into());
        prior.owner_id = Some("user-x".into());
        store.save(&prior).unwrap();
        // New code arrives.
        store.set_code("xyz123").unwrap();
        let loaded = store.load().unwrap();
        assert!(!loaded.paired);
        assert_eq!(loaded.api_key, None);
        assert_eq!(loaded.pairing_code.as_deref(), Some("XYZ123"));
    }

    #[test]
    fn claim_records_owner_and_key() {
        let dir = tempfile::tempdir().unwrap();
        let store = PairingStore::new(dir.path().join("pairing.json"));
        store.set_code("AB23X4").unwrap();
        let state = store.claim("user-abc", "ados_secret").unwrap();
        assert!(state.is_paired());
        assert_eq!(state.api_key.as_deref(), Some("ados_secret"));
        assert_eq!(state.owner_id.as_deref(), Some("user-abc"));
        assert!(state.paired_at.is_some());
    }

    #[test]
    fn claim_rejects_already_paired() {
        let dir = tempfile::tempdir().unwrap();
        let store = PairingStore::new(dir.path().join("pairing.json"));
        store.set_code("AB23X4").unwrap();
        store.claim("user-1", "ados_k1").unwrap();
        let err = store.claim("user-2", "ados_k2").unwrap_err();
        assert!(matches!(err, PairingError::AlreadyPaired));
    }

    #[test]
    fn unpair_clears_state() {
        let dir = tempfile::tempdir().unwrap();
        let store = PairingStore::new(dir.path().join("pairing.json"));
        store.set_code("AB23X4").unwrap();
        store.claim("user-abc", "ados_k").unwrap();
        store.unpair().unwrap();
        let loaded = store.load().unwrap();
        assert!(!loaded.paired);
        assert_eq!(loaded.api_key, None);
    }

    #[test]
    fn save_pretty_prints_with_indent_two() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pairing.json");
        let store = PairingStore::new(&path);
        let mut state = PairingState::default();
        state.paired = true;
        state.api_key = Some("ados_test".into());
        state.owner_id = Some("user".into());
        store.save(&state).unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        // Pretty printed: contains a newline + 2-space indent before "api_key" et al.
        assert!(raw.contains("\n  "));
    }

    #[test]
    fn corrupt_file_resolves_to_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pairing.json");
        std::fs::write(&path, "not json").unwrap();
        let store = PairingStore::new(&path);
        let loaded = store.load().unwrap();
        assert!(!loaded.paired);
    }

    #[test]
    fn generate_code_returns_safe_charset_only() {
        // 100 codes, all should be 6 chars from the safe charset.
        for _ in 0..100 {
            let code = generate_code();
            assert_eq!(code.len(), CODE_LENGTH);
            for ch in code.chars() {
                assert!(
                    SAFE_CHARSET.contains(&(ch as u8)),
                    "code {} contains forbidden char {}",
                    code,
                    ch
                );
            }
            // Forbidden glyphs are ambiguous reads — guarantee they never appear.
            for forbidden in ['0', 'O', '1', 'I', 'L'] {
                assert!(
                    !code.contains(forbidden),
                    "code {} contains forbidden glyph {}",
                    code,
                    forbidden
                );
            }
        }
    }

    #[test]
    fn generate_api_key_has_correct_prefix_and_length() {
        let key = generate_api_key();
        assert!(key.starts_with("ados_"));
        // 32 bytes -> ceil(32 / 3) * 4 = 44 chars in standard base64;
        // url_safe_no_pad strips the padding so 32 bytes -> 43 chars.
        // Total length: "ados_" (5) + 43 = 48.
        assert_eq!(key.len(), 48);
        // Charset check — every char after the prefix is base64url-safe.
        let suffix = &key[5..];
        for ch in suffix.chars() {
            assert!(
                ch.is_ascii_alphanumeric() || ch == '-' || ch == '_',
                "api key suffix contains non-base64url char: {}",
                ch
            );
        }
    }

    #[test]
    fn generate_api_key_returns_unique_values() {
        let a = generate_api_key();
        let b = generate_api_key();
        assert_ne!(a, b, "two consecutive api keys collided — RNG broken");
    }

    #[test]
    fn get_or_create_code_returns_fresh_code_within_ttl() {
        let dir = tempfile::tempdir().unwrap();
        let store = PairingStore::new(dir.path().join("pairing.json"));
        let code1 = store.get_or_create_code().unwrap();
        let code2 = store.get_or_create_code().unwrap();
        // Within TTL, second call returns the same code as the first.
        assert_eq!(code1, code2);
    }

    #[test]
    fn get_or_create_code_regenerates_after_ttl_expiry() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pairing.json");
        let store = PairingStore::new(&path);
        // Seed an "old" code by writing one with code_created_at far in the past.
        let mut state = PairingState::default();
        state.pairing_code = Some("ABCDEF".into());
        state.code_created_at = Some(0.0); // way pre-1970-equivalent in TTL terms
        store.save(&state).unwrap();
        // New call should regenerate (because (now - 0) > 900 seconds).
        let fresh = store.get_or_create_code().unwrap();
        assert_ne!(fresh, "ABCDEF");
        assert_eq!(fresh.len(), CODE_LENGTH);
    }

    #[test]
    fn get_or_create_code_preserves_existing_api_key() {
        // A regenerated code should NOT clear the api_key — the device
        // stays paired even as the operator-facing code rotates.
        let dir = tempfile::tempdir().unwrap();
        let store = PairingStore::new(dir.path().join("pairing.json"));
        store.set_code("INITIAL").unwrap();
        store.claim("user-x", "ados_initial_key").unwrap();
        // Force regen by forging the timestamp.
        let mut state = store.load().unwrap();
        state.code_created_at = Some(0.0);
        store.save(&state).unwrap();
        // Get fresh code; api_key must still be intact.
        let _ = store.get_or_create_code().unwrap();
        let after = store.load().unwrap();
        assert!(after.is_paired());
        assert_eq!(after.api_key.as_deref(), Some("ados_initial_key"));
    }
}
