use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use ados_config::capture_storage::{FsSpace, GIB};

use super::*;

/// Reads `<mount>/.fake-space` (`<total> <free>`), the stand-in for statvfs.
fn fake_stat(mount: &Path) -> Option<FsSpace> {
    let text = fs::read_to_string(mount.join(".fake-space")).ok()?;
    let mut parts = text.split_whitespace().map(|p| p.parse::<u64>().ok());
    Some(FsSpace {
        total_bytes: parts.next()??,
        free_bytes: parts.next()??,
    })
}

/// A capture volume mounted at `<tmp>/mnt` with the given space; the capture
/// root is an explicit directory on it.
fn fixture(total: u64, free: u64) -> (tempfile::TempDir, SysRoots, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path();
    let mount = base.join("mnt");
    fs::create_dir_all(base.join("sys/block/nvme0n1/nvme0n1p1")).unwrap();
    fs::create_dir_all(base.join("sys/block/nvme0n1/queue")).unwrap();
    fs::write(base.join("sys/block/nvme0n1/queue/rotational"), "0\n").unwrap();
    fs::create_dir_all(&mount).unwrap();
    fs::write(mount.join(".fake-space"), format!("{total} {free}")).unwrap();
    fs::write(
        base.join("mounts"),
        format!("/dev/nvme0n1p1 {} ext4 rw 0 0\n", mount.display()),
    )
    .unwrap();
    let sys = SysRoots {
        sys_block: base.join("sys/block"),
        mounts: base.join("mounts"),
        fallback_root: base.join("fallback"),
        sticky_file: base.join("capture-root"),
        stat: fake_stat,
    };
    let root = mount.join("ados/capture");
    fs::create_dir_all(&root).unwrap();
    (tmp, sys, root)
}

fn cfg_for(root: &Path, evict_unuploaded: bool) -> CaptureStorageConfig {
    CaptureStorageConfig {
        capture_root: root.display().to_string(),
        capture_evict_unuploaded: evict_unuploaded,
        ..CaptureStorageConfig::default()
    }
}

/// A session of `bytes` with the given markers. A recording session has no
/// manifest yet, as the recorder writes it at finish.
fn session(root: &Path, name: &str, bytes: usize, uploaded: bool, recording: bool) {
    let dir = root.join(name);
    fs::create_dir_all(dir.join("frames")).unwrap();
    if !recording {
        fs::write(dir.join(cs::MANIFEST_NAME), b"{}").unwrap();
    }
    fs::write(dir.join("frames/data.bin"), vec![7u8; bytes]).unwrap();
    if uploaded {
        fs::write(dir.join(".uploaded"), b"").unwrap();
    }
    if recording {
        fs::write(dir.join(".recording"), b"").unwrap();
    }
}

/// The pass clock: `mono_ns` as given, wall time now plus `ahead_s`, and a
/// capture-active marker that does not exist.
fn at(mono_ns: u64, ahead_s: i64) -> PassTime {
    PassTime {
        mono_ns,
        live: Liveness {
            now_unix: PassTime::now().live.now_unix + ahead_s,
            capture_marker: PathBuf::from("/nonexistent/ados-test/capture/active"),
        },
    }
}

/// `run_pass` at the current wall time with a no-op keepalive.
fn run(
    cfg: &CaptureStorageConfig,
    sys: &SysRoots,
    mode: PassMode,
    probe: Option<WriteProbe>,
    cache: &mut SizeCache,
    now_ns: u64,
) -> CapturePass {
    run_pass(cfg, sys, mode, probe, cache, at(now_ns, 0), &|| {})
}

fn s(name: &str, bytes: u64, uploaded: bool, recording: bool) -> Session {
    Session {
        name: name.into(),
        born: 0,
        bytes,
        uploaded,
        recording,
        active: recording,
    }
}

#[test]
fn only_drone_and_visor_run_capture() {
    assert!(active_for("drone"));
    assert!(active_for("visor"));
    for p in ["ground-station", "workstation", "compute"] {
        assert!(!active_for(p), "{p}");
    }
}

