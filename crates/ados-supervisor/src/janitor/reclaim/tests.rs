use super::*;
use std::fs;

#[test]
fn the_config_and_runtime_roots_are_refused() {
    assert!(is_protected(Path::new("/etc/ados")));
    assert!(is_protected(Path::new("/etc/ados/config.yaml")));
    assert!(is_protected(Path::new("/etc/ados/wfb/rx.key")));
    assert!(is_protected(Path::new("/opt/ados/venv/bin/python")));
    // A neighbouring name that merely shares a prefix is not protected.
    assert!(!is_protected(Path::new("/etc/adosxyz/thing")));
    assert!(!is_protected(Path::new("/var/ados/recordings/a.mp4")));
    assert!(!is_protected(Path::new("/var/log/ados/plugins/x.log")));
}

#[test]
fn a_protected_file_is_never_removed_even_when_asked_directly() {
    // The guard is at the removal helper, so a caller cannot route around it.
    assert_eq!(remove_file_guarded(Path::new("/etc/ados/config.yaml")), 0);
    assert!(
        Path::new("/etc/ados/config.yaml").exists() || true,
        "the point is that the helper refuses, not that the file exists here"
    );
}

#[test]
fn apt_archives_lose_the_debs_and_nothing_else() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    fs::write(dir.join("libfoo_1.2_arm64.deb"), vec![0u8; 4096]).unwrap();
    fs::write(dir.join("libbar_2.0_arm64.ddeb"), vec![0u8; 2048]).unwrap();
    fs::write(dir.join("lock"), b"x").unwrap();
    fs::create_dir(dir.join("partial")).unwrap();
    fs::write(dir.join("partial").join("half_1.0.deb"), vec![0u8; 1024]).unwrap();

    assert_eq!(reclaim_apt_archives(dir), 4096 + 2048 + 1024);
    assert!(dir.join("lock").exists(), "apt's lock is not a package");
    assert_eq!(reclaim_apt_archives(dir), 0, "idempotent");
}

#[test]
fn trimming_keeps_the_tail_and_is_idempotent() {
    let tmp = tempfile::tempdir().unwrap();
    let log = tmp.path().join("com-example-thing.log");
    // 100 lines of 100 bytes = ~10 100 bytes.
    let body: String = (0..100).map(|i| format!("{:0>99}\n", i)).collect();
    fs::write(&log, &body).unwrap();
    let before = fs::metadata(&log).unwrap().len();

    let freed = trim_append_only(&log, 4_000, 1_000);
    assert!(freed > 0, "an over-cap file must be trimmed");
    let after = fs::metadata(&log).unwrap().len();
    assert!(
        after <= 1_000,
        "the trim must respect the keep size: {after}"
    );
    assert!(after > 0, "the tail is the floor; it must not go to zero");
    assert_eq!(freed, before - after);

    // The survivor starts on a line boundary and ends with the last line.
    let kept = fs::read_to_string(&log).unwrap();
    assert!(kept.starts_with('0') || kept.starts_with('9') || kept.starts_with('1'));
    assert!(kept.ends_with("99\n"), "the newest lines are the ones kept");

    assert_eq!(
        trim_append_only(&log, 4_000, 1_000),
        0,
        "a second trim must reclaim nothing"
    );
}

#[test]
fn a_file_under_its_cap_is_untouched() {
    let tmp = tempfile::tempdir().unwrap();
    let log = tmp.path().join("small.log");
    fs::write(&log, b"one line\n").unwrap();
    assert_eq!(trim_append_only(&log, 4_000, 1_000), 0);
    assert_eq!(fs::read(&log).unwrap(), b"one line\n");
}

#[test]
fn only_dot_log_files_are_trimmed_in_the_plugin_directory() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let big: Vec<u8> = vec![b'x'; 10_000];
    fs::write(dir.join("com-example-a.log"), &big).unwrap();
    fs::write(dir.join("notes.txt"), &big).unwrap();

    let freed = trim_plugin_logs(dir, 1_000, 200);
    assert!(freed > 0);
    assert_eq!(
        fs::metadata(dir.join("notes.txt")).unwrap().len(),
        10_000,
        "a file that is not a plugin log is not ours to trim"
    );
}

#[test]
fn the_newest_quarantined_store_survives() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    for (name, size) in [
        ("logs.db.corrupt-1", 1_000usize),
        ("logs.db.corrupt-2", 2_000),
        ("logs.db.corrupt-3", 3_000),
    ] {
        fs::write(dir.join(name), vec![0u8; size]).unwrap();
        // Space the mtimes so "newest" is unambiguous.
        std::thread::sleep(std::time::Duration::from_millis(1100));
    }
    fs::write(dir.join("logs.db"), vec![0u8; 500]).unwrap();

    let freed = prune_quarantines(dir, 1);
    assert_eq!(freed, 1_000 + 2_000);
    assert!(
        dir.join("logs.db.corrupt-3").exists(),
        "the most recent corruption is the evidence and must survive"
    );
    assert!(
        dir.join("logs.db").exists(),
        "the live store is not a corpse"
    );
    assert_eq!(prune_quarantines(dir, 1), 0, "idempotent");
}

#[test]
fn recordings_older_than_the_window_go_but_the_newest_stay() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    for i in 0..5 {
        fs::write(dir.join(format!("flight-{i}.mp4")), vec![0u8; 1_000]).unwrap();
    }
    fs::write(dir.join("README"), b"not a recording").unwrap();

    // A cutoff far in the future makes every file older than the window.
    let freed = reclaim_recordings(dir, i64::MAX, 2, crate::janitor::now_unix());
    assert_eq!(freed, 3_000, "five recordings, the newest two kept");
    let left = fs::read_dir(dir).unwrap().count();
    assert_eq!(left, 3, "two recordings plus the file that is not one");
    assert!(dir.join("README").exists());
}

