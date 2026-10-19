use std::cell::RefCell;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::PathBuf;

use super::*;

fn mode_of(path: &Path) -> u32 {
    std::fs::metadata(path).unwrap().permissions().mode() & 0o7777
}

/// The gid every file this test process creates carries.
fn own_gid(dir: &Path) -> u32 {
    std::fs::metadata(dir).unwrap().gid()
}

#[test]
fn a_new_root_is_created_setgid_group_writable_and_handed_to_the_group() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("ados/capture");
    // A gid the fresh directory does not carry, so the seam is called even
    // when the test itself runs as root.
    let gid = own_gid(tmp.path()).wrapping_add(1);
    let calls: RefCell<Vec<(PathBuf, u32)>> = RefCell::new(Vec::new());
    let chown = |p: &Path, g: u32| {
        calls.borrow_mut().push((p.to_path_buf(), g));
        Ok(())
    };
    let fix = prepare_capture_root(&root, Some(gid), &chown).unwrap();
    assert!(fix.chmodded);
    assert_eq!(mode_of(&root), CAPTURE_ROOT_MODE);
    // The directory does not carry the capture gid, so it is handed over
    // through the seam.
    assert!(fix.chowned);
    assert_eq!(calls.borrow().as_slice(), &[(root.clone(), gid)]);

    // A second pass finds the mode already right.
    let again = prepare_capture_root(&root, Some(gid), &chown).unwrap();
    assert!(!again.chmodded);
}

#[test]
fn an_existing_root_with_the_wrong_mode_is_corrected_but_its_sessions_are_not() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("capture");
    let session = root.join("20260101T000000Z_abcd");
    std::fs::create_dir_all(&session).unwrap();
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::set_permissions(&session, std::fs::Permissions::from_mode(0o750)).unwrap();
    let gid = own_gid(tmp.path());
    let fix = prepare_capture_root(&root, Some(gid), &|_, _| Ok(())).unwrap();
    assert!(fix.chmodded);
    assert_eq!(mode_of(&root), CAPTURE_ROOT_MODE);
    assert_eq!(mode_of(&session), 0o750, "sessions keep their own mode");
}

#[test]
fn without_the_group_the_root_is_only_created() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("capture");
    let fix =
        prepare_capture_root(&root, None, &|_, _| panic!("no chown without a group")).unwrap();
    assert_eq!(fix, RootFix::default());
    assert!(root.is_dir());
    assert_eq!(mode_of(&root) & 0o2000, 0);
}

#[test]
fn a_failed_chown_is_reported() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("capture");
    let err = prepare_capture_root(&root, Some(own_gid(tmp.path())), &|_, _| {
        Err(io::Error::from(io::ErrorKind::PermissionDenied))
    })
    .unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
}

/// What the applied mode grants, decided on the bits alone: a peer that is
/// neither the owner nor "other"-privileged but carries the group gets
/// read, write and search, and what it creates inherits the group (setgid).
#[test]
fn the_applied_mode_admits_a_group_member_to_create_sessions() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("capture");
    prepare_capture_root(&root, Some(own_gid(tmp.path())), &|_, _| Ok(())).unwrap();
    let mode = mode_of(&root);
    assert_eq!(mode & 0o070, 0o070, "group rwx");
    assert_eq!(mode & 0o2000, 0o2000, "setgid");
    assert_eq!(mode & 0o1000, 0o1000, "sticky");
    assert_eq!(mode & 0o002, 0, "others may not write");
    // Setgid in effect: a session made here carries the root's group.
    let session = root.join("s1");
    std::fs::create_dir(&session).unwrap();
    assert_eq!(
        std::fs::metadata(&session).unwrap().gid(),
        std::fs::metadata(&root).unwrap().gid()
    );
}

#[test]
fn a_refused_root_is_neither_created_nor_handed_to_the_group() {
    for root in [
        "/etc",
        "/etc/ados/capture",
        "/var/ados",
        "/var/lib/ados/c",
        "rel/capture",
    ] {
        let chown = |_: &Path, _: u32| -> io::Result<()> { panic!("no chown for {root}") };
        let path = Path::new(root);
        let err = prepare_capture_root(path, Some(0), &chown).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied, "{root}");
    }
}

/// The real permission check, with a second identity. Needs root, so it is
/// ignored by default; run the built test binary under sudo:
/// `sudo <ados_config test binary> --ignored capture_group_member`.
/// A process running as `nobody` whose only extra credential is the capture
/// gid creates a session directory under the prepared root; the same process
/// without that gid is refused.
#[cfg(target_os = "linux")]
#[test]
#[ignore = "needs root: run the test binary under sudo with --ignored"]
fn capture_group_member_creates_a_session_and_an_outsider_cannot() {
    assert!(nix::unistd::geteuid().is_root(), "run this test under sudo");
    // Any gid works: the kernel compares numbers, no group entry is needed.
    const GID: u32 = 47_311;
    let tmp = tempfile::tempdir().unwrap();
    std::fs::set_permissions(tmp.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
    let root = tmp.path().join("capture");
    let fix = prepare_capture_root(&root, Some(GID), &chown_to_root).unwrap();
    assert!(fix.chowned && fix.chmodded);
    let meta = std::fs::metadata(&root).unwrap();
    assert_eq!((meta.uid(), meta.gid()), (0, GID));
    assert_eq!(mode_of(&root), CAPTURE_ROOT_MODE);

    let mkdir_as_nobody = |dir: &Path, groups: &str| {
        std::process::Command::new("setpriv")
            .args(["--reuid=nobody", "--regid=nogroup", groups, "mkdir"])
            .arg(dir)
            .status()
            .unwrap()
            .success()
    };
    let member = root.join("member_session");
    assert!(mkdir_as_nobody(&member, &format!("--groups={GID}")));
    assert_eq!(std::fs::metadata(&member).unwrap().gid(), GID, "setgid");
    assert!(!mkdir_as_nobody(&root.join("outsider"), "--clear-groups"));
    assert!(!root.join("outsider").exists());
}