#[test]
fn eviction_order_uploaded_oldest_first() {
    let sessions = [
        s("a", 100, false, false),
        s("b", 100, true, false),
        s("c", 100, true, true),
        s("d", 100, true, false),
        s("e", 100, false, false),
    ];
    // Uploaded first, oldest first; the recording one never.
    assert_eq!(plan_evictions(&sessions, 150, false), [1, 3]);
    // Un-uploaded kept without the flag, even when more is needed.
    assert_eq!(plan_evictions(&sessions, 10_000, false), [1, 3]);
    // With the flag, un-uploaded follow, oldest first; still never the active.
    assert_eq!(plan_evictions(&sessions, 10_000, true), [1, 3, 0, 4]);
    assert_eq!(plan_evictions(&sessions, 250, true), [1, 3, 0]);
    assert!(plan_evictions(&sessions, 0, true).is_empty());

    // A crashed session (`.recording`, silent past the window) goes with the
    // un-uploaded ones, only under the flag, and never when it is uploaded.
    let crashed = |name: &str, uploaded| Session {
        active: false,
        ..s(name, 100, uploaded, true)
    };
    let sessions = [
        crashed("x", false),
        s("y", 100, true, false),
        crashed("z", true),
    ];
    assert_eq!(plan_evictions(&sessions, 10_000, false), [1]);
    assert_eq!(plan_evictions(&sessions, 10_000, true), [1, 0]);
}

#[test]
fn recording_sessions_are_counted_and_judged_by_their_newest_write() {
    let (_tmp, sys, root) = fixture(100 * GIB, 800 * GIB);
    session(&root, "live", 1000, false, true);
    session(&root, "done", 1000, false, false);
    fs::create_dir_all(root.join("unmarked")).unwrap();
    fs::write(root.join("unmarked/x.bin"), vec![0u8; 4000]).unwrap();

    let cfg = cfg_for(&root, true);
    let now = run(
        &cfg,
        &sys,
        PassMode::Refresh,
        None,
        &mut SizeCache::new(),
        1,
    );
    assert_eq!(
        now.sidecar.capture.sessions, 2,
        "in-progress counted, unmarked not"
    );
    assert!(now.sidecar.capture.used_bytes >= 2000);
    assert!(now.sidecar.capture.used_bytes < 4000);

    let sessions = scan_sessions(&root, &mut SizeCache::new(), &at(0, 0).live, &|| {});
    let live = sessions.iter().find(|s| s.name == "live").unwrap();
    assert!(live.recording && live.active);
    let later = at(0, ACTIVE_WINDOW_S + 60).live;
    let sessions = scan_sessions(&root, &mut SizeCache::new(), &later, &|| {});
    let live = sessions.iter().find(|s| s.name == "live").unwrap();
    assert!(
        live.recording && !live.active,
        "silent past the window: crashed"
    );
}

#[test]
fn an_in_progress_session_is_never_evicted() {
    // Under the floor, eviction of un-uploaded sessions allowed.
    let (_tmp, sys, root) = fixture(100 * GIB, GIB);
    session(&root, "live", 1000, false, true);
    let pass = run(
        &cfg_for(&root, true),
        &sys,
        PassMode::Evict,
        None,
        &mut SizeCache::new(),
        1,
    );
    assert_eq!(pass.freed, 0);
    assert!(root.join("live").exists());
    assert_eq!(pass.sidecar.capture.sessions, 1);
}

#[test]
fn a_crashed_session_is_counted_and_evicted_only_with_the_flag() {
    let (_tmp, sys, root) = fixture(100 * GIB, GIB);
    session(&root, "crashed", 1000, false, true);
    let silent = at(1, ACTIVE_WINDOW_S + 60);

    // Flag off: counted, kept.
    let kept = run_pass(
        &cfg_for(&root, false),
        &sys,
        PassMode::Evict,
        None,
        &mut SizeCache::new(),
        silent.clone(),
        &|| {},
    );
    assert_eq!(kept.freed, 0);
    assert!(root.join("crashed").exists());
    assert_eq!(kept.sidecar.capture.sessions, 1);
    assert!(kept.sidecar.capture.used_bytes >= 1000);

    // Flag on: evicted with the un-uploaded sessions.
    let gone = run_pass(
        &cfg_for(&root, true),
        &sys,
        PassMode::Evict,
        None,
        &mut SizeCache::new(),
        silent,
        &|| {},
    );
    assert!(gone.freed >= 1000);
    assert!(!root.join("crashed").exists());
    assert_eq!(gone.sidecar.capture.sessions, 0);
}

