//! What a pass deletes, and how: only sessions, through the tree, never
//! through a link.

use std::os::unix::fs::symlink;

use super::*;

#[test]
fn a_pass_without_eviction_deletes_nothing() {
    let (_tmp, sys, root) = fixture(100 * GIB, GIB);
    session(&root, "s1", 1000, true, false);
    for mode in [PassMode::Refresh, PassMode::Remeasure] {
        let pass = run(
            &cfg_for(&root, true),
            &sys,
            mode,
            None,
            &mut SizeCache::new(),
            1,
        );
        assert_eq!(pass.freed, 0);
        assert!(root.join("s1").exists());
    }
}

#[test]
fn only_directories_with_a_manifest_are_sessions() {
    let (_tmp, sys, root) = fixture(100 * GIB, GIB);
    session(&root, "s1", 1000, true, false);
    fs::create_dir_all(root.join("not-a-session/frames")).unwrap();
    fs::write(root.join("not-a-session/frames/x.bin"), vec![0u8; 5000]).unwrap();
    fs::create_dir_all(root.join("uploaded-no-manifest")).unwrap();
    fs::write(root.join("uploaded-no-manifest/.uploaded"), b"").unwrap();
    fs::write(root.join("stray.bin"), vec![0u8; 100]).unwrap();
    let pass = run(
        &cfg_for(&root, true),
        &sys,
        PassMode::Evict,
        None,
        &mut SizeCache::new(),
        1,
    );
    assert_eq!(pass.sidecar.capture.sessions, 0, "s1 was evicted");
    assert!(!root.join("s1").exists());
    assert!(
        pass.sidecar.capture.used_bytes < 5000,
        "foreign bytes are not counted"
    );
    assert!(
        root.join("not-a-session").exists(),
        "never the janitor's to delete"
    );
    assert!(root.join("uploaded-no-manifest").exists());
    assert!(root.join("stray.bin").exists());
}

/// The descriptor walk reports progress inside a session; off Linux the whole
/// session is one `remove_dir_all`.
#[cfg(target_os = "linux")]
#[test]
fn a_large_session_is_removed_with_progress_inside_it() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    session(root, "big", 10, true, false);
    for i in 0..600 {
        fs::write(root.join(format!("big/frames/f{i}.bin")), b"x").unwrap();
    }
    let now = at(0, 0).live;
    let s = scan_sessions(root, &mut SizeCache::new(), &now, &|| {}).remove(0);
    let ticks = std::cell::Cell::new(0u32);
    let freed = remove_session(root, &s, &now, &|| ticks.set(ticks.get() + 1));
    assert_eq!(freed, s.bytes);
    assert!(!root.join("big").exists());
    assert!(ticks.get() >= 2, "{}", ticks.get());
}

/// A directory outside the capture root that a symlink may point at, with a
/// file at its top and one level down.
fn precious(base: &Path) -> PathBuf {
    let target = base.join("precious");
    fs::create_dir_all(target.join("sub")).unwrap();
    fs::write(target.join("keep.txt"), b"x").unwrap();
    fs::write(target.join("sub/keep.txt"), b"x").unwrap();
    target
}

fn assert_intact(target: &Path) {
    assert!(target.join("keep.txt").exists());
    assert!(target.join("sub/keep.txt").exists());
}

#[test]
fn a_session_swapped_for_a_symlink_after_the_scan_is_not_followed() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("capture");
    let target = precious(tmp.path());
    session(&root, "s1", 10, true, false);
    let now = at(0, 0).live;
    let s = scan_sessions(&root, &mut SizeCache::new(), &now, &|| {}).remove(0);

    // The writer that owns the session renames it away and links its name to
    // a directory it wants emptied.
    fs::rename(root.join("s1"), tmp.path().join("moved")).unwrap();
    symlink(&target, root.join("s1")).unwrap();

    assert_eq!(remove_session(&root, &s, &now, &|| {}), 0);
    assert_intact(&target);
    let link = root.join("s1").symlink_metadata().unwrap();
    assert!(link.file_type().is_symlink(), "the link is left alone");
    assert!(tmp.path().join("moved/frames/data.bin").exists());
}

#[test]
fn links_inside_a_session_are_removed_never_followed() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("capture");
    let target = precious(tmp.path());
    session(&root, "s1", 10, true, false);
    symlink(&target, root.join("s1/frames/dir-link")).unwrap();
    symlink(target.join("keep.txt"), root.join("s1/file-link")).unwrap();
    let now = at(0, 0).live;
    let s = scan_sessions(&root, &mut SizeCache::new(), &now, &|| {}).remove(0);

    assert_eq!(remove_session(&root, &s, &now, &|| {}), s.bytes);
    assert!(!root.join("s1").exists());
    assert_intact(&target);
}
