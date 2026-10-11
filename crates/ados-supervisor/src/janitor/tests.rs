use super::*;
use std::fs;

#[test]
fn an_absent_section_is_enabled_with_defaults() {
    let cfg = read_config_from("agent:\n  name: x\n");
    assert!(cfg.enabled);
    assert_eq!(cfg, JanitorConfig::default());
}

#[test]
fn a_malformed_config_leaves_the_janitor_on() {
    assert!(read_config_from(": : : not yaml").enabled);
}

#[test]
fn tunables_are_read_and_the_cadence_is_floored() {
    let cfg = read_config_from(
        "storage:\n  janitor:\n    enabled: false\n    interval_s: 5\n    \
         plugin_log_max_mb: 8\n    recording_retention_days: 30\n    \
         quarantine_keep_newest: 2\n",
    );
    assert!(!cfg.enabled);
    // A five-second sweep would spend more disk logging than it recovers.
    assert_eq!(cfg.interval, Duration::from_secs(60));
    assert_eq!(cfg.plugin_log_max_bytes, 8 * 1024 * 1024);
    assert_eq!(cfg.recording_retention, Duration::from_secs(30 * 86_400));
    assert_eq!(cfg.quarantine_keep_newest, 2);
}

/// Lay down one of everything the janitor sweeps, and return the roots.
fn populate(base: &std::path::Path) -> Roots {
    let archives = base.join("apt-archives");
    let lists = base.join("apt-lists");
    let plugins = base.join("plugin-logs");
    let recordings = base.join("recordings");
    let store = base.join("logd");
    for d in [&archives, &lists, &plugins, &recordings, &store] {
        fs::create_dir_all(d).unwrap();
    }
    fs::write(archives.join("libfoo_1.0_arm64.deb"), vec![0u8; 4_000]).unwrap();
    fs::write(
        lists.join("deb.example.org_dists_stable_Packages"),
        vec![0u8; 5_000],
    )
    .unwrap();
    fs::write(lists.join("lock"), b"").unwrap();
    let long: String = (0..200).map(|i| format!("{i:0>99}\n")).collect();
    fs::write(plugins.join("com-example-a.log"), &long).unwrap();
    fs::write(base.join("audit.jsonl"), &long).unwrap();
    for i in 0..4 {
        fs::write(recordings.join(format!("flight-{i}.mp4")), vec![0u8; 1_000]).unwrap();
    }
    for i in 0..3 {
        fs::write(store.join(format!("logs.db.corrupt-{i}")), vec![0u8; 2_000]).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(1100));
    }
    fs::write(store.join("logs.db"), vec![0u8; 900]).unwrap();

    Roots {
        apt_archives: archives,
        apt_lists: lists,
        plugin_logs: plugins,
        audit_log: base.join("audit.jsonl"),
        recordings,
        journal: base.join("no-journal-here"),
        logd_store: store,
        installed: base.join("no-opt-ados-here"),
    }
}

fn tight_config() -> JanitorConfig {
    JanitorConfig {
        plugin_log_max_bytes: 4_000,
        plugin_log_keep_bytes: 1_000,
        audit_max_bytes: 4_000,
        audit_keep_bytes: 1_000,
        // Every recording is "old" for the purposes of the sweep.
        recording_retention: Duration::from_secs(0),
        recording_pressure_retention: Duration::from_secs(0),
        recording_keep_newest: 2,
        ..JanitorConfig::default()
    }
}

#[tokio::test]
async fn a_routine_sweep_leaves_the_things_with_value_alone() {
    let tmp = tempfile::tempdir().unwrap();
    let roots = populate(tmp.path());
    let cfg = tight_config();

    // Plenty of free space.
    let (rung, freed) = sweep(&roots, &cfg, Some(80.0), None).await;
    assert_eq!(rung, Rung::Routine);
    assert_eq!(freed.apt_archives, 4_000);
    assert!(freed.plugin_logs > 0);
    assert!(freed.audit_log > 0);
    assert_eq!(
        freed.recordings, 2_000,
        "four recordings, the newest two kept"
    );
    // The escalated categories did not run.
    assert_eq!(freed.apt_lists, 0);
    assert_eq!(freed.quarantined_stores, 0);
    assert!(
        roots
            .apt_lists
            .join("deb.example.org_dists_stable_Packages")
            .exists(),
        "the package index survives a routine sweep"
    );
    assert_eq!(
        fs::read_dir(&roots.logd_store).unwrap().count(),
        4,
        "three quarantined stores plus the live one survive a routine sweep"
    );
}

