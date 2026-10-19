use super::*;

const S: i64 = 1_000_000_000;

fn now(mono_s: i64, utc_s: i64, trusted: bool) -> Now {
    Now {
        mono_ns: mono_s * S,
        utc_ms: utc_s * 1000,
        utc_trusted: trusted,
    }
}

#[test]
fn a_charge_survives_a_service_restart_in_the_same_boot() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("lora/airtime.jsonl");
    let (mut l, _) = Ledger::open(&p, "boot-a", now(100, 1_000, false), 3600).unwrap();
    l.charge(
        now(110, 1_010, false),
        866.125,
        "865.0-867.0",
        30_000_000,
        "IN865",
    )
    .unwrap();
    drop(l);
    let (l, report) = Ledger::open(&p, "boot-a", now(120, 1_020, false), 3600).unwrap();
    assert_eq!(report.entries, 1);
    assert_eq!(l.spends()[0].at_ns, 110 * S);
    assert_eq!(l.spent_in(865.0, 867.0, 0), 30_000_000);
}

#[test]
fn an_untrusted_previous_boot_counts_as_spent_at_service_start() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("airtime.jsonl");
    let (mut l, _) = Ledger::open(&p, "boot-a", now(5_000, 9_000, false), 3600).unwrap();
    l.charge(
        now(5_000, 9_000, false),
        866.125,
        "865.0-867.0",
        30_000_000,
        "IN865",
    )
    .unwrap();
    drop(l);
    // Rebooted: monotonic restarts near zero, no trusted clock.
    let (l, _) = Ledger::open(&p, "boot-b", now(20, 9_100, false), 3600).unwrap();
    assert_eq!(l.spends()[0].at_ns, 20 * S);
    assert_eq!(l.spent_in(865.0, 867.0, 20 * S - 3600 * S), 30_000_000);
}

#[test]
fn trusted_clocks_age_a_previous_boot_by_utc() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("airtime.jsonl");
    let (mut l, _) = Ledger::open(&p, "boot-a", now(10, 50_000, true), 3600).unwrap();
    l.charge(
        now(10, 50_000, true),
        866.125,
        "865.0-867.0",
        1_000,
        "IN865",
    )
    .unwrap();
    drop(l);
    // 4000 s later on a new boot: older than the window.
    let (l, _) = Ledger::open(&p, "boot-b", now(30, 54_000, true), 3600).unwrap();
    assert_eq!(l.spends()[0].at_ns, 30 * S - 4000 * S);
    assert_eq!(l.spent_in(865.0, 867.0, 30 * S - 3600 * S), 0);
}

#[test]
fn a_trusted_heartbeat_bounds_the_downtime_credited() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("airtime.jsonl");
    let (mut l, _) = Ledger::open(&p, "boot-a", now(10, 0, false), 3600).unwrap();
    // Spent while the clock was untrusted, later the clock locked.
    l.charge(now(10, 0, false), 866.125, "865.0-867.0", 5_000, "IN865")
        .unwrap();
    l.heartbeat(now(70, 60_000, true)).unwrap();
    drop(l);
    let (l, _) = Ledger::open(&p, "boot-b", now(30, 61_000, true), 3600).unwrap();
    // Placed at the heartbeat: 1000 s before now.
    assert_eq!(l.spends()[0].at_ns, 30 * S - 1000 * S);
}

#[test]
fn a_torn_tail_is_dropped_and_the_file_rewritten() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("airtime.jsonl");
    let (mut l, _) = Ledger::open(&p, "boot-a", now(1, 1, false), 3600).unwrap();
    l.charge(now(2, 2, false), 866.125, "865.0-867.0", 7, "IN865")
        .unwrap();
    drop(l);
    let mut f = OpenOptions::new().append(true).open(&p).unwrap();
    f.write_all(b"{\"kind\":\"tx\",\"boot").unwrap();
    drop(f);
    let (l, report) = Ledger::open(&p, "boot-a", now(3, 3, false), 3600).unwrap();
    assert!(report.torn_tail && !report.reset);
    assert_eq!(l.spends().len(), 1);
    assert!(!std::fs::read_to_string(&p).unwrap().contains("\"boot\n"));
}

