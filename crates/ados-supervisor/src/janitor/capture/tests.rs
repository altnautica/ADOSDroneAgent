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

/// A session of `bytes` with the given markers.
fn session(root: &Path, name: &str, bytes: usize, uploaded: bool, recording: bool) {
    let dir = root.join(name);
    fs::create_dir_all(dir.join("frames")).unwrap();
    fs::write(dir.join(cs::MANIFEST_NAME), b"{}").unwrap();
    fs::write(dir.join("frames/data.bin"), vec![7u8; bytes]).unwrap();
    if uploaded {
        fs::write(dir.join(".uploaded"), b"").unwrap();
    }
    if recording {
        fs::write(dir.join(".recording"), b"").unwrap();
    }
}

/// `run_pass` with a no-op keepalive.
fn run(
    cfg: &CaptureStorageConfig,
    sys: &SysRoots,
    mode: PassMode,
    probe: Option<WriteProbe>,
    cache: &mut SizeCache,
    now_ns: u64,
) -> CapturePass {
    run_pass(cfg, sys, mode, probe, cache, now_ns, &|| {})
}

fn s(name: &str, bytes: u64, uploaded: bool, recording: bool) -> Session {
    Session {
        name: name.into(),
        born: 0,
        bytes,
        uploaded,
        recording,
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
        42,
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

#[test]
fn a_large_session_is_removed_with_progress_inside_it() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    session(root, "big", 10, true, false);
    for i in 0..600 {
        fs::write(root.join(format!("big/frames/f{i}.bin")), b"x").unwrap();
    }
    let s = scan_sessions(root, &mut SizeCache::new(), &|| {}).remove(0);
    let ticks = std::cell::Cell::new(0u32);
    let freed = remove_session(root, &s, &|| ticks.set(ticks.get() + 1));
    assert_eq!(freed, s.bytes);
    assert!(!root.join("big").exists());
    assert!(ticks.get() >= 2, "{}", ticks.get());
}

#[test]
fn the_capture_root_is_created_or_reported_full() {
    let (_tmp, sys, root) = fixture(1000 * GIB, 800 * GIB);
    let fresh = root.join("nested/capture");
    let pass = run(
        &cfg_for(&fresh, false),
        &sys,
        PassMode::Refresh,
        None,
        &mut SizeCache::new(),
        1,
    );
    assert!(fresh.is_dir(), "mkdir -p");
    assert!(pass.root_error.is_none());
    assert_eq!(pass.sidecar.capture.state, CaptureState::Ok);

    // A file where a parent directory should be: the root cannot be created.
    fs::write(root.join("blocker"), b"x").unwrap();
    let blocked = root.join("blocker/capture");
    let pass = run(
        &cfg_for(&blocked, false),
        &sys,
        PassMode::Refresh,
        None,
        &mut SizeCache::new(),
        2,
    );
    assert!(pass.root_error.is_some());
    assert_eq!(pass.sidecar.capture.state, CaptureState::Full);
    assert_eq!(pass.sidecar.capture_root, blocked.display().to_string());
}

#[test]
fn refresh_passes_reuse_settled_sizes_and_remeasure_the_recording_one() {
    let (_tmp, sys, root) = fixture(1000 * GIB, 800 * GIB);
    session(&root, "settled", 1000, true, false);
    session(&root, "live", 1000, false, true);
    let cfg = cfg_for(&root, false);
    let mut cache = SizeCache::new();
    let first = run(&cfg, &sys, PassMode::Refresh, None, &mut cache, 1);
    assert_eq!(cache.len(), 1, "only the settled session is cached");

    // Growth in the recording session shows on the next refresh; a change to
    // a settled one waits for the janitor-interval re-measure, which runs
    // whether or not eviction is enabled.
    fs::write(root.join("live/frames/more.bin"), vec![1u8; 500]).unwrap();
    fs::write(root.join("settled/frames/late.bin"), vec![1u8; 300]).unwrap();
    let second = run(&cfg, &sys, PassMode::Refresh, None, &mut cache, 2);
    assert_eq!(
        second.sidecar.capture.used_bytes,
        first.sidecar.capture.used_bytes + 500
    );
    let full = run(&cfg, &sys, PassMode::Remeasure, None, &mut cache, 3);
    assert_eq!(
        full.sidecar.capture.used_bytes,
        first.sidecar.capture.used_bytes + 800
    );

    fs::remove_dir_all(root.join("settled")).unwrap();
    run(&cfg, &sys, PassMode::Refresh, None, &mut cache, 4);
    assert!(cache.is_empty(), "a vanished session leaves the cache");
}

#[test]
fn a_roomy_volume_is_ok_and_untouched() {
    let (_tmp, sys, root) = fixture(1000 * GIB, 800 * GIB);
    session(&root, "s1", 1000, true, false);
    let pass = run(
        &cfg_for(&root, false),
        &sys,
        PassMode::Evict,
        None,
        &mut SizeCache::new(),
        7,
    );
    assert_eq!(pass.freed, 0);
    let c = &pass.sidecar.capture;
    assert_eq!(c.state, CaptureState::Ok);
    assert_eq!(c.budget_bytes, 850 * GIB);
    assert_eq!(c.floor_free_bytes, 5 * GIB);
    assert_eq!(c.budget_pct, 85);
    assert!(c.used_bytes >= 1000);
    let v = pass.sidecar.volume.as_ref().unwrap();
    assert_eq!(v.device, "nvme0n1p1");
    assert_eq!(pass.sidecar.updated_ns, 7);
}

#[test]
fn a_pass_writes_the_sidecar() {
    let (tmp, sys, root) = fixture(1000 * GIB, 800 * GIB);
    session(&root, "s1", 1000, true, false);
    let probe = WriteProbe {
        mbps: 1450.2,
        measured_at_ns: 9,
    };
    let pass = run(
        &cfg_for(&root, false),
        &sys,
        PassMode::Evict,
        Some(probe),
        &mut SizeCache::new(),
        5,
    );
    let path = tmp.path().join("run/capture-storage.json");
    write_sidecar(&path, &pass.sidecar).unwrap();
    let text = fs::read_to_string(&path).unwrap();
    assert!(text.starts_with("{\"version\":1,"), "{text}");
    let v: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(v["capture_root"], root.display().to_string());
    assert_eq!(v["capture"]["sessions"], 1);
    assert_eq!(v["capture"]["uploaded"], 1);
    assert_eq!(v["capture"]["state"], "ok");
    assert_eq!(v["volume"]["fs"], "ext4");
    assert_eq!(v["write_probe"]["mbps"], 1450.2);
    assert_eq!(v["write_probe"]["measured_at_ns"], 9);
}

#[test]
fn an_unmeasurable_volume_admits_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let cfg = cfg_for(tmp.path(), false);
    let sidecar = build_sidecar(&cfg, tmp.path(), None, &[], None, 0);
    assert!(sidecar.volume.is_none());
    assert_eq!(sidecar.capture.state, CaptureState::Full);
}

#[test]
fn the_write_probe_measures_and_cleans_up() {
    let tmp = tempfile::tempdir().unwrap();
    let mbps = measure_write(tmp.path(), 1024 * 1024).unwrap();
    assert!(mbps > 0.0);
    assert!(!tmp.path().join(".write-probe").exists());
    assert_eq!(measure_write(&tmp.path().join("absent"), 1024), None);
}

#[test]
fn probe_cadence() {
    let min = Duration::from_secs(60);
    assert!(!probe_due(Duration::from_secs(59), None));
    assert!(probe_due(min, None));
    assert!(!probe_due(min * 1000, Some(Duration::from_secs(3600))));
    assert!(probe_due(min * 1000, Some(PROBE_INTERVAL)));
}
