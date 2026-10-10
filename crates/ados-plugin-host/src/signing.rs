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
//! * Revocation list: `/etc/ados/plugin-revocations.json`, either a bare JSON
//!   array of signer ids or `{"signers": [...], "archives": [...]}` where
//!   `archives` holds lowercase sha256 hex digests of `.adosplug` files. A
//!   plugin signed by a revoked id, or an archive whose digest is listed,
//!   refuses to install or enable.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use base64::Engine;
use ed25519_dalek::{Signature, VerifyingKey};

use crate::errors::{SignatureError, SignatureErrorKind};

/// Default trusted-keys directory.
pub const PLUGIN_KEYS_DIR: &str = "/etc/ados/plugin-keys";
/// Default revocation-list path.
pub const PLUGIN_REVOCATIONS_PATH: &str = "/etc/ados/plugin-revocations.json";

/// First-party Altnautica signers: the id and the exact Ed25519 public key
/// bytes (`scripts/plugin-keys/<id>.pem`).
///
/// **Security boundary** — the key bytes are pinned in code so a key file
/// planted under a first-party name in `/etc/ados/plugin-keys/` (by `trust.d`
/// or `--plugin-key`) verifies only as a third party. First-party status
/// unlocks the `inline` GCS isolation level, `resources.class: heavy`, and an
/// agent half on a service backend that enforces no sandbox. Rotate by adding
/// the new signer and dropping the retired one in a deliberate code change.
pub const FIRST_PARTY_SIGNERS: &[(&str, [u8; 32])] = &[
    (
        "altnautica-2026-A",
        [
            0x4f, 0x71, 0x59, 0x21, 0xb6, 0xc7, 0xc5, 0x2f, 0x8a, 0x32, 0x71, 0x09, 0x93, 0xda,
            0x90, 0xf7, 0x0f, 0x35, 0xe1, 0x01, 0x73, 0xed, 0xda, 0x99, 0x04, 0xf0, 0x82, 0x1e,
            0xaa, 0x3b, 0x81, 0x7e,
        ],
    ),
    (
        "altnautica-2026-B",
        [
            0xbe, 0x0f, 0xb1, 0x72, 0x43, 0x57, 0x7c, 0x58, 0x37, 0xbf, 0x26, 0x3f, 0xf0, 0x23,
            0x04, 0x0e, 0x82, 0x05, 0x16, 0x32, 0x96, 0xa7, 0x76, 0x0d, 0x38, 0x95, 0x14, 0x83,
            0xaf, 0x8f, 0x4b, 0x7b,
        ],
    ),
];