#[test]
fn a_corrupt_file_is_moved_aside_and_duty_starts_exhausted() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("airtime.jsonl");
    std::fs::write(&p, "garbage\n{\"kind\":\"seen\",\"boot_id\":\"a\",\"t_mono_ns\":1,\"t_utc_ms\":1,\"utc_trusted\":false}\n").unwrap();
    let (l, report) = Ledger::open(&p, "boot-a", now(100, 100, false), 3600).unwrap();
    assert!(report.reset);
    assert!(dir.path().join("airtime.jsonl.bad").exists());
    assert_eq!(l.exhausted_until_ns(), Some(100 * S + 3600 * S));
    assert!(l.spends().is_empty());
}

#[test]
fn compaction_drops_spends_older_than_the_window() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("airtime.jsonl");
    let (mut l, _) = Ledger::open(&p, "boot-a", now(1, 1, false), 3600).unwrap();
    l.charge(now(10, 10, false), 866.125, "865.0-867.0", 1, "IN865")
        .unwrap();
    l.charge(now(4000, 4000, false), 866.125, "865.0-867.0", 2, "IN865")
        .unwrap();
    l.compact(now(4000, 4000, false), 3600).unwrap();
    assert_eq!(l.spends().len(), 1);
    l.charge(now(4001, 4001, false), 866.125, "865.0-867.0", 3, "IN865")
        .unwrap();
    drop(l);
    let (l, _) = Ledger::open(&p, "boot-a", now(4002, 4002, false), 3600).unwrap();
    assert_eq!(l.spent_in(0.0, 1000.0, 0), 5);
}

#[test]
fn the_corrupt_ledger_lockout_survives_a_reload_and_a_reboot() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("airtime.jsonl");
    std::fs::write(&p, "garbage\n{\"kind\":\"seen\",\"boot_id\":\"a\",\"t_mono_ns\":1,\"t_utc_ms\":1,\"utc_trusted\":false}\n").unwrap();
    let (l, report) = Ledger::open(&p, "boot-a", now(100, 100, false), 3600).unwrap();
    assert!(report.reset);
    drop(l);
    // A SIGHUP reload in the same boot keeps the lockout end.
    let (l, report) = Ledger::open(&p, "boot-a", now(200, 200, false), 3600).unwrap();
    assert!(!report.reset);
    assert_eq!(l.exhausted_until_ns(), Some(3700 * S));
    drop(l);
    // Compaction inside the window keeps the reset entry.
    let (mut l, _) = Ledger::open(&p, "boot-a", now(300, 300, false), 3600).unwrap();
    l.compact(now(300, 300, false), 3600).unwrap();
    drop(l);
    // A reboot with no trusted clock counts the reset from service start.
    let (l, _) = Ledger::open(&p, "boot-b", now(10, 400, false), 3600).unwrap();
    assert_eq!(l.exhausted_until_ns(), Some(3610 * S));
    drop(l);
    // Once the window has passed, compaction drops it.
    let (mut l, _) = Ledger::open(&p, "boot-b", now(4000, 4400, false), 3600).unwrap();
    l.compact(now(4000, 4400, false), 3600).unwrap();
    drop(l);
    let (l, _) = Ledger::open(&p, "boot-b", now(4001, 4401, false), 3600).unwrap();
    assert_eq!(l.exhausted_until_ns(), None);
}

#[test]
fn a_previous_boot_spend_ages_out_across_service_restarts() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("airtime.jsonl");
    let (mut l, _) = Ledger::open(&p, "boot-a", now(5_000, 9_000, false), 3600).unwrap();
    l.charge(
        now(5_000, 9_000, false),
        866.125,
        "865.0-867.0",
        30_000_000,
        "IN865",
    )
    .unwrap();
    drop(l);
    // Placed at the first start of the next boot ...
    let (l, _) = Ledger::open(&p, "boot-b", now(20, 9_100, false), 3600).unwrap();
    assert_eq!(l.spends()[0].at_ns, 20 * S);
    drop(l);
    // ... and kept there by a later restart in that boot, so it leaves the
    // window one hour later instead of being re-placed at every start.
    let (l, _) = Ledger::open(&p, "boot-b", now(3_700, 12_800, false), 3600).unwrap();
    assert_eq!(l.spends()[0].at_ns, 20 * S);
    assert_eq!(l.spent_in(865.0, 867.0, 3_700 * S - 3_600 * S), 0);
}
