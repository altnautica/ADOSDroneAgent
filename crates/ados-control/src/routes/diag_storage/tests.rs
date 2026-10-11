use super::*;
use serde_json::Map;

/// Build a window of snapshots one second apart carrying a write counter that
/// advances by `per_tick` sectors.
fn rows_with_counter(count: usize, per_tick: f64, device: &str) -> Vec<HwRow> {
    (0..count)
        .map(|i| {
            let mut signals = Map::new();
            signals.insert(
                format!("disk.{device}.wr_sectors"),
                json!(1_000.0 + per_tick * i as f64),
            );
            HwRow {
                ts_us: 1_000_000 * i as i64,
                signals,
            }
        })
        .collect()
}

#[test]
fn write_rate_is_a_delta_across_the_window_not_a_reading() {
    // 2 sectors/s = 1 KB/s.
    let rows = rows_with_counter(120, 2.0, "mmcblk0");
    let m = write_rate(&rows);
    assert_eq!(m.kb_s.map(round1), Some(1.0));
    assert_eq!(m.device.as_deref(), Some("mmcblk0"));
    assert!(m.reason.is_none());
}

#[test]
fn a_counter_that_went_backwards_refuses_rather_than_reporting_a_negative_rate() {
    let mut rows = rows_with_counter(120, 2.0, "mmcblk0");
    // The box rebooted: the kernel counter restarts near zero.
    let last = rows.last_mut().unwrap();
    last.signals
        .insert("disk.mmcblk0.wr_sectors".into(), json!(5.0));

    let m = write_rate(&rows);
    assert_eq!(m.kb_s, None);
    assert!(
        m.reason.as_deref().unwrap().contains("rebooted"),
        "reason should name the restart, got {:?}",
        m.reason
    );
}

#[test]
fn a_window_too_short_to_be_a_rate_refuses() {
    let rows = rows_with_counter(5, 2.0, "mmcblk0");
    let m = write_rate(&rows);
    assert_eq!(m.kb_s, None);
    assert!(m.reason.as_deref().unwrap().contains("too short"));
}

#[test]
fn the_busiest_device_wins_over_an_idle_one() {
    let mut rows = rows_with_counter(120, 2.0, "mmcblk0");
    for (i, row) in rows.iter_mut().enumerate() {
        // A second device that barely moves.
        row.signals.insert(
            "disk.sda.wr_sectors".into(),
            json!(50_000.0 + 0.01 * i as f64),
        );
    }
    assert_eq!(busiest_device(&rows).as_deref(), Some("mmcblk0"));
}

#[test]
fn throttle_reports_sticky_bits_no_single_poll_would_catch() {
    // Every sample looks clean except one, which recorded that an
    // undervoltage had occurred (bit 16).
    let mut rows = rows_with_counter(10, 2.0, "mmcblk0");
    rows[0].signals.insert("throttle.raw".into(), json!(0));
    rows[4]
        .signals
        .insert("throttle.raw".into(), json!(1u64 << 16));
    for row in rows.iter_mut().skip(5) {
        row.signals.insert("throttle.raw".into(), json!(0));
    }

    let t = throttle_history(&rows);
    assert_eq!(t["undervoltage_occurred"], json!(true));
    assert_eq!(t["undervoltage_now"], json!(false));
    assert_eq!(t["clean"], json!(false));
}

#[test]
fn a_board_without_a_throttle_bitfield_says_unsupported_not_clean() {
    let rows = rows_with_counter(10, 2.0, "mmcblk0");
    let t = throttle_history(&rows);
    assert_eq!(t["supported"], json!(false));
    assert!(t.get("clean").is_none(), "absence must not read as clean");
}

#[test]
fn an_unreachable_store_is_unknown_never_ok() {
    let out = build(None, Path::new("/nonexistent"), None, None, true);
    assert_eq!(out["verdict"], json!("unknown"));
    assert_eq!(out["write"]["kb_per_s"], json!(null));
}