#[tokio::test]
async fn nothing_is_reclaimed_without_a_per_category_number() {
    let tmp = tempfile::tempdir().unwrap();
    let roots = populate(tmp.path());
    let (_, freed) = sweep(&roots, &tight_config(), Some(5.0), None).await;

    // The invariant: whatever came off the disk is attributed to a named
    // category, so the event can carry it. A category that freed bytes with
    // no accounting line would show up here as a total that is larger than
    // the parts.
    let summed: u64 = freed.pairs().iter().map(|(_, b)| *b).sum();
    assert_eq!(summed, freed.total());
    assert!(freed.total() > 0, "this fixture has plenty to reclaim");
    for (name, bytes) in freed.pairs() {
        if bytes > 0 {
            assert!(!name.is_empty());
        }
    }
}

#[tokio::test]
async fn the_critical_rung_still_leaves_the_newest_quarantined_store() {
    let tmp = tempfile::tempdir().unwrap();
    let roots = populate(tmp.path());
    let (rung, freed) = sweep(&roots, &tight_config(), Some(1.0), None).await;

    assert_eq!(rung, Rung::Critical);
    assert_eq!(freed.quarantined_stores, 4_000, "two of three corpses");
    assert!(
        roots.logd_store.join("logs.db.corrupt-2").exists(),
        "the most recent corruption is the evidence of the last failure"
    );
    assert!(
        roots.logd_store.join("logs.db").exists(),
        "the live store is never a janitor target"
    );
    // The floors held across the board.
    assert!(
        fs::metadata(roots.audit_log).unwrap().len() > 0,
        "the audit trail keeps its tail"
    );
    assert_eq!(
        fs::read_dir(&roots.recordings).unwrap().count(),
        2,
        "the newest recordings survive however old they are"
    );
}

#[tokio::test]
async fn a_second_sweep_reclaims_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let roots = populate(tmp.path());
    let cfg = tight_config();

    let (_, first) = sweep(&roots, &cfg, Some(1.0), None).await;
    assert!(first.total() > 0);
    let (_, second) = sweep(&roots, &cfg, Some(1.0), None).await;
    assert_eq!(
        second.total(),
        0,
        "a janitor that keeps finding work on an unchanged disk is deleting \
         something it should not: {second:?}"
    );
}

#[tokio::test]
async fn what_is_left_to_give_is_measured_not_the_raw_footprint() {
    let tmp = tempfile::tempdir().unwrap();
    let roots = populate(tmp.path());
    let cfg = tight_config();

    let before = reclaimable(&roots, &cfg);
    // Everything the fixture holds is visible as something a pressure sweep
    // could take.
    assert_eq!(before.apt_archives, 4_000);
    assert_eq!(
        before.apt_lists, 5_000,
        "the index is reclaimable under pressure"
    );
    assert_eq!(before.quarantined_stores, 4_000, "two of three corpses");
    assert!(before.plugin_logs > 0);
    assert!(before.audit_log > 0);

    // A log's reclaimable figure is what a trim would free, never its whole
    // size — the tail is a floor and reporting it as reclaimable would
    // promise space the janitor will not take.
    let log_len = fs::metadata(roots.plugin_logs.join("com-example-a.log"))
        .unwrap()
        .len();
    assert!(
        before.plugin_logs < log_len,
        "reported {} for a {log_len}-byte log; the tail must not be counted",
        before.plugin_logs
    );

    // After a full pass there is nothing left to give.
    let (_, freed) = sweep(&roots, &cfg, Some(1.0), None).await;
    assert!(freed.total() > 0);
    assert_eq!(
        reclaimable(&roots, &cfg).total(),
        0,
        "a swept box must not keep advertising space it cannot free"
    );
}

// --- the footprint budget ------------------------------------------------

#[tokio::test]
async fn a_category_over_its_cap_is_trimmed_on_a_box_with_plenty_of_room() {
    // The reason caps exist alongside a total budget: recordings must not
    // take the store's share just because the store is idle. Free space is
    // fine, the total is far under budget, and this still trims.
    let tmp = tempfile::tempdir().unwrap();
    let roots = populate(tmp.path());
    let cfg = JanitorConfig {
        // Keep the age-based sweep inert so this test isolates the cap.
        recording_retention: Duration::from_secs(86_400 * 3650),
        recording_pressure_retention: Duration::from_secs(86_400 * 3650),
        recording_keep_newest: 1,
        caps: budget::Caps {
            recordings: 2_500,
            ..budget::Caps::default()
        },
        ..JanitorConfig::default()
    };

    // Four 1 000-byte recordings against a 2 500-byte cap.
    let (rung, freed) = sweep(&roots, &cfg, Some(90.0), None).await;
    assert_eq!(rung, Rung::Routine, "plenty of free space, tiny footprint");
    assert_eq!(
        freed.recordings, 2_000,
        "trimmed down to the cap, oldest first"
    );
    assert_eq!(
        measure_footprint(&roots).recordings,
        2_000,
        "left at or under the cap"
    );
}

