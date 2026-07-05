//! Ed25519 signature verification, trusted-keys store, revocation list, and
//! the first-party allowlist for plugins.
//!
//! The signing model:
//!
//! * Signing payload: the 32-byte canonical payload hash of the archive (the
//!   manifest plus assets, the signature file excluded). See [`crate::archive`]
//!   for the canonical layout. The Ed25519 signature is computed over the
//!   32-byte digest itself, so verification runs in constant time independent
//!   of archive size.
//! * Signature format: base64-encoded raw 64-byte Ed25519 signature.
//! * Trusted-keys store: PEM (SPKI) public keys under `/etc/ados/plugin-keys/`,
//!   filename `<signer-id>.pem`.
//! * Revocation list: a JSON list of signer ids at
//!   `/etc/ados/plugin-revocations.json`. A plugin signed with a revoked id
//!   refuses to load.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use base64::Engine;
use ed25519_dalek::{Signature, VerifyingKey};

use crate::errors::{SignatureError, SignatureErrorKind};

/// Default trusted-keys directory.
pub const PLUGIN_KEYS_DIR: &str = "/etc/ados/plugin-keys";
/// Default revocation-list path.
pub const PLUGIN_REVOCATIONS_PATH: &str = "/etc/ados/plugin-revocations.json";

/// Hardcoded allowlist of first-party Altnautica signer ids.
///
/// **Security boundary** — this is maintained in code rather than via
/// filesystem prefix-matching so a malicious actor with write access to
/// `/etc/ados/plugin-keys/` cannot plant a key file with the right prefix and
/// impersonate first-party status. First-party status unlocks the `inprocess`
/// agent isolation level and the `inline` GCS isolation level; third parties
/// cannot use either even if they declare them in the manifest. Rotate by
/// adding the new signer id and dropping the retired one in a deliberate code
/// change.
pub const FIRST_PARTY_SIGNERS: &[&str] = &["altnautica-2026-A", "altnautica-2026-B"];

/// First-party status is granted only to ids on the explicit allowlist. It is
/// never inferred from a path, a prefix match, or a directory listing.
pub fn is_first_party_signer(signer_id: &str) -> bool {
    FIRST_PARTY_SIGNERS.contains(&signer_id)
}

/// A loaded trusted public key.
#[derive(Debug, Clone)]
pub struct TrustedKey {
    pub signer_id: String,
    pub verifying_key: VerifyingKey,
}

/// Load every PEM public key from the trusted-keys directory. The map is keyed
/// by `signer_id` (the filename stem). A missing or empty directory returns an
/// empty map; the caller decides whether that is fatal. A key file that fails
/// to read or fails to decode as an Ed25519 SPKI PEM is skipped (mirroring the
/// Python loader's per-file tolerance).
pub fn load_trusted_keys(keys_dir: Option<&Path>) -> BTreeMap<String, TrustedKey> {
    let base: PathBuf = keys_dir
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from(PLUGIN_KEYS_DIR));
    let mut keys: BTreeMap<String, TrustedKey> = BTreeMap::new();
    let Ok(read_dir) = std::fs::read_dir(&base) else {
        tracing::info!(path = %base.display(), "plugin_keys_dir_missing");
        return keys;
    };
    let mut paths: Vec<PathBuf> = read_dir
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("pem"))
        .collect();
    paths.sort();
    for path in paths {
        let Some(signer_id) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        let pem = match std::fs::read_to_string(&path) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(signer_id, error = %e, "plugin_key_read_failed");
                continue;
            }
        };
        match verifying_key_from_pem(&pem) {
            Ok(verifying_key) => {
                keys.insert(
                    signer_id.to_string(),
                    TrustedKey {
                        signer_id: signer_id.to_string(),
                        verifying_key,
                    },
                );
            }
            Err(e) => {
                tracing::warn!(signer_id, error = %e, "plugin_key_decode_failed");
            }
        }
    }
    keys
}

/// Decode an Ed25519 SPKI PEM public key into a [`VerifyingKey`].
pub fn verifying_key_from_pem(pem: &str) -> Result<VerifyingKey, String> {
    use ed25519_dalek::pkcs8::DecodePublicKey;
    VerifyingKey::from_public_key_pem(pem).map_err(|e| format!("not an Ed25519 SPKI PEM: {e}"))
}

