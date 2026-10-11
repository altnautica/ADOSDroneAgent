use super::*;

#[test]
fn free_space_selects_the_rung() {
    let cfg = JanitorConfig::default();
    // An empty footprint contributes Routine, so this isolates the
    // free-space half of the decision.
    let empty = super::super::budget::Footprint::default();
    assert_eq!(cfg.rung_for(Some(50.0), &empty), Rung::Routine);
    assert_eq!(cfg.rung_for(Some(20.0), &empty), Rung::Routine);
    assert_eq!(cfg.rung_for(Some(19.9), &empty), Rung::Pressure);
    assert_eq!(cfg.rung_for(Some(10.0), &empty), Rung::Pressure);
    assert_eq!(cfg.rung_for(Some(9.9), &empty), Rung::Critical);
    assert_eq!(cfg.rung_for(Some(0.0), &empty), Rung::Critical);
}

#[test]
fn an_unmeasured_filesystem_never_escalates() {
    // Refusing to read free space is not evidence that space is short, and
    // the escalated rungs give up real evidence.
    let empty = super::super::budget::Footprint::default();
    assert_eq!(
        JanitorConfig::default().rung_for(None, &empty),
        Rung::Routine
    );
}

#[test]
fn the_escalated_rungs_widen_the_plan() {
    let cfg = JanitorConfig::default();
    let routine = cfg.plan(Rung::Routine);
    assert!(!routine.apt_lists);
    assert!(!routine.prune_quarantines);
    assert_eq!(routine.journal_target_bytes, None);
    assert_eq!(routine.recording_cutoff, cfg.recording_retention);

    for rung in [Rung::Pressure, Rung::Critical] {
        let p = cfg.plan(rung);
        assert!(p.apt_lists, "{rung:?} must give up the package index");
        assert!(p.prune_quarantines, "{rung:?} must prune old quarantines");
        assert!(p.journal_target_bytes.is_some());
        assert_eq!(p.recording_cutoff, cfg.recording_pressure_retention);
        assert!(p.recording_cutoff < cfg.recording_retention);
    }
}

#[test]
fn the_journal_is_never_vacuumed_below_its_floor() {
    // An operator (or a bad config) asks for something below the floor.
    let cfg = JanitorConfig {
        journal_pressure_bytes: 1024,
        ..JanitorConfig::default()
    };
    let p = cfg.plan(Rung::Critical);
    assert_eq!(p.journal_target_bytes, Some(cfg.journal_floor_bytes));
}

#[test]
fn the_newest_quarantine_survives_every_rung() {
    let entries = vec![
        ("logs.db.corrupt-1000".to_string(), 1_000),
        ("logs.db.corrupt-3000".to_string(), 3_000),
        ("logs.db.corrupt-2000".to_string(), 2_000),
    ];
    let cfg = JanitorConfig::default();
    for rung in [Rung::Routine, Rung::Pressure, Rung::Critical] {
        let plan = cfg.plan(rung);
        let doomed = beyond_newest(&entries, plan.keep_quarantines);
        assert!(
            !doomed.contains(&"logs.db.corrupt-3000".to_string()),
            "the newest quarantined store must survive at {rung:?}, got {doomed:?}"
        );
    }
}

#[test]
fn a_zero_keep_cannot_empty_the_set() {
    let entries = vec![
        ("a".to_string(), 1),
        ("b".to_string(), 2),
        ("c".to_string(), 3),
    ];
    // Even asked for zero, the newest is kept.
    let doomed = beyond_newest(&entries, 0);
    assert_eq!(doomed, vec!["b".to_string(), "a".to_string()]);
    assert!(!doomed.contains(&"c".to_string()));
}

#[test]
fn beyond_newest_keeps_the_newest_not_the_first_listed() {
    let entries = vec![
        ("old".to_string(), 100),
        ("new".to_string(), 900),
        ("mid".to_string(), 500),
    ];
    assert_eq!(
        beyond_newest(&entries, 1),
        vec!["mid".to_string(), "old".to_string()]
    );
    assert_eq!(beyond_newest(&entries, 2), vec!["old".to_string()]);
    assert!(beyond_newest(&entries, 5).is_empty());
}

#[test]
fn retention_never_takes_the_newest_recordings_however_old() {
    // Every recording predates the cutoff — a box that has not flown for a
    // year. The newest three still survive.
    let entries: Vec<(String, i64)> = (0..10)
        .map(|i| (format!("flight-{i:02}.mp4"), 1_000 + i as i64))
        .collect();
    let doomed = older_than_excluding(&entries, 100_000, &newest_names(&entries, 3));
    assert_eq!(doomed.len(), 7);
    for keeper in ["flight-09.mp4", "flight-08.mp4", "flight-07.mp4"] {
        assert!(
            !doomed.contains(&keeper.to_string()),
            "{keeper} must survive"
        );
    }
}