#[tokio::test]
async fn the_cap_never_takes_the_newest_even_when_it_alone_exceeds_it() {
    // The floor outranks the cap. This is not hypothetical: a single 1 GB
    // quarantined store against a 400 MB quarantine share is what the drone
    // was actually holding.
    let tmp = tempfile::tempdir().unwrap();
    let roots = populate(tmp.path());
    let cfg = JanitorConfig {
        caps: budget::Caps {
            // Far below the size of even one corpse in the fixture.
            quarantined_stores: 100,
            ..budget::Caps::default()
        },
        ..JanitorConfig::default()
    };

    let (_, freed) = sweep(&roots, &cfg, Some(90.0), None).await;
    assert_eq!(freed.quarantined_stores, 4_000, "two of the three corpses");
    assert!(
        roots.logd_store.join("logs.db.corrupt-2").exists(),
        "the newest quarantined store survives its own cap"
    );
    // And the residue is visible rather than silently accepted.
    let after = measure_footprint(&roots);
    assert!(
        budget::over_cap(&after, &cfg.caps, budget::Category::QuarantinedStores) > 0,
        "a category a floor holds above its cap must still read as over"
    );
}

#[tokio::test]
async fn a_footprint_over_budget_escalates_the_rung_on_a_mostly_empty_card() {
    // The failure a percentage trigger cannot see: a big card barely used,
    // and an agent whose own footprint has run away. Free space says 97%,
    // and the pass still escalates.
    let tmp = tempfile::tempdir().unwrap();
    let roots = populate(tmp.path());
    let cfg = JanitorConfig {
        budget_bytes: 1_000,
        ..JanitorConfig::default()
    };
    let footprint = measure_footprint(&roots);
    assert!(footprint.budgeted_total() > 1_000);
    assert_eq!(cfg.rung_for(Some(97.0), &footprint), Rung::Critical);
}

#[tokio::test]
async fn the_installed_product_is_measured_and_never_touched() {
    let tmp = tempfile::tempdir().unwrap();
    let mut roots = populate(tmp.path());
    let installed = tmp.path().join("opt-ados");
    fs::create_dir_all(installed.join("venv/bin")).unwrap();
    fs::write(installed.join("venv/bin/python"), vec![0u8; 9_000]).unwrap();
    roots.installed = installed.clone();

    let before = measure_footprint(&roots);
    assert_eq!(before.installed, 9_000, "reported");
    assert!(
        !before.pairs().iter().any(|(n, _)| *n == "installed"),
        "and excluded from the budgeted categories"
    );

    let cfg = JanitorConfig {
        budget_bytes: 1,
        caps: budget::Caps {
            recordings: 0,
            quarantined_stores: 0,
            ..budget::Caps::default()
        },
        ..JanitorConfig::default()
    };
    // The most aggressive pass this config can produce.
    let (rung, _) = sweep(&roots, &cfg, Some(0.5), None).await;
    assert_eq!(rung, Rung::Critical);
    assert_eq!(
        measure_footprint(&roots).installed,
        9_000,
        "the installed product is never reclaimed, at any rung"
    );
}

#[tokio::test]
async fn cap_enforcement_is_idempotent() {
    let tmp = tempfile::tempdir().unwrap();
    let roots = populate(tmp.path());
    let cfg = JanitorConfig {
        recording_retention: Duration::from_secs(86_400 * 3650),
        recording_pressure_retention: Duration::from_secs(86_400 * 3650),
        recording_keep_newest: 1,
        caps: budget::Caps {
            recordings: 2_500,
            quarantined_stores: 2_500,
            ..budget::Caps::default()
        },
        ..JanitorConfig::default()
    };
    let (_, first) = sweep(&roots, &cfg, Some(90.0), None).await;
    assert!(first.total() > 0);
    let (_, second) = sweep(&roots, &cfg, Some(90.0), None).await;
    assert_eq!(
        second.total(),
        0,
        "a settled box reclaims nothing: {second:?}"
    );
}