#[test]
fn the_measured_pre_fix_rate_reads_critical() {
    // 1 714 KB/s, the rate measured on a test node before the retention fix.
    let rows = rows_with_counter(120, 1_714.0 * 1024.0 / 512.0, "mmcblk0");
    let out = build(Some(&rows), Path::new("/nonexistent"), None, None, true);
    assert_eq!(out["verdict"], json!("critical"));
    assert!(out["reason"].as_str().unwrap().contains("wore a card out"));
}

#[test]
fn a_quiet_agent_on_a_healthy_card_reads_ok() {
    let rows = rows_with_counter(120, 2.0, "mmcblk0");
    let out = build(
        Some(&rows),
        Path::new("/nonexistent"),
        Some((100, 20)),
        None,
        true,
    );
    assert_eq!(out["verdict"], json!("ok"));
}

#[test]
fn a_full_filesystem_outranks_a_healthy_write_rate() {
    let rows = rows_with_counter(120, 2.0, "mmcblk0");
    let out = build(
        Some(&rows),
        Path::new("/nonexistent"),
        Some((100, 95)),
        None,
        true,
    );
    assert_eq!(out["verdict"], json!("critical"));
    assert!(out["reason"].as_str().unwrap().contains("full"));
}

// --- the direct /proc/diskstats measurement ------------------------------

/// A `/proc/diskstats` body with `wr_sectors` set for one device. The real
/// file's field order, taken from a Pi: three identity fields then the
/// counters, with sectors-written seventh.
fn diskstats(device: &str, wr_sectors: u64) -> String {
    format!(
            " 179       0 {device} 1200 40 90000 900 5000 300 {wr_sectors} 4200 0 3100 5100 0 0 0 0 40 30\n"
        )
}

#[test]
fn the_rate_is_measured_from_the_kernel_without_the_store() {
    // 12 000 sectors in 5s = 6 000 KiB/5s = 1 228.8 KB/s.
    let a = diskstats("mmcblk0", 1_000_000);
    let b = diskstats("mmcblk0", 1_012_000);
    let m = live_write_rate(&a, &b, 5.0).expect("a moving counter is a rate");
    assert_eq!(m.kb_s.map(round1), Some(1_200.0));
    assert_eq!(m.device.as_deref(), Some("mmcblk0"));
    assert_eq!(m.source, "direct");
    assert!(m.reason.is_none());
}

#[test]
fn the_direct_reading_wins_over_the_stores_older_average() {
    // The store's retained window says the box is quiet; the kernel says it
    // is not. The tool must show the reading it just took, and say so —
    // otherwise a change made a minute ago is invisible behind an hours-long
    // average.
    let rows = rows_with_counter(120, 2.0, "mmcblk0");
    let a = diskstats("mmcblk0", 1_000_000);
    let b = diskstats("mmcblk0", 1_012_000);

    let out = build(
        Some(&rows),
        Path::new("/nonexistent"),
        None,
        Some((&a, &b, 5.0)),
        true,
    );
    assert_eq!(out["write"]["source"], json!("direct"));
    assert_eq!(out["write"]["kb_per_s"], json!(1_200.0));
    assert_eq!(out["verdict"], json!("critical"));
}

#[test]
fn an_unreadable_diskstats_falls_back_to_the_store_rather_than_reporting_nothing() {
    let rows = rows_with_counter(120, 2.0, "mmcblk0");
    let out = build(Some(&rows), Path::new("/nonexistent"), None, None, true);
    assert_eq!(out["write"]["source"], json!("store"));
    assert_eq!(out["write"]["kb_per_s"], json!(1.0));
}

#[test]
fn with_neither_source_the_rate_is_absent_and_names_both_failures() {
    let out = build(None, Path::new("/nonexistent"), None, None, true);
    assert_eq!(out["write"]["kb_per_s"], json!(null));
    assert_eq!(out["write"]["source"], json!("none"));
    assert_eq!(out["verdict"], json!("unknown"));
    let why = out["write"]["reason"].as_str().unwrap();
    assert!(why.contains("diskstats") && why.contains("store"), "{why}");
}

#[test]
fn a_device_that_vanished_between_samples_does_not_become_a_negative_rate() {
    // A counter can only go backwards over five seconds if the device was
    // re-added. Reading that as a delta would print an enormous rate.
    let a = diskstats("sda", 5_000_000);
    let b = diskstats("sda", 12);
    assert!(live_write_rate(&a, &b, 5.0).is_none());
}