/// Read the revocation list. A missing file returns an empty set; a malformed
/// file (not a JSON list) returns an empty set with a warning, matching the
/// Python `load_revocation_list`.
pub fn load_revocation_list(path: Option<&Path>) -> BTreeSet<String> {
    let target: PathBuf = path
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from(PLUGIN_REVOCATIONS_PATH));
    let raw = match std::fs::read_to_string(&target) {
        Ok(s) => s,
        Err(_) => return BTreeSet::new(),
    };
    let parsed: serde_json::Value = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = %e, "plugin_revocations_read_failed");
            return BTreeSet::new();
        }
    };
    let serde_json::Value::Array(items) = parsed else {
        tracing::warn!("plugin_revocations_bad_shape");
        return BTreeSet::new();
    };
    items
        .into_iter()
        .map(|v| match v {
            serde_json::Value::String(s) => s,
            other => other.to_string(),
        })
        .collect()
}

/// Verify the plugin archive signature.
///
/// **Security boundary** — the signed payload is the 32-byte canonical payload
/// hash, base64 sig decoded to the raw 64-byte Ed25519 signature, verified
/// against the trusted key for `signer_id`. Returns `Ok(())` on success and a
/// kinded [`SignatureError`] on every failure path (revoked, unknown signer,
/// malformed base64, wrong-length sig, verify failure), matching the order and
/// `kind` of the Python `verify_archive_signature`.
pub fn verify_archive_signature(
    payload_hash: &[u8; 32],
    signature_b64: &str,
    signer_id: &str,
    trusted_keys: &BTreeMap<String, TrustedKey>,
    revocations: &BTreeSet<String>,
) -> Result<(), SignatureError> {
    if revocations.contains(signer_id) {
        return Err(SignatureError::new(
            SignatureErrorKind::Revoked,
            format!("signer {signer_id} is on the revocation list"),
        ));
    }

    let Some(key) = trusted_keys.get(signer_id) else {
        return Err(SignatureError::new(
            SignatureErrorKind::UnknownSigner,
            format!("signer {signer_id} not in {PLUGIN_KEYS_DIR}/"),
        ));
    };

    if !verify_ed25519(payload_hash, signature_b64, &key.verifying_key) {
        return Err(SignatureError::new(
            SignatureErrorKind::Invalid,
            format!("signature does not verify under key {signer_id}"),
        ));
    }

    tracing::info!(signer_id, "plugin_signature_verified");
    Ok(())
}

/// Verify a base64 Ed25519 signature over `data` with `key`. Any decode or
/// length failure verifies as `false`, mirroring the Python signing verifier's
/// catch-all-and-return-false posture (the caller maps a `false` to the
/// `invalid` kind).
pub fn verify_ed25519(data: &[u8], signature_b64: &str, key: &VerifyingKey) -> bool {
    let Ok(sig_bytes) = base64::engine::general_purpose::STANDARD.decode(signature_b64) else {
        return false;
    };
    let sig_array: [u8; 64] = match sig_bytes.try_into() {
        Ok(a) => a,
        Err(_) => return false,
    };
    let sig = Signature::from_bytes(&sig_array);
    key.verify_strict(data, &sig).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_party_allowlist_is_exact() {
        assert!(is_first_party_signer("altnautica-2026-A"));
        assert!(is_first_party_signer("altnautica-2026-B"));
        // Prefix-matching must NOT grant first-party status.
        assert!(!is_first_party_signer("altnautica-2026-A-evil"));
        assert!(!is_first_party_signer("altnautica-2099-Z"));
        assert!(!is_first_party_signer("third-party"));
    }

    #[test]
    fn revoked_signer_rejected_before_key_lookup() {
        let keys = BTreeMap::new();
        let mut revs = BTreeSet::new();
        revs.insert("bad".to_string());
        let err = verify_archive_signature(&[0u8; 32], "QUJD", "bad", &keys, &revs).unwrap_err();
        assert_eq!(err.kind, SignatureErrorKind::Revoked);
    }

    #[test]
    fn unknown_signer_rejected() {
        let keys = BTreeMap::new();
        let revs = BTreeSet::new();
        let err = verify_archive_signature(&[0u8; 32], "QUJD", "nobody", &keys, &revs).unwrap_err();
        assert_eq!(err.kind, SignatureErrorKind::UnknownSigner);
    }

    #[test]
    fn malformed_base64_does_not_verify() {
        // A made-up key; the point is the base64 decode fails first.
        let vk = VerifyingKey::from_bytes(&[1u8; 32]);
        if let Ok(vk) = vk {
            assert!(!verify_ed25519(b"data", "!!!not base64!!!", &vk));
        }
    }
}