#[test]
fn the_budget_and_caps_are_configurable() {
    let cfg = read_config_from(
        "storage:\n  janitor:\n    budget_mb: 2048\n    caps:\n      recordings_mb: 512\n      journal_mb: 64\n",
    );
    assert_eq!(cfg.budget_bytes, 2048 * 1024 * 1024);
    assert_eq!(cfg.caps.recordings, 512 * 1024 * 1024);
    assert_eq!(cfg.caps.journal, 64 * 1024 * 1024);
    // Unspecified caps keep their defaults rather than collapsing to zero,
    // which would make one configured cap silently delete everything else.
    assert_eq!(cfg.caps.apt, budget::DEFAULT_CAP_APT);
    assert_eq!(cfg.caps.log_store, budget::DEFAULT_CAP_LOG_STORE);
}

#[tokio::test]
async fn an_unmeasured_disk_sweeps_routinely_not_critically() {
    let tmp = tempfile::tempdir().unwrap();
    let roots = populate(tmp.path());
    let (rung, freed) = sweep(&roots, &tight_config(), None, None).await;
    assert_eq!(rung, Rung::Routine);
    assert_eq!(freed.quarantined_stores, 0);
    assert_eq!(freed.apt_lists, 0);
}

// --- the recordings space floor + the in-flight capture -----------------

/// Lay down TWO mediamtx segment streams: settled segments plus, on each
/// path, one segment a capture is appending to right now.
///
/// Two paths, not one, so `recording_keep_newest: 1` is spent on the newest
/// file OVERALL (`sub/seg-live.mp4`) and `main/seg-live.mp4` can be saved by
/// nothing but the in-flight rule. With a single path the keep-newest count
/// would cover the live segment and the in-flight test would pass with the
/// rule deleted.
fn populate_segments(base: &std::path::Path) -> Roots {
    let roots = populate(base);
    let now = now_unix();
    let main = roots.recordings.join("main");
    let sub = roots.recordings.join("sub");
    fs::create_dir_all(&main).unwrap();
    fs::create_dir_all(&sub).unwrap();
    for (i, age) in [7_200i64, 5_400, 3_600].iter().enumerate() {
        let path = main.join(format!("seg-{i}.mp4"));
        fs::write(&path, vec![0u8; 1_000]).unwrap();
        set_mtime(&path, now - age);
    }
    for (dir, age) in [(&main, 40i64), (&sub, 5)] {
        let live = dir.join("seg-live.mp4");
        fs::write(&live, vec![0u8; 1_000]).unwrap();
        set_mtime(&live, now - age);
    }
    // The flat on-demand captures `populate` wrote are settled; age them out
    // of the in-flight window so this fixture isolates the segment streams.
    for i in 0..4 {
        set_mtime(
            &roots.recordings.join(format!("flight-{i}.mp4")),
            now - 9_000,
        );
    }
    roots
}

fn set_mtime(path: &std::path::Path, unix_secs: i64) {
    let when = std::time::UNIX_EPOCH + Duration::from_secs(unix_secs as u64);
    let f = std::fs::OpenOptions::new().write(true).open(path).unwrap();
    f.set_modified(when).unwrap();
}

#[tokio::test]
async fn a_sweep_never_reclaims_the_segment_of_an_in_flight_capture() {
    // The regression this guards: mediamtx writes a continuous segment
    // stream, so the newest segment of a recording path is being appended to
    // at this instant. A janitor that reclaims it truncates the capture an
    // operator is deliberately making — the janitor and the recorder fighting
    // over one file.
    //
    // Everything is set up to make it a target: every rule is at its most
    // aggressive (age cutoff of zero, a cap of zero, keep_newest at its floor
    // of 1, and a free-space reading far below the floor), so only the
    // in-flight rule can save it.
    let tmp = tempfile::tempdir().unwrap();
    let roots = populate_segments(tmp.path());
    let live = roots.recordings.join("main/seg-live.mp4");
    let cfg = JanitorConfig {
        recording_retention: Duration::from_secs(0),
        recording_pressure_retention: Duration::from_secs(0),
        recording_keep_newest: 1,
        caps: budget::Caps {
            recordings: 0,
            ..budget::Caps::default()
        },
        ..JanitorConfig::default()
    };
    let (_, freed) = sweep(&roots, &cfg, Some(1.0), Some(0)).await;
    assert_eq!(
        freed.recordings, 7_000,
        "every settled capture came off: four flat plus three settled segments"
    );
    assert!(
        live.exists(),
        "the segment an in-flight capture is appending to must survive every \
         rule at once — the keep-newest count is spent on the other path, so \
         only the in-flight rule can have saved it"
    );
    assert!(
        roots.recordings.join("sub/seg-live.mp4").exists(),
        "and the other path's live segment survives too"
    );
    assert!(
        !roots.recordings.join("main/seg-0.mp4").exists(),
        "the settled segments are still reclaimable"
    );
}