#[test]
fn the_busiest_device_wins_the_direct_measurement_too() {
    let a = format!(
        "{}{}",
        diskstats("mmcblk0", 1_000_000),
        diskstats("sda", 700)
    );
    let b = format!(
        "{}{}",
        diskstats("mmcblk0", 1_000_050),
        diskstats("sda", 90_700)
    );
    let m = live_write_rate(&a, &b, 5.0).unwrap();
    assert_eq!(m.device.as_deref(), Some("sda"));
}

#[test]
fn zram_swap_traffic_is_never_reported_as_card_wear() {
    // zram0 is swap in RAM; on a memory-tight SBC it can out-write the card.
    let a = format!(
        "{}{}{}",
        diskstats("mmcblk0", 1_000_000),
        diskstats("zram0", 10),
        diskstats("loop0", 10)
    );
    let b = format!(
        "{}{}{}",
        diskstats("mmcblk0", 1_000_050),
        diskstats("zram0", 900_010),
        diskstats("loop0", 900_010)
    );
    let m = live_write_rate(&a, &b, 5.0).unwrap();
    assert_eq!(m.device.as_deref(), Some("mmcblk0"));

    let mut rows = rows_with_counter(120, 2.0, "mmcblk0");
    for (i, row) in rows.iter_mut().enumerate() {
        row.signals
            .insert("disk.zram0.wr_sectors".into(), json!(9_000.0 * i as f64));
    }
    assert_eq!(busiest_device(&rows).as_deref(), Some("mmcblk0"));
}

#[test]
fn diskstats_lines_too_short_to_carry_the_counter_are_skipped_not_zeroed() {
    // A truncated line is missing data. Treating it as no writes is exactly
    // the fabricated zero this surface exists to avoid.
    let text = "  8       0 sda 1 2 3\n 179       0 mmcblk0 1 2 3 4 5 6 7 8 9 10 11\n";
    let parsed = parse_diskstats(text);
    assert!(
        !parsed.contains_key("sda"),
        "a short line carries no counter"
    );
    assert_eq!(parsed.get("mmcblk0").copied(), Some(7));
}

#[test]
fn an_empty_or_garbage_diskstats_yields_no_direct_reading() {
    assert!(parse_diskstats("").is_empty());
    assert!(live_write_rate("", "", 5.0).is_none());
    assert!(live_write_rate("not a diskstats file", "either", 5.0).is_none());
    // A zero-length window cannot be divided by.
    let a = diskstats("mmcblk0", 1);
    let b = diskstats("mmcblk0", 2);
    assert!(live_write_rate(&a, &b, 0.0).is_none());
}

#[test]
fn a_genuinely_idle_card_reads_as_zero_not_as_unmeasured() {
    // The counter really did not move. That is a measurement of zero, which
    // is a different claim from "could not measure", and both must be
    // expressible.
    let a = diskstats("mmcblk0", 1_000_000);
    let m = live_write_rate(&a, &a, 5.0).expect("an unmoved counter is still a reading");
    assert_eq!(m.kb_s, Some(0.0));
    assert_eq!(m.source, "direct");
}

// --- the store being off is a state, not a fault -------------------------

#[test]
fn a_disabled_store_is_not_reported_as_a_broken_one() {
    // With the store off there are no rows, which used to mean the whole
    // verdict collapsed to "unknown" — the word an operator reads as
    // "something here is broken". The rate is measurable without the store,
    // so the verdict stands on its own and the store simply says it is off.
    let a = diskstats("mmcblk0", 1_000_000);
    let b = diskstats("mmcblk0", 1_000_240);
    let out = build(
        None,
        Path::new("/nonexistent"),
        Some((100, 20)),
        Some((&a, &b, 5.0)),
        false,
    );
    assert_eq!(out["verdict"], json!("ok"));
    assert_eq!(out["store"]["enabled"], json!(false));
    assert_eq!(out["write"]["source"], json!("direct"));
    // 240 sectors over 5s = 24 KB/s.
    assert_eq!(out["write"]["kb_per_s"], json!(24.0));
}