/// First-party status: the id is on [`FIRST_PARTY_SIGNERS`] AND the key the
/// signature verified against is that id's pinned key, byte for byte. Never
/// inferred from a path, a prefix match, or a file name.
pub fn is_first_party_signer(signer_id: &str, key: &VerifyingKey) -> bool {
    FIRST_PARTY_SIGNERS
        .iter()
        .any(|(id, bytes)| *id == signer_id && key.as_bytes() == bytes)
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

/// The revocation list: revoked signer ids, revoked archive digests
/// (lowercase sha256 hex of the `.adosplug` file), and revoked plugin versions
/// (`<plugin_id>@<version>`, the form the registry revokes a release by).
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Revocations {
    #[serde(default)]
    pub signers: BTreeSet<String>,
    #[serde(default)]
    pub archives: BTreeSet<String>,
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub versions: BTreeSet<String>,
}

impl Revocations {
    /// True when `sha256_hex` names a revoked archive.
    pub fn archive_revoked(&self, sha256_hex: &str) -> bool {
        self.archives.contains(&sha256_hex.to_ascii_lowercase())
    }

    /// True when `version` of `plugin_id` is a revoked release.
    pub fn version_revoked(&self, plugin_id: &str, version: &str) -> bool {
        self.versions.contains(&format!("{plugin_id}@{version}"))
    }

    /// Total entry count, the add-only merge's change detector.
    fn len(&self) -> usize {
        self.signers.len() + self.archives.len() + self.versions.len()
    }
}

/// Parse a revocation document: a bare array of signer ids; the local
/// `{"signers": [...], "archives": [...], "versions": [...]}` object; or the
/// registry's `/v1/revoked.json` object `{"signer_keys": [...],
/// "plugin_versions": ["<id>@<version>", ...], "generated_at": <ms>}`, whose
/// signer keys are signer ids and whose plugin versions carry no archive
/// digest. Both object shapes may appear in one document. Anything else is an
/// error, never an empty list: a garbled file must not silently re-trust a
/// revoked signer.
pub fn parse_revocations(bytes: &[u8]) -> Result<Revocations, String> {
    let value: serde_json::Value =
        serde_json::from_slice(bytes).map_err(|e| format!("revocation list is not JSON: {e}"))?;
    let strings = |v: serde_json::Value, field: &str| -> Result<BTreeSet<String>, String> {
        let serde_json::Value::Array(items) = v else {
            return Err(format!("revocation list `{field}` is not an array"));
        };
        items
            .into_iter()
            .map(|item| match item {
                serde_json::Value::String(s) if !s.trim().is_empty() => Ok(s.trim().to_string()),
                other => Err(format!("revocation list `{field}` holds {other}")),
            })
            .collect()
    };
    match value {
        v @ serde_json::Value::Array(_) => Ok(Revocations {
            signers: strings(v, "signers")?,
            ..Revocations::default()
        }),
        serde_json::Value::Object(mut map) => {
            let mut take = |k: &str| map.remove(k).map_or(Ok(BTreeSet::new()), |v| strings(v, k));
            let mut signers = take("signers")?;
            signers.extend(take("signer_keys")?);
            let archives = take("archives")?
                .into_iter()
                .map(|a| a.to_ascii_lowercase())
                .collect();
            let mut versions = take("versions")?;
            versions.extend(take("plugin_versions")?);
            if let Some(bad) = versions.iter().find(|v| !is_plugin_version_ref(v)) {
                return Err(format!(
                    "revocation list version `{bad}` is not <plugin_id>@<version>"
                ));
            }
            Ok(Revocations {
                signers,
                archives,
                versions,
            })
        }
        _ => Err("revocation list is neither an array nor an object".to_string()),
    }
}

/// Whether `entry` is a `<plugin_id>@<version>` reference with both halves set.
fn is_plugin_version_ref(entry: &str) -> bool {
    entry
        .split_once('@')
        .is_some_and(|(id, version)| !id.is_empty() && !version.is_empty())
}

/// Read the revocation list at `path`. A missing file is an empty list; an
/// unreadable or malformed one is an error.
pub fn load_revocations(path: &Path) -> Result<Revocations, String> {
    match std::fs::read(path) {
        Ok(bytes) => parse_revocations(&bytes),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Revocations::default()),
        Err(e) => Err(format!("revocation list unreadable: {e}")),
    }
}

