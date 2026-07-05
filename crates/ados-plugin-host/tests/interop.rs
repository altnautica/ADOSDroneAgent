//! Cross-language byte-parity tests for the plugin lifecycle security
//! boundaries.
//!
//! `tests/interop/fixtures.json` is generated from the live Python agent code
//! (`tests/interop/generate_fixtures.py` imports `ados.plugins.archive` for the
//! canonical payload hash and `ados.services.signing` for the Ed25519
//! verifier). These tests assert the Rust `ados-plugin-host` crate computes the
//! exact same canonical hash and verifies a signature the agent produced, plus
//! the tampered / revoked / unknown-signer reject paths. This is the regression
//! guard for the two cross-language security boundaries: the value that gets
//! signed (the canonical hash) and the Ed25519 verification over it.
//!
//! Regenerate the fixture with:
//!   .venv/bin/python crates/ados-plugin-host/tests/interop/generate_fixtures.py

use std::collections::{BTreeMap, BTreeSet};

use base64::Engine;

use ados_plugin_host::archive::{canonical_payload_hash, parse_archive_bytes};
use ados_plugin_host::errors::SignatureErrorKind;
use ados_plugin_host::signing::{
    verify_archive_signature, verify_ed25519, verifying_key_from_pem, TrustedKey,
};

fn fixtures() -> serde_json::Value {
    let raw = include_str!("interop/fixtures.json");
    serde_json::from_str(raw).expect("fixtures.json parses")
}

fn f_str(f: &serde_json::Value, key: &str) -> String {
    f[key].as_str().expect(key).to_string()
}

fn archive_bytes(f: &serde_json::Value) -> Vec<u8> {
    base64::engine::general_purpose::STANDARD
        .decode(f_str(f, "archive_b64"))
        .expect("archive_b64 decodes")
}

#[test]
fn canonical_payload_hash_matches_python() {
    // Parse the same archive the Python generator built; the Rust reader's
    // entry map must produce the byte-identical canonical hash that the agent
    // signs.
    let f = fixtures();
    let contents = parse_archive_bytes(archive_bytes(&f)).expect("parse archive");
    let expected_hex = f_str(&f, "payload_hash_hex");
    assert_eq!(
        hex::encode(contents.payload_hash),
        expected_hex,
        "Rust canonical_payload_hash diverged from the Python value that was signed"
    );
}

#[test]
fn canonical_hash_helper_matches_when_fed_the_same_entries() {
    // Build the entry map by hand from the parsed archive and re-run the bare
    // helper; it must equal the parser's hash and the Python value.
    let f = fixtures();
    let raw = archive_bytes(&f);
    let contents = parse_archive_bytes(raw.clone()).expect("parse archive");

    // Re-derive the entries from the zip and feed the helper directly.
    use std::io::Read;
    let mut zf = zip::ZipArchive::new(std::io::Cursor::new(&raw)).unwrap();
    let mut entries: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    for i in 0..zf.len() {
        let mut file = zf.by_index(i).unwrap();
        if file.is_dir() {
            continue;
        }
        let name = file.name().to_string();
        let mut buf = Vec::new();
        file.read_to_end(&mut buf).unwrap();
        entries.insert(name, buf);
    }
    assert_eq!(canonical_payload_hash(&entries), contents.payload_hash);
    assert_eq!(
        hex::encode(canonical_payload_hash(&entries)),
        f_str(&f, "payload_hash_hex")
    );
}

#[test]
fn ed25519_verifies_python_signature() {
    // The signature the Python agent produced over the canonical hash must
    // verify in Rust against the same SPKI PEM public key.
    let f = fixtures();
    let contents = parse_archive_bytes(archive_bytes(&f)).expect("parse archive");
    let pem = f_str(&f, "public_pem");
    let key = verifying_key_from_pem(&pem).expect("PEM decodes to an Ed25519 key");
    let sig_b64 = f_str(&f, "signature_b64");
    assert!(
        verify_ed25519(&contents.payload_hash, &sig_b64, &key),
        "Rust failed to verify a signature the Python agent accepted"
    );
}

#[test]
fn tampered_signature_is_rejected() {
    let f = fixtures();
    let contents = parse_archive_bytes(archive_bytes(&f)).expect("parse archive");
    let pem = f_str(&f, "public_pem");
    let key = verifying_key_from_pem(&pem).expect("PEM decodes");
    let tampered = f_str(&f, "tampered_signature_b64");
    assert!(
        !verify_ed25519(&contents.payload_hash, &tampered, &key),
        "a tampered signature must not verify"
    );
}

/// Build the trusted-keys map from the fixture for the full verify path.
fn trusted_from_fixture(f: &serde_json::Value) -> BTreeMap<String, TrustedKey> {
    let signer_id = f_str(f, "signer_id");
    let pem = f_str(f, "public_pem");
    let key = verifying_key_from_pem(&pem).expect("PEM decodes");
    let mut map = BTreeMap::new();
    map.insert(
        signer_id.clone(),
        TrustedKey {
            signer_id,
            verifying_key: key,
        },
    );
    map
}

#[test]
fn full_verify_path_accepts_trusted_signer() {
    let f = fixtures();
    let contents = parse_archive_bytes(archive_bytes(&f)).expect("parse archive");
    let trusted = trusted_from_fixture(&f);
    let revocations = BTreeSet::new();
    verify_archive_signature(
        &contents.payload_hash,
        &f_str(&f, "signature_b64"),
        &f_str(&f, "signer_id"),
        &trusted,
        &revocations,
    )
    .expect("trusted signer must verify");
}

#[test]
fn full_verify_path_rejects_tampered_signature_as_invalid() {
    let f = fixtures();
    let contents = parse_archive_bytes(archive_bytes(&f)).expect("parse archive");
    let trusted = trusted_from_fixture(&f);
    let err = verify_archive_signature(
        &contents.payload_hash,
        &f_str(&f, "tampered_signature_b64"),
        &f_str(&f, "signer_id"),
        &trusted,
        &BTreeSet::new(),
    )
    .expect_err("tampered signature must fail");
    assert_eq!(err.kind, SignatureErrorKind::Invalid);
}

#[test]
fn full_verify_path_rejects_revoked_signer() {
    let f = fixtures();
    let contents = parse_archive_bytes(archive_bytes(&f)).expect("parse archive");
    let trusted = trusted_from_fixture(&f);
    let mut revocations = BTreeSet::new();
    revocations.insert(f_str(&f, "signer_id"));
    let err = verify_archive_signature(
        &contents.payload_hash,
        &f_str(&f, "signature_b64"),
        &f_str(&f, "signer_id"),
        &trusted,
        &revocations,
    )
    .expect_err("revoked signer must fail");
    assert_eq!(err.kind, SignatureErrorKind::Revoked);
}

#[test]
fn full_verify_path_rejects_unknown_signer() {
    let f = fixtures();
    let contents = parse_archive_bytes(archive_bytes(&f)).expect("parse archive");
    // Empty trusted-keys store -> the signer is unknown.
    let err = verify_archive_signature(
        &contents.payload_hash,
        &f_str(&f, "signature_b64"),
        &f_str(&f, "signer_id"),
        &BTreeMap::new(),
        &BTreeSet::new(),
    )
    .expect_err("unknown signer must fail");
    assert_eq!(err.kind, SignatureErrorKind::UnknownSigner);
}
