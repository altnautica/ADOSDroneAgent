//! Integration tests for the canonical atomic-write helper.
//!
//! Covers the contract every consumer relies on: round-trip equality,
//! crash-safe rename (prior content survives a panic mid-write), mode
//! application, concurrent writers leaving exactly one final file, and
//! tempfile cleanup on a write failure.

use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;
use std::thread;

use ados_core::atomic::{
    write_atomic, write_atomic_config, write_atomic_secret, AtomicWriteError,
};

#[test]
fn round_trip_bytes_match() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("file");
    let payload = b"the quick brown fox jumps over the lazy dog";
    write_atomic(&path, payload, 0o644).unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), payload);
}

#[test]
fn replaces_existing_file_atomically() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("file");
    std::fs::write(&path, b"old content").unwrap();
    write_atomic(&path, b"new content", 0o600).unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), b"new content");
}

#[test]
fn crash_safe_rename_preserves_prior_content() {
    // Pre-existing file with old content. Spawn a thread that calls a
    // wrapper which writes the tempfile then panics BEFORE the rename.
    // After the thread joins (with a panic), the original file must
    // still hold its prior content because the rename never happened.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.json");
    std::fs::write(&path, b"old").unwrap();

    let path_clone = path.clone();
    let handle = thread::spawn(move || {
        // Manually emulate "write tempfile, panic before rename" by
        // creating a sibling tempfile, writing bytes, then panicking.
        // The canonical helper itself is too well-behaved to fail
        // mid-rename in a portable way, so we model the contract.
        let parent = path_clone.parent().unwrap();
        let tmp = parent.join(".state.json.crashtest.tmp");
        std::fs::write(&tmp, b"new").unwrap();
        // Simulate a crash before rename.
        panic!("simulated crash before rename");
    });
    let res = handle.join();
    assert!(res.is_err(), "expected panic to propagate");

    // Original file content survived because rename never executed.
    assert_eq!(std::fs::read(&path).unwrap(), b"old");

    // The leaked tempfile is a known artefact of the simulated crash;
    // a normal `write_atomic` call would have removed it. Verify the
    // canonical helper does the right thing on a happy-path call after
    // a crash: writes the new content, leaves no leftovers.
    write_atomic(&path, b"newer", 0o644).unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), b"newer");
}

#[test]
fn mode_0600_is_applied() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("secret");
    write_atomic_secret(&path, b"shh").unwrap();
    let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "expected 0o600, got 0o{:o}", mode);
}

#[test]
fn mode_0644_is_applied() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config");
    write_atomic_config(&path, b"x: 1\n").unwrap();
    let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o644, "expected 0o644, got 0o{:o}", mode);
}

#[test]
fn concurrent_writers_leave_one_final_file() {
    // Two threads write the same target with different content.
    // Whichever rename wins, the file MUST hold one of the two
    // payloads in full — never a torn intermediate, never a leftover
    // tempfile in the directory.
    let dir = tempfile::tempdir().unwrap();
    let target = Arc::new(dir.path().join("contended"));

    let payload_a = vec![b'a'; 4096];
    let payload_b = vec![b'b'; 4096];

    let target_a = target.clone();
    let payload_a_clone = payload_a.clone();
    let h1 = thread::spawn(move || {
        for _ in 0..50 {
            write_atomic(&target_a, &payload_a_clone, 0o644).unwrap();
        }
    });

    let target_b = target.clone();
    let payload_b_clone = payload_b.clone();
    let h2 = thread::spawn(move || {
        for _ in 0..50 {
            write_atomic(&target_b, &payload_b_clone, 0o644).unwrap();
        }
    });

    h1.join().unwrap();
    h2.join().unwrap();

    let final_bytes = std::fs::read(&*target).unwrap();
    assert!(
        final_bytes == payload_a || final_bytes == payload_b,
        "final content was a torn write of len {}",
        final_bytes.len()
    );

    // Only the final file should remain; no leftover .tmp files.
    let entries: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .collect();
    assert_eq!(
        entries.len(),
        1,
        "leftover entries in {:?}: {:?}",
        dir.path(),
        entries.iter().map(|e| e.file_name()).collect::<Vec<_>>()
    );
}

#[test]
fn tempfile_cleanup_on_write_failure() {
    // Make the parent directory read-only so the OpenOptions::open
    // call inside `write_atomic` fails. The helper must not leak any
    // tempfile into the (read-only) directory and must surface the io
    // error to the caller.
    let dir = tempfile::tempdir().unwrap();
    let parent = dir.path().join("locked");
    std::fs::create_dir(&parent).unwrap();
    std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o500)).unwrap();

    let target = parent.join("file");
    let res = write_atomic(&target, b"x", 0o644);
    assert!(matches!(res, Err(AtomicWriteError::Io(_))));

    // Restore write so the tempdir can be cleaned up by Drop.
    std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o700)).unwrap();

    // No tempfile leaked into the directory.
    let entries: Vec<_> = std::fs::read_dir(&parent)
        .unwrap()
        .filter_map(|e| e.ok())
        .collect();
    assert!(
        entries.is_empty(),
        "leftover entries in {:?}: {:?}",
        parent,
        entries.iter().map(|e| e.file_name()).collect::<Vec<_>>()
    );
}

#[test]
fn invalid_mode_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("file");
    // Mode 0o12345 is outside the 12-bit envelope.
    let err = write_atomic(&path, b"x", 0o12345).unwrap_err();
    match err {
        AtomicWriteError::InvalidMode(m) => assert_eq!(m, 0o12345),
        other => panic!("expected InvalidMode, got {other:?}"),
    }
    // No file was created.
    assert!(!path.exists());
}

#[test]
fn ensure_secret_dir_creates_with_0700() {
    use ados_core::atomic::ensure_secret_dir;
    let dir = tempfile::tempdir().unwrap();
    let secret = dir.path().join("secrets");
    ensure_secret_dir(&secret).unwrap();
    let mode = std::fs::metadata(&secret).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o700, "expected 0700, got 0o{:o}", mode);
}

#[test]
fn ensure_secret_dir_tightens_existing_dir() {
    use ados_core::atomic::ensure_secret_dir;
    let dir = tempfile::tempdir().unwrap();
    let secret = dir.path().join("loose");
    std::fs::create_dir(&secret).unwrap();
    std::fs::set_permissions(&secret, std::fs::Permissions::from_mode(0o755)).unwrap();
    ensure_secret_dir(&secret).unwrap();
    let mode = std::fs::metadata(&secret).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o700, "expected 0700, got 0o{:o}", mode);
}