#[test]
fn a_healthy_verdict_never_claims_a_clean_throttle_history_nobody_read() {
    // The sticky bits live only in the store. With it off, "no throttle
    // events recorded" would be a clean bill of health for something that
    // was never looked at — the exact fabrication this surface refuses.
    let a = diskstats("mmcblk0", 1_000_000);
    let b = diskstats("mmcblk0", 1_000_240);
    let out = build(
        None,
        Path::new("/nonexistent"),
        Some((100, 20)),
        Some((&a, &b, 5.0)),
        false,
    );
    let reason = out["reason"].as_str().unwrap();
    assert!(
        !reason.contains("no throttle events recorded"),
        "must not claim a history it did not read: {reason}"
    );
    assert!(reason.contains("not checked"), "{reason}");
    // And the throttle block names the store being off as the reason,
    // rather than the "did not answer" that means a fault.
    let why = out["throttle"]["reason"].as_str().unwrap();
    assert!(why.contains("disabled"), "{why}");
    assert_eq!(out["throttle"]["supported"], json!(null));
}

#[test]
fn with_the_store_on_but_silent_the_reason_is_still_a_fault() {
    // Same absent rows, opposite meaning: somebody asked for the store and
    // it is not answering. That IS something to look into, and it must not
    // be softened into "it is off".
    let a = diskstats("mmcblk0", 1_000_000);
    let b = diskstats("mmcblk0", 1_000_240);
    let out = build(
        None,
        Path::new("/nonexistent"),
        Some((100, 20)),
        Some((&a, &b, 5.0)),
        true,
    );
    assert_eq!(out["store"]["enabled"], json!(true));
    let why = out["throttle"]["reason"].as_str().unwrap();
    assert!(why.contains("did not answer"), "{why}");
}

#[test]
fn a_store_that_is_on_and_answering_still_reports_a_clean_history() {
    // The unchanged path: the claim is allowed when it was actually read.
    let mut rows = rows_with_counter(120, 2.0, "mmcblk0");
    for row in rows.iter_mut() {
        row.signals.insert("throttle.raw".into(), json!(0));
    }
    let out = build(
        Some(&rows),
        Path::new("/nonexistent"),
        Some((100, 20)),
        None,
        true,
    );
    assert_eq!(out["verdict"], json!("ok"));
    assert!(out["reason"]
        .as_str()
        .unwrap()
        .contains("no throttle events recorded"));
}

// --- the janitor's record ------------------------------------------------

#[test]
fn no_janitor_pass_reads_as_unknown_never_as_nothing_to_reclaim() {
    let out = janitor_facts(Path::new("/no/such/janitor.json"), 1_000);
    assert_eq!(out["ran"], json!(false));
    // The distinction that matters: a box nobody has swept must not report
    // zero reclaimable, which would read as "there is nothing to give".
    assert_eq!(out["reclaimable_bytes"], json!(null));
    assert_eq!(out["reclaimed_bytes"], json!(null));
    assert!(out["reason"]
        .as_str()
        .unwrap()
        .contains("has not completed"));
}

#[test]
fn a_corrupt_record_says_so_rather_than_inventing_figures() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("janitor.json");
    std::fs::write(&path, b"{ truncated").unwrap();
    let out = janitor_facts(&path, 1_000);
    assert_eq!(out["ran"], json!(false));
    assert_eq!(out["reclaimable_bytes"], json!(null));
    assert!(out["reason"]
        .as_str()
        .unwrap()
        .contains("could not be read"));
}

#[test]
fn the_last_pass_is_reported_with_its_age_so_staleness_is_visible() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("janitor.json");
    std::fs::write(
        &path,
        serde_json::to_vec(&json!({
            "version": 1,
            "rung": "pressure",
            "reclaimed_bytes": 366_002_176u64,
            "reclaimed": { "apt_archives": 195_035_136u64, "apt_lists": 170_967_040u64 },
            "reclaimable_bytes": 1_048_576u64,
            "reclaimable": { "quarantined_stores": 1_048_576u64 },
            "free_pct": 18.5,
            "ran_at_unix": 1_700_000_000i64,
        }))
        .unwrap(),
    )
    .unwrap();

    let out = janitor_facts(&path, 1_700_002_400);
    assert_eq!(out["ran"], json!(true));
    assert_eq!(out["rung"], json!("pressure"));
    assert_eq!(out["reclaimed_bytes"], json!(366_002_176u64));
    assert_eq!(out["reclaimable_bytes"], json!(1_048_576u64));
    assert_eq!(out["reclaimed"]["apt_archives"], json!(195_035_136u64));
    // Forty minutes ago — the reader can see the figures are not live.
    assert_eq!(out["age_s"], json!(2_400));
    assert_eq!(out["reason"], json!(null));
}

