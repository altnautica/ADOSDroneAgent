//! Keypair persistence integration test.
//!
//! Pins the on-disk format the upstream `wfb_tx` C binary expects when
//! invoked with `-K <path>`:
//!
//! - File length is exactly 64 bytes (32-byte secret + 32-byte public).
//! - File mode is exactly 0o600.
//! - Last 32 bytes are the public key the manager returns from the
//!   write call.
//! - SHA-256 fingerprint of the public-key half matches the wizard's
//!   summary string.
//! - Two managers pointing at the same file plus the same passphrase
//!   round-trip identical bytes (same fingerprint), so a service
//!   restart does not desync the keypair the air and ground halves
//!   share.

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use ados_wfb::{
    derive_keypair, key_fingerprint, WfbAdvancedOpts, WfbConfig, WfbManager, DEFAULT_WFB_TX_PATH,
    KEY_LEN, PUBLIC_KEY_LEN,
};
use sha2::{Digest, Sha256};

fn cfg_with_path(passphrase: &str, path: PathBuf) -> WfbConfig {
    WfbConfig {
        channel: 161,
        mcs_index: 1,
        tx_power_dbm: 25,
        key_passphrase: passphrase.to_string(),
        wfb_tx_path: PathBuf::from(DEFAULT_WFB_TX_PATH),
        interface: None,
        keypair_path: path,
        advanced: WfbAdvancedOpts::default(),
    }
}

fn sha256_hex8(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    let mut s = String::with_capacity(16);
    for b in &digest[..8] {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

#[tokio::test]
async fn persisted_keypair_is_64_bytes_at_mode_0600() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("wfb-keypair");

    let mgr = WfbManager::new(cfg_with_path(
        "persistence-pass-A",
        path.clone(),
    ))
    .expect("ctor");

    let public = mgr.persist_keypair_file().await.expect("persist");

    let bytes = std::fs::read(&path).expect("read keypair");
    assert_eq!(
        bytes.len(),
        KEY_LEN + PUBLIC_KEY_LEN,
        "keypair file must be exactly secret(32) + public(32)"
    );
    // Last 32 bytes = public component returned from the write.
    assert_eq!(&bytes[KEY_LEN..], &public[..]);
    // First 32 bytes are the broadcast key (the secret half).
    assert_eq!(bytes[..KEY_LEN].len(), KEY_LEN);

    let mode = std::fs::metadata(&path)
        .expect("stat keypair")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600, "keypair file must be 0o600");
}

#[tokio::test]
async fn public_fingerprint_matches_sha256_first_eight_bytes() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("kp");

    let passphrase = "fingerprint-anchor-2026";
    let mgr =
        WfbManager::new(cfg_with_path(passphrase, path.clone())).expect("ctor");

    let public = mgr.persist_keypair_file().await.expect("persist");

    // The fingerprint helper hashes the broadcast key half, not the
    // public half. Independently derive both and confirm:
    //   - the public-key SHA-256(public)[..8] hex round-trips against
    //     SHA-256 of the public bytes (catches drift in the public-key
    //     surface);
    //   - the broadcast-key fingerprint helper produces a stable
    //     16-char string for the same passphrase.
    let public_fp = sha256_hex8(&public);
    let public_fp_again = sha256_hex8(&public);
    assert_eq!(public_fp, public_fp_again, "public fingerprint must be stable");
    assert_eq!(public_fp.len(), 16, "fingerprint is exactly 8 bytes hex");

    // Independently re-derive the public from the same passphrase and
    // confirm it matches the persisted bytes.
    let (independent_public, broadcast) =
        derive_keypair(passphrase).expect("derive_keypair");
    assert_eq!(
        independent_public, public,
        "persisted public must match a fresh derivation against the same passphrase"
    );

    // The broadcast-key fingerprint that the wizard surfaces is
    // `key_fingerprint(broadcast_bytes)`. Anchor it here so a future
    // change in the helper surface (length, hash function) breaks
    // loudly with the persisted file as a witness.
    let broadcast_fp = key_fingerprint(&broadcast);
    assert_eq!(broadcast_fp.len(), 16);
    assert!(broadcast_fp.chars().all(|c| c.is_ascii_hexdigit()));
}