#[test]
fn a_running_capture_keeps_every_recording_session() {
    let (tmp, sys, root) = fixture(100 * GIB, GIB);
    session(&root, "paused", 1000, false, true);
    // Silent for 20 minutes, eviction of un-uploaded sessions allowed.
    let mut silent = at(1, 20 * 60);
    let marker = tmp.path().join("run/capture/active");
    fs::create_dir_all(marker.parent().unwrap()).unwrap();
    fs::write(&marker, b"").unwrap();
    silent.live.capture_marker = marker.clone();

    let kept = run_pass(
        &cfg_for(&root, true),
        &sys,
        PassMode::Evict,
        None,
        &mut SizeCache::new(),
        silent.clone(),
        &|| {},
    );
    assert_eq!(kept.freed, 0);
    assert!(root.join("paused").exists(), "a capture is running");

    fs::remove_file(&marker).unwrap();
    let gone = run_pass(
        &cfg_for(&root, true),
        &sys,
        PassMode::Evict,
        None,
        &mut SizeCache::new(),
        silent,
        &|| {},
    );
    assert!(gone.freed >= 1000);
    assert!(!root.join("paused").exists(), "no capture running: crashed");
}

#[test]
fn excess_is_the_larger_of_overrun_and_floor_shortfall() {
    assert_eq!(excess_bytes(100, 80, Some(50), 10), 20);
    assert_eq!(excess_bytes(100, 80, Some(5), 40), 35);
    assert_eq!(excess_bytes(100, 200, Some(50), 10), 0);
    assert_eq!(
        excess_bytes(100, 0, None, 10),
        0,
        "unknown figures never evict"
    );
}

#[test]
fn a_pass_evicts_uploaded_sessions_first_and_keeps_the_active_one() {
    // 4.9 GiB free under the 5 GiB floor: ~0.1 GiB must come off.
    let (_tmp, sys, root) = fixture(100 * GIB, 5 * GIB - GIB / 10);
    session(&root, "s1-old-unuploaded", 1000, false, false);
    session(&root, "s2-uploaded-recording", 1000, true, true);
    session(&root, "s3-uploaded", 1000, true, false);
    session(&root, "s4-uploaded", 1000, true, false);
    session(&root, "s5-active", 1000, false, true);
    fs::write(root.join(".write-probe"), b"x").unwrap();

    let ticks = std::cell::Cell::new(0u32);
    let pass = run_pass(
        &cfg_for(&root, false),
        &sys,
        PassMode::Evict,
        None,
        &mut SizeCache::new(),
        at(42, 0),
        &|| ticks.set(ticks.get() + 1),
    );
    assert!(
        ticks.get() >= 2 + 5,
        "progress is reported per measured and per removed session: {}",
        ticks.get()
    );
    assert!(pass.freed >= 2000, "{}", pass.freed);
    assert!(!root.join("s3-uploaded").exists());
    assert!(!root.join("s4-uploaded").exists());
    assert!(root.join("s1-old-unuploaded").exists(), "un-uploaded kept");
    assert!(
        root.join("s2-uploaded-recording").exists(),
        "recording kept"
    );
    assert!(root.join("s5-active").exists());
    assert_eq!(pass.sidecar.capture.sessions, 3);
    assert_eq!(pass.sidecar.capture.uploaded, 1);
    // The fake volume does not gain space from deletions: still under the floor.
    assert_eq!(pass.sidecar.capture.state, CaptureState::Full);

    // With the flag the un-uploaded, idle session goes too.
    let pass = run(
        &cfg_for(&root, true),
        &sys,
        PassMode::Evict,
        None,
        &mut SizeCache::new(),
        43,
    );
    assert!(!root.join("s1-old-unuploaded").exists());
    assert!(root.join("s2-uploaded-recording").exists());
    assert!(root.join("s5-active").exists());
    assert_eq!(pass.sidecar.capture.sessions, 2);
}

mod removal;
mod sidecar;