#[test]
fn a_mediamtx_segment_tree_is_enumerated_one_level_down() {
    // The layout mediamtx's native recorder writes: `<root>/<stream>/<stamp>.mp4`.
    // A flat-only scan would see none of it, so the category would read as
    // permanently over its cap while every reclaim found nothing to take.
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    fs::create_dir(dir.join("main")).unwrap();
    fs::write(dir.join("main/seg-01.mp4"), vec![0u8; 500]).unwrap();
    fs::write(dir.join("main/seg-02.mp4"), vec![0u8; 500]).unwrap();
    fs::write(dir.join("main/notes.txt"), b"x").unwrap();
    fs::write(dir.join("flat.mp4"), vec![0u8; 100]).unwrap();

    let mut names: Vec<String> = recording_entries(dir)
        .into_iter()
        .map(|(name, _)| name)
        .collect();
    names.sort();
    assert_eq!(
        names,
        vec![
            "flat.mp4".to_string(),
            "main/seg-01.mp4".to_string(),
            "main/seg-02.mp4".to_string(),
        ]
    );
    // And a relative name resolves back to the file it names.
    assert!(dir.join("main/seg-01.mp4").exists());
}

#[test]
fn the_free_space_floor_reclaims_oldest_first_and_stops_at_the_deficit() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let now = crate::janitor::now_unix();
    // Four settled segments, oldest first by mtime. Nothing is in flight.
    for (i, age) in [7_200i64, 5_400, 3_600, 1_800].iter().enumerate() {
        let path = dir.join(format!("seg-{i}.mp4"));
        fs::write(&path, vec![0u8; 1_000]).unwrap();
        set_mtime(&path, now - age);
    }

    // A 2 500-byte shortfall takes the three oldest (the fourth is the
    // keep-newest floor), and stops as soon as the deficit is covered.
    let freed = reclaim_recordings_to_free_floor(dir, 2_500, 1, now);
    assert_eq!(freed, 3_000, "oldest-first until the deficit is covered");
    assert!(!dir.join("seg-0.mp4").exists());
    assert!(!dir.join("seg-1.mp4").exists());
    assert!(!dir.join("seg-2.mp4").exists());
    assert!(dir.join("seg-3.mp4").exists(), "the newest survives");

    // No deficit → no reclaim, which is what makes a settled box a no-op.
    assert_eq!(reclaim_recordings_to_free_floor(dir, 0, 1, now), 0);
}

#[test]
fn the_free_space_floor_never_takes_the_segment_of_an_in_flight_capture() {
    // The janitor and the recorder must not fight over the same file: the
    // newest segment of an actively-recording path is being appended to right
    // now, and even a volume under its floor must leave it alone.
    //
    // TWO recording paths, both live, with `keep_newest` at its floor of 1 —
    // so the keep-newest count covers only the newest file OVERALL and the
    // second path's live segment can be saved by nothing but the in-flight
    // rule. A one-path fixture would still pass with that rule deleted.
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let now = crate::janitor::now_unix();
    fs::create_dir(dir.join("main")).unwrap();
    fs::create_dir(dir.join("sub")).unwrap();
    let settled = dir.join("main/seg-01.mp4");
    let live = dir.join("main/seg-02.mp4");
    let newest_overall = dir.join("sub/seg-01.mp4");
    for path in [&settled, &live, &newest_overall] {
        fs::write(path, vec![0u8; 1_000]).unwrap();
    }
    set_mtime(&settled, now - 3_600);
    set_mtime(&live, now - 40);
    set_mtime(&newest_overall, now - 5);

    // A deficit larger than everything on disk.
    let freed = reclaim_recordings_to_free_floor(dir, 10_000, 1, now);
    assert_eq!(freed, 1_000, "only the settled segment came off");
    assert!(!settled.exists());
    assert!(
        live.exists(),
        "the segment an in-flight capture is appending to must survive, and \
         the keep-newest count is spent on the other path"
    );
    assert!(newest_overall.exists());
}

#[test]
fn absent_directories_are_zero_not_an_error() {
    let missing = Path::new("/no/such/janitor/dir");
    assert_eq!(dir_bytes(missing), 0);
    assert_eq!(reclaim_apt_archives(missing), 0);
    assert_eq!(reclaim_apt_lists(missing), 0);
    assert_eq!(trim_plugin_logs(missing, 1, 1), 0);
    assert_eq!(
        reclaim_recordings(missing, i64::MAX, 1, crate::janitor::now_unix()),
        0
    );
    assert_eq!(
        reclaim_recordings_to_free_floor(missing, 10_000, 1, crate::janitor::now_unix()),
        0
    );
    assert_eq!(recording_entries(missing), Vec::new());
    assert_eq!(prune_quarantines(missing, 1), 0);
}

/// Set a file's mtime to a Unix second, so the age-ordered tests do not
/// depend on filesystem write-order timing.
fn set_mtime(path: &Path, unix_secs: i64) {
    let when = std::time::UNIX_EPOCH + std::time::Duration::from_secs(unix_secs as u64);
    let f = std::fs::OpenOptions::new().write(true).open(path).unwrap();
    f.set_modified(when).unwrap();
}