#[test]
fn retention_leaves_everything_inside_the_window() {
    let entries = vec![
        ("recent-a.mp4".to_string(), 9_000),
        ("recent-b.mp4".to_string(), 9_500),
    ];
    assert!(older_than_excluding(&entries, 1_000, &newest_names(&entries, 1)).is_empty());
}

#[test]
fn the_newest_segment_of_each_recording_path_is_in_flight_when_fresh() {
    let now = 1_000_000i64;
    // Two mediamtx segment directories plus a flat on-demand capture. Each
    // path's newest segment was written seconds ago; the older ones were not.
    let entries = vec![
        ("main/seg-01.mp4".to_string(), now - 600),
        ("main/seg-02.mp4".to_string(), now - 60),
        ("sub/seg-01.mp4".to_string(), now - 700),
        ("sub/seg-02.mp4".to_string(), now - 30),
        ("flight.mp4".to_string(), now - 5),
    ];
    let live = in_flight_names(&entries, now, RECORDING_IN_FLIGHT_WINDOW_S);
    assert!(live.contains("main/seg-02.mp4"));
    assert!(live.contains("sub/seg-02.mp4"));
    assert!(live.contains("flight.mp4"));
    // The finished segments behind them are not in flight and stay reclaimable.
    assert!(!live.contains("main/seg-01.mp4"));
    assert!(!live.contains("sub/seg-01.mp4"));
    assert_eq!(live.len(), 3);
}

#[test]
fn a_settled_recording_path_has_nothing_in_flight() {
    // Nothing has been written for an hour: the recorder is not running, so
    // no file is being appended to and the freshness rule protects none of
    // them. This is the half that keeps the rule from becoming a permanent
    // floor on the newest file of every path.
    let now = 1_000_000i64;
    let entries = vec![
        ("main/seg-01.mp4".to_string(), now - 4_000),
        ("main/seg-02.mp4".to_string(), now - 3_600),
    ];
    assert!(in_flight_names(&entries, now, RECORDING_IN_FLIGHT_WINDOW_S).is_empty());
}

#[test]
fn an_undatable_capture_is_never_read_as_in_flight() {
    // An unreadable mtime sorts as epoch, which makes it a reclaim candidate
    // before anything datable. It must not come back as "being written now".
    let now = 1_000_000i64;
    let entries = vec![("main/undatable.mp4".to_string(), 0)];
    assert!(in_flight_names(&entries, now, RECORDING_IN_FLIGHT_WINDOW_S).is_empty());
}

#[test]
fn the_protected_set_is_the_keep_floor_plus_whatever_is_in_flight() {
    let now = 1_000_000i64;
    // keep_newest=1 with two recording paths: the count alone protects the
    // newest ONE file overall, so the second path's live segment is only
    // covered by the in-flight rule.
    let entries = vec![
        ("main/seg-01.mp4".to_string(), now - 900),
        ("main/seg-02.mp4".to_string(), now - 10),
        ("sub/seg-01.mp4".to_string(), now - 800),
        ("sub/seg-02.mp4".to_string(), now - 40),
    ];
    let protected = protected_recordings(&entries, 1, now);
    assert!(protected.contains("main/seg-02.mp4"));
    assert!(
        protected.contains("sub/seg-02.mp4"),
        "the second path's live segment must be protected even at keep_newest=1"
    );
    assert!(!protected.contains("main/seg-01.mp4"));
    assert!(!protected.contains("sub/seg-01.mp4"));
}

#[test]
fn the_free_space_deficit_is_the_shortfall_and_zero_when_unmeasured() {
    // Below the floor: the shortfall is what must come off.
    assert_eq!(free_space_deficit(Some(200), 1_000), 800);
    // At or above the floor: nothing to do, so nothing is deleted.
    assert_eq!(free_space_deficit(Some(1_000), 1_000), 0);
    assert_eq!(free_space_deficit(Some(5_000), 1_000), 0);
    // Unmeasured is never grounds for deleting evidence.
    assert_eq!(free_space_deficit(None, 1_000), 0);
}

#[test]
fn a_file_under_its_cap_is_not_trimmed() {
    assert_eq!(trim_from(100, 200, 50), None);
    assert_eq!(trim_from(200, 200, 50), None, "at the cap is not over it");
}

#[test]
fn a_trim_leaves_the_tail_and_a_second_trim_does_nothing() {
    // 1000 bytes, cap 400, keep 100 -> drop 900, leaving 100.
    assert_eq!(trim_from(1_000, 400, 100), Some(900));
    // The file is now 100 bytes, under the cap: idempotent.
    assert_eq!(trim_from(100, 400, 100), None);
}

#[test]
fn a_keep_larger_than_the_cap_cannot_cause_a_trim_loop() {
    // keep 800 > cap 400 would leave the file over its cap and re-trigger
    // on every pass. It is clamped to the cap instead.
    assert_eq!(trim_from(1_000, 400, 800), Some(600));
    assert_eq!(trim_from(400, 400, 800), None, "the result is not over cap");
}