/// Merge `incoming` into the list at `path`, add-only: an entry is never
/// removed. Writes durably and only when something was added; returns whether
/// it did. A malformed local file is an error and is left untouched.
pub fn merge_revocations(path: &Path, incoming: &Revocations) -> Result<bool, String> {
    let mut current = load_revocations(path)?;
    let before = current.len();
    current.signers.extend(incoming.signers.iter().cloned());
    current
        .archives
        .extend(incoming.archives.iter().map(|a| a.to_ascii_lowercase()));
    current.versions.extend(incoming.versions.iter().cloned());
    if current.len() == before {
        return Ok(false);
    }
    let mut bytes = serde_json::to_vec_pretty(&current).map_err(|e| e.to_string())?;
    bytes.push(b'\n');
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    ados_protocol::sidecar::write_durable(path, &bytes).map_err(|e| e.to_string())?;
    Ok(true)
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

    fn pinned(id: &str) -> VerifyingKey {
        let (_, bytes) = FIRST_PARTY_SIGNERS.iter().find(|(i, _)| *i == id).unwrap();
        VerifyingKey::from_bytes(bytes).unwrap()
    }

    #[test]
    fn first_party_needs_the_pinned_key_bytes() {
        assert!(is_first_party_signer(
            "altnautica-2026-A",
            &pinned("altnautica-2026-A")
        ));
        assert!(is_first_party_signer(
            "altnautica-2026-B",
            &pinned("altnautica-2026-B")
        ));
        // The right name with another key is not first-party.
        assert!(!is_first_party_signer(
            "altnautica-2026-A",
            &pinned("altnautica-2026-B")
        ));
        let forged = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]).verifying_key();
        assert!(!is_first_party_signer("altnautica-2026-A", &forged));
        // Prefix-matching must NOT grant first-party status.
        assert!(!is_first_party_signer(
            "altnautica-2026-A-evil",
            &pinned("altnautica-2026-A")
        ));
    }

    #[test]
    fn pinned_bytes_match_the_shipped_pem_files() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/plugin-keys");
        for (id, bytes) in FIRST_PARTY_SIGNERS {
            let pem = std::fs::read_to_string(dir.join(format!("{id}.pem"))).unwrap();
            assert_eq!(
                verifying_key_from_pem(&pem).unwrap().as_bytes(),
                bytes,
                "{id}"
            );
        }
    }

    #[test]
    fn missing_file_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let revs = load_revocations(&dir.path().join("absent.json")).unwrap();
        assert_eq!(revs, Revocations::default());
    }

    #[test]
    fn revocations_parse_both_shapes_and_refuse_garbage() {
        let bare = parse_revocations(br#"["alice"]"#).unwrap();
        assert!(bare.signers.contains("alice") && bare.archives.is_empty());
        let v2 = parse_revocations(br#"{"signers":["bob"],"archives":["ABCD"]}"#).unwrap();
        assert!(v2.signers.contains("bob"));
        assert!(v2.archive_revoked("abcd"));
        assert!(parse_revocations(br#"{"signers":[]"#).is_err());
        assert!(parse_revocations(br#""alice""#).is_err());
        assert!(parse_revocations(br#"{"signers":"alice"}"#).is_err());
        assert!(parse_revocations(br#"[1]"#).is_err());
    }

    #[test]
    fn the_registry_revoked_json_shape_parses() {
        // The exact body the registry's `/v1/revoked.json` serves.
        let registry = br#"{"signer_keys":["community-2025a"],"plugin_versions":["com.example.thermal@1.2.0"],"generated_at":1760000000000}"#;
        let revs = parse_revocations(registry).unwrap();
        assert!(revs.signers.contains("community-2025a"));
        assert!(revs.archives.is_empty());
        assert!(revs.version_revoked("com.example.thermal", "1.2.0"));
        assert!(!revs.version_revoked("com.example.thermal", "1.2.1"));
        // An empty registry list is an empty revocation set, not an error.
        let empty = br#"{"signer_keys":[],"plugin_versions":[],"generated_at":1}"#;
        assert_eq!(parse_revocations(empty).unwrap(), Revocations::default());
        // A version entry that is not <id>@<version> is refused.
        assert!(parse_revocations(br#"{"plugin_versions":["com.example.thermal"]}"#).is_err());
        assert!(parse_revocations(br#"{"plugin_versions":["@1.0.0"]}"#).is_err());
    }

    #[test]
    fn a_registry_merge_adds_its_versions_and_keeps_the_local_shape() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("revoked.json");
        std::fs::write(&path, br#"{"signers":["alice"],"archives":["ab12"]}"#).unwrap();
        let registry = parse_revocations(
            br#"{"signer_keys":["bob"],"plugin_versions":["com.example.web@2.0.0"],"generated_at":5}"#,
        )
        .unwrap();
        assert!(merge_revocations(&path, &registry).unwrap());
        let merged = load_revocations(&path).unwrap();
        assert!(merged.signers.contains("alice") && merged.signers.contains("bob"));
        assert!(merged.archive_revoked("ab12"));
        assert!(merged.version_revoked("com.example.web", "2.0.0"));
        let on_disk: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(
            on_disk["versions"],
            serde_json::json!(["com.example.web@2.0.0"])
        );
        assert!(
            !merge_revocations(&path, &registry).unwrap(),
            "add-only, idempotent"
        );
    }

    #[test]
    fn merge_is_add_only_and_durable() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("revoked.json");
        std::fs::write(&path, br#"["alice"]"#).unwrap();
        let incoming = Revocations {
            signers: ["bob".to_string()].into(),
            archives: ["EF01".to_string()].into(),
            ..Revocations::default()
        };
        assert!(merge_revocations(&path, &incoming).unwrap());
        let merged = load_revocations(&path).unwrap();
        assert!(merged.signers.contains("alice") && merged.signers.contains("bob"));
        assert!(merged.archives.contains("ef01"));
        // Nothing new: no write; an empty incoming never removes.
        assert!(!merge_revocations(&path, &incoming).unwrap());
        assert!(!merge_revocations(&path, &Revocations::default()).unwrap());
        assert_eq!(load_revocations(&path).unwrap(), merged);
        // A malformed local file is refused, not overwritten.
        std::fs::write(&path, b"{garbled").unwrap();
        assert!(merge_revocations(&path, &incoming).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"{garbled");
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