#[test]
fn the_footprint_and_its_budget_are_carried_through() {
    // Space, not throughput, is the headline: it is what fills a card and
    // corrupts it. The route must pass the budget arithmetic through
    // untouched so the renderer can lead with it.
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("janitor.json");
    std::fs::write(
        &path,
        serde_json::to_vec(&json!({
            "version": 1,
            "rung": "routine",
            "reclaimed_bytes": 0,
            "reclaimed": {},
            "reclaimable_bytes": 0,
            "reclaimable": {},
            "footprint_bytes": 1_694_498_816u64,
            "footprint": {
                "quarantined_stores": 1_073_741_824u64,
                "journal": 322_961_408u64,
                "apt": 195_035_136u64,
            },
            "budget_bytes": 5_368_709_120u64,
            "caps": { "quarantined_stores": 419_430_400u64 },
            "over_cap": { "quarantined_stores": 654_311_424u64 },
            "installed_bytes": 634_388_480u64,
            "ran_at_unix": 1_700_000_000i64,
        }))
        .unwrap(),
    )
    .unwrap();

    let out = janitor_facts(&path, 1_700_000_060);
    assert_eq!(out["footprint_bytes"], json!(1_694_498_816u64));
    assert_eq!(out["budget_bytes"], json!(5_368_709_120u64));
    assert_eq!(
        out["footprint"]["quarantined_stores"],
        json!(1_073_741_824u64)
    );
    assert_eq!(out["caps"]["quarantined_stores"], json!(419_430_400u64));
    // The residue a floor held above its cap is surfaced, not swallowed.
    assert_eq!(out["over_cap"]["quarantined_stores"], json!(654_311_424u64));
    // And the installed product travels separately from the budgeted total.
    assert_eq!(out["installed_bytes"], json!(634_388_480u64));
    assert!(
        out["footprint_bytes"].as_u64().unwrap() < out["budget_bytes"].as_u64().unwrap(),
        "this box is inside its budget while one category is well over its cap"
    );
}

#[test]
fn an_older_record_without_the_budget_fields_reports_them_absent_not_zero() {
    // A node that has not yet run a janitor carrying the budget must not
    // read as "occupying nothing", which would say the card is empty.
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("janitor.json");
    std::fs::write(&path, br#"{"rung":"routine","reclaimed_bytes":0}"#).unwrap();
    let out = janitor_facts(&path, 1_700_000_000);
    assert_eq!(out["footprint_bytes"], json!(null));
    assert_eq!(out["budget_bytes"], json!(null));
    assert_eq!(out["installed_bytes"], json!(null));
}

#[test]
fn a_record_without_a_timestamp_has_no_age_rather_than_a_zero_one() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("janitor.json");
    std::fs::write(&path, br#"{"rung":"routine","reclaimed_bytes":0}"#).unwrap();
    let out = janitor_facts(&path, 1_700_000_000);
    assert_eq!(out["ran"], json!(true));
    assert_eq!(
        out["age_s"],
        json!(null),
        "an unknown age is not 'just now'"
    );
}

#[test]
fn a_recorded_undervoltage_downgrades_an_otherwise_healthy_box() {
    let mut rows = rows_with_counter(120, 2.0, "mmcblk0");
    rows[3]
        .signals
        .insert("throttle.raw".into(), json!(1u64 << 16));
    let out = build(
        Some(&rows),
        Path::new("/nonexistent"),
        Some((100, 20)),
        None,
        true,
    );
    assert_eq!(out["verdict"], json!("wearing"));
    assert!(out["reason"].as_str().unwrap().contains("undervoltage"));
}
