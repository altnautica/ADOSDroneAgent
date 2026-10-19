//! The capture root, the size cache, the sidecar a pass builds and the write
//! probe.

use super::*;

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