#[tokio::test]
async fn a_volume_under_the_free_space_floor_reclaims_recordings_by_space() {
    // The guard `recordDeleteAfter: 24h` cannot provide: a high-bitrate day
    // fills the volume long before the window elapses, and a full volume
    // stops the recorder dead. Nothing here is old enough or over its cap, so
    // space is the ONLY thing that can trigger the reclaim.
    let tmp = tempfile::tempdir().unwrap();
    let roots = populate_segments(tmp.path());
    let cfg = JanitorConfig {
        // A decade of retention and a cap far above the fixture: the age and
        // cap rules are both inert.
        recording_retention: Duration::from_secs(86_400 * 3650),
        recording_pressure_retention: Duration::from_secs(86_400 * 3650),
        recording_keep_newest: 1,
        ..JanitorConfig::default()
    };

    // Above the floor: nothing is taken, because nothing else says to.
    let (_, plenty) = sweep(
        &roots,
        &cfg,
        Some(90.0),
        Some(plan::RECORD_MIN_FREE_BYTES * 2),
    )
    .await;
    assert_eq!(
        plenty.recordings, 0,
        "a volume with headroom must not reclaim on age or cap here"
    );

    // Below the floor by 2 500 bytes: 1 000-byte files come off oldest-first
    // until the shortfall is covered, so three go (the loop stops as soon as
    // it is met, it does not keep going to a round number).
    let (_, tight) = sweep(
        &roots,
        &cfg,
        Some(90.0),
        Some(plan::RECORD_MIN_FREE_BYTES - 2_500),
    )
    .await;
    assert_eq!(
        tight.recordings, 3_000,
        "the space floor is what reclaimed, and only as far as the deficit"
    );
    // Oldest first: the flat captures aged out to 9 000s predate every
    // segment, so they are what went.
    for i in 0..3 {
        assert!(
            !roots.recordings.join(format!("flight-{i}.mp4")).exists(),
            "flight-{i}.mp4 is among the three oldest and must have gone"
        );
    }
    assert!(
        roots.recordings.join("main/seg-0.mp4").exists(),
        "the deficit was covered before the newer segments were reached"
    );
    assert!(
        roots.recordings.join("main/seg-live.mp4").exists(),
        "and the in-flight segment is never a space-floor target"
    );
}

#[tokio::test]
async fn recordings_are_spared_when_cheaper_categories_cover_the_deficit() {
    // The card is short by 2 500 bytes, and the escalated rung's quarantine
    // prune alone frees 4 000. The operator's recordings must not be what
    // pays for space a dead store corpse was holding.
    let tmp = tempfile::tempdir().unwrap();
    let roots = populate_segments(tmp.path());
    let cfg = JanitorConfig {
        recording_retention: Duration::from_secs(86_400 * 3650),
        recording_pressure_retention: Duration::from_secs(86_400 * 3650),
        recording_keep_newest: 1,
        ..JanitorConfig::default()
    };
    let (rung, freed) = sweep(
        &roots,
        &cfg,
        Some(1.0),
        Some(plan::RECORD_MIN_FREE_BYTES - 2_500),
    )
    .await;
    assert_eq!(rung, Rung::Critical);
    assert!(freed.quarantined_stores >= 2_500);
    assert_eq!(freed.recordings, 0, "recordings paid for reclaimable waste");
    assert!(roots.recordings.join("flight-0.mp4").exists());
}

#[tokio::test]
async fn an_unmeasured_recordings_volume_never_reclaims_on_space() {
    // "I could not read the filesystem" is not grounds for deleting an
    // operator's recordings. This is the same rule the rung selection applies
    // to an unreadable free percentage, and it is why the floor takes an
    // `Option` rather than defaulting to zero free.
    let tmp = tempfile::tempdir().unwrap();
    let roots = populate_segments(tmp.path());
    let cfg = JanitorConfig {
        recording_retention: Duration::from_secs(86_400 * 3650),
        recording_pressure_retention: Duration::from_secs(86_400 * 3650),
        recording_keep_newest: 1,
        ..JanitorConfig::default()
    };
    let (_, freed) = sweep(&roots, &cfg, Some(90.0), None).await;
    assert_eq!(freed.recordings, 0);
}