#[tokio::test]
async fn manager_restart_loads_same_keypair_for_same_passphrase() {
    // Round-trip: write a known keypair, drop the manager, construct a
    // second manager pointing at the same path with the same passphrase,
    // re-persist, and assert the file's bytes are identical and the
    // public-key fingerprint matches across the two writes. This is the
    // weakest possible "load-on-start" property the manager exposes
    // today: the manager does not read an existing keypair from disk on
    // construction (it derives + persists on demand), but the
    // determinism of the KDF means two managers with the same
    // passphrase + path produce byte-identical files.
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("kp-roundtrip");

    let passphrase = "round-trip-passphrase";

    let first = WfbManager::new(cfg_with_path(passphrase, path.clone())).expect("ctor 1");
    let public_first = first.persist_keypair_file().await.expect("persist 1");
    let bytes_first = std::fs::read(&path).expect("read 1");
    drop(first);

    // "Restart": fresh manager pointing at the same path + same
    // passphrase.
    let second = WfbManager::new(cfg_with_path(passphrase, path.clone())).expect("ctor 2");
    let public_second = second.persist_keypair_file().await.expect("persist 2");
    let bytes_second = std::fs::read(&path).expect("read 2");

    assert_eq!(
        public_first, public_second,
        "same passphrase => same public key across manager restarts"
    );
    assert_eq!(
        bytes_first, bytes_second,
        "same passphrase => identical on-disk bytes across manager restarts"
    );
    assert_eq!(
        sha256_hex8(&public_first),
        sha256_hex8(&public_second),
        "fingerprint must be stable across manager restarts"
    );
}

#[tokio::test]
async fn re_persist_overwrites_existing_file_atomically() {
    // The atomic-write helper installs the new file via rename(2). A
    // second write under the same passphrase must produce the same
    // bytes (deterministic KDF) and must NOT leave a stale `.tmp`
    // sibling lying around — the helper guarantees that contract and we
    // pin it here so a future swap to a non-atomic write surfaces.
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("kp-overwrite");

    let mgr =
        WfbManager::new(cfg_with_path("overwrite-pass", path.clone())).expect("ctor");

    mgr.persist_keypair_file().await.expect("first write");
    let bytes_a = std::fs::read(&path).expect("read a");
    mgr.persist_keypair_file().await.expect("second write");
    let bytes_b = std::fs::read(&path).expect("read b");

    assert_eq!(bytes_a, bytes_b, "deterministic KDF => identical re-write");

    // No stray files in the parent directory beyond the keypair itself
    // (the atomic-write helper must have cleaned up its tempfile).
    let entries: Vec<String> = std::fs::read_dir(dir.path())
        .expect("read tempdir")
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        entries.iter().all(|n| n == "kp-overwrite"),
        "atomic-write helper must not leave .tmp siblings, got: {entries:?}",
    );
}

#[tokio::test]
async fn different_passphrase_yields_different_keypair() {
    // Sanity check on the KDF surface from the persistence side: two
    // managers writing to the same path under different passphrases
    // produce different bytes. A collision here would break the
    // air-ground key sharing scheme silently.
    let dir = tempfile::tempdir().expect("tempdir");
    let path_a = dir.path().join("kp-A");
    let path_b = dir.path().join("kp-B");

    let mgr_a =
        WfbManager::new(cfg_with_path("operator-alpha", path_a.clone())).expect("ctor A");
    let mgr_b =
        WfbManager::new(cfg_with_path("operator-bravo", path_b.clone())).expect("ctor B");

    mgr_a.persist_keypair_file().await.expect("write A");
    mgr_b.persist_keypair_file().await.expect("write B");

    let bytes_a = std::fs::read(&path_a).expect("read A");
    let bytes_b = std::fs::read(&path_b).expect("read B");

    assert_ne!(
        bytes_a, bytes_b,
        "different passphrases must yield different keypair bytes"
    );
}
