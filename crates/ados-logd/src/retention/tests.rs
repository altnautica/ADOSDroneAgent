use super::*;
use crate::db;

/// One microsecond-epoch base far enough into 2023 that bucket math is
/// well-defined and the synthetic-low-timestamp guard is not in play.
const BASE_US: i64 = 1_700_000_000_000_000;

fn temp_store() -> (tempfile::TempDir, std::path::PathBuf, Connection) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("logs.db");
    let conn = db::open(&path).unwrap();
    let session = open_boot(&conn);
    // Stash the session id where the seed helpers can reach it via a thread
    // local would be overkill; instead seed helpers take the session.
    let _ = session;
    (dir, path, conn)
}

fn open_boot(conn: &Connection) -> i64 {
    conn.execute(
        "INSERT INTO sessions (started_us, kind, reason) VALUES (?1, 'boot', 'start')",
        params![BASE_US],
    )
    .unwrap();
    conn.last_insert_rowid()
}

fn seed_metric(conn: &Connection, ts_us: i64, metric: &str, value: f64) {
    conn.execute(
        "INSERT INTO metrics (ts_us, metric, value) VALUES (?1, ?2, ?3)",
        params![ts_us, metric, value],
    )
    .unwrap();
}

fn seed_log(conn: &Connection, ts_us: i64) {
    conn.execute(
        "INSERT INTO logs (ts_us, source, level, msg) VALUES (?1, 'test', 2, 'm')",
        params![ts_us],
    )
    .unwrap();
}

fn count(conn: &Connection, table: &str) -> i64 {
    conn.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
        .unwrap()
}

#[test]
fn rollup_aggregates_closed_minute_buckets_and_leaves_the_open_one() {
    let (_dir, _path, conn) = temp_store();
    // Three samples in one closed minute bucket, one in the open (current)
    // bucket. now sits inside the bucket after the closed one.
    let closed_bucket = (BASE_US / BUCKET_1M_US) * BUCKET_1M_US;
    seed_metric(&conn, closed_bucket + 1_000, "cpu.util.all", 10.0);
    seed_metric(&conn, closed_bucket + 2_000, "cpu.util.all", 30.0);
    seed_metric(&conn, closed_bucket + 3_000, "cpu.util.all", 20.0);
    // A sample in the still-open next bucket must NOT be rolled.
    let open_bucket = closed_bucket + BUCKET_1M_US;
    seed_metric(&conn, open_bucket + 1_000, "cpu.util.all", 99.0);

    // now is inside the open bucket, so only the first bucket is closed.
    let now = open_bucket + 5_000;
    let written = roll_up(&conn, now).unwrap();
    assert!(written >= 1, "at least the 1m closed bucket was rolled");

    let (cnt, sum, mn, mx, last): (i64, f64, f64, f64, f64) = conn
        .query_row(
            "SELECT count, sum, min, max, last FROM metrics_1m \
                 WHERE metric='cpu.util.all' AND bucket_us=?1",
            params![closed_bucket],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .unwrap();
    assert_eq!(cnt, 3, "three raw rows folded");
    assert_eq!(sum, 60.0);
    assert_eq!(mn, 10.0);
    assert_eq!(mx, 30.0);
    assert_eq!(
        last, 20.0,
        "last is the value at the latest ts in the bucket"
    );

    // The open bucket was not rolled.
    let open_rolled: i64 = conn
        .query_row(
            "SELECT count(*) FROM metrics_1m WHERE bucket_us=?1",
            params![open_bucket],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(open_rolled, 0, "the open bucket must not be rolled");
}

#[test]
fn rollup_is_idempotent_and_groups_distinct_tag_sets() {
    let (_dir, _path, conn) = temp_store();
    let bucket = (BASE_US / BUCKET_1M_US) * BUCKET_1M_US;
    // Two metrics that share a name but differ by tags must roll into two
    // distinct rollup rows.
    conn.execute(
        "INSERT INTO metrics (ts_us, metric, value, tags) VALUES (?1, 'net.rx', 100.0, ?2)",
        params![
            bucket + 1_000,
            rmp_serde::to_vec_named(&tag("wlan0")).unwrap()
        ],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO metrics (ts_us, metric, value, tags) VALUES (?1, 'net.rx', 200.0, ?2)",
        params![
            bucket + 2_000,
            rmp_serde::to_vec_named(&tag("eth0")).unwrap()
        ],
    )
    .unwrap();
    let now = bucket + BUCKET_1M_US + 5_000;

    roll_up(&conn, now).unwrap();
    let rows_1: i64 = conn
        .query_row(
            "SELECT count(*) FROM metrics_1m WHERE metric='net.rx'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(rows_1, 2, "two distinct tag sets roll into two rows");

    // Running rollup again does not double-count: same row count, same sums.
    roll_up(&conn, now).unwrap();
    let rows_2: i64 = conn
        .query_row(
            "SELECT count(*) FROM metrics_1m WHERE metric='net.rx'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(rows_2, 2, "re-roll is idempotent on row count");
    let total_count: i64 = conn
        .query_row(
            "SELECT sum(count) FROM metrics_1m WHERE metric='net.rx'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(total_count, 2, "re-roll did not double the folded count");
}

fn tag(iface: &str) -> super::super::wire::Fields {
    let mut f = super::super::wire::Fields::new();
    f.insert("iface".to_string(), rmpv::Value::from(iface));
    f
}

#[test]
fn raw_ttl_deletes_old_rows_and_keeps_recent_ones() {
    let (_dir, path, conn) = temp_store();
    let cfg = RetentionConfig::default();
    let now = BASE_US;
    let old = now - (40 * 24 * 60 * 60) * 1_000_000; // 40 days old
    let recent = now - (60 * 1_000_000); // a minute old

    seed_metric(&conn, old, "cpu.util.all", 1.0);
    seed_log(&conn, old);
    seed_metric(&conn, recent, "cpu.util.all", 2.0);
    seed_log(&conn, recent);

    let report = run_maintenance(
        &conn,
        &cfg,
        now,
        &path,
        false,
        &AtomicBool::new(false),
        ClockTrust::Established,
        None,
    )
    .unwrap();
    assert!(report.ttl_deleted_rows >= 2, "old rows were TTL-deleted");
    // The old metric was rolled up before deletion, so its long-horizon shape
    // survives even though the raw row is gone.
    assert!(
        report.rolled_up_rows >= 1,
        "the old metric was rolled before being TTL-deleted"
    );
    // The recent rows survive; the old rows are gone.
    assert_eq!(count(&conn, "metrics"), 1, "only the recent metric remains");
    assert_eq!(count(&conn, "logs"), 1, "only the recent log remains");
    let surviving_ts: i64 = conn
        .query_row("SELECT ts_us FROM metrics", [], |r| r.get(0))
        .unwrap();
    assert_eq!(surviving_ts, recent);
    // The rolled-up bucket for the old metric is still present (rollup TTL is
    // a year, far beyond the 40-day-old row).
    let rolled: i64 = conn
        .query_row("SELECT count(*) FROM metrics_1m", [], |r| r.get(0))
        .unwrap();
    assert!(rolled >= 1, "rolled bucket survives raw TTL");
}

#[test]
fn an_unestablished_clock_refuses_the_ttl_but_still_enforces_the_size_cap() {
    // The RTC-less board: it booted at the kernel build date, so `now` is
    // not a time anybody measured. Deleting by `now - window` would take
    // the flight's own boot window with it, which is the one thing the
    // Black Box exists to keep.
    let (_dir, path, conn) = temp_store();
    let cfg = RetentionConfig::default();
    let now = BASE_US;
    let old = now - (40 * 24 * 60 * 60) * 1_000_000;

    seed_metric(&conn, old, "cpu.util.all", 1.0);
    seed_log(&conn, old);

    let report = run_maintenance(
        &conn,
        &cfg,
        now,
        &path,
        false,
        &AtomicBool::new(false),
        ClockTrust::Unestablished,
        None,
    )
    .unwrap();
    assert_eq!(report.ttl_deleted_rows, 0, "no age-based delete may run");
    assert_eq!(report.rollup_ttl_deleted_rows, 0);
    assert_eq!(count(&conn, "metrics"), 1, "the row survives the pass");
    assert_eq!(count(&conn, "logs"), 1);
    // Rollup still runs: it folds rows into buckets, it never deletes.
    assert!(report.rolled_up_rows >= 1);

    // And the same rows go on the first pass once the clock is trustworthy,
    // so this is a hold, not a permanent leak.
    let after = run_maintenance(
        &conn,
        &cfg,
        now,
        &path,
        false,
        &AtomicBool::new(false),
        ClockTrust::Established,
        None,
    )
    .unwrap();
    assert!(after.ttl_deleted_rows >= 2);
}

#[test]
fn the_size_cap_still_evicts_on_a_node_whose_clock_is_never_established() {
    // The bound that keeps a permanently-offline drone from filling its
    // card must not depend on the clock: eviction orders rows, it does not
    // date them. Without this the TTL gate above would trade a deleted
    // boot window for an unbounded store.
    let (_dir, path, conn) = temp_store();
    let now = BASE_US;
    seed_bulk_logs(&conn, now, 70_000, 512);
    conn.pragma_update(None, "wal_checkpoint", "TRUNCATE")
        .unwrap();
    let cfg = RetentionConfig {
        max_bytes: MIN_MAX_BYTES,
        low_water_ratio: 0.5,
        ..RetentionConfig::default()
    }
    .clamped();
    // Assert the precondition against the measure eviction arms on, so the
    // test fails loudly rather than silently skipping the eviction path.
    let used = used_store_bytes(&conn, &path).unwrap();
    assert!(used > cfg.max_bytes, "{used} <= {}", cfg.max_bytes);

    let report = run_maintenance(
        &conn,
        &cfg,
        now,
        &path,
        false,
        &AtomicBool::new(false),
        ClockTrust::Unestablished,
        None,
    )
    .unwrap();
    assert!(
        report.had_eviction(),
        "the size cap is the bound that does not need a clock"
    );
    assert_eq!(report.ttl_deleted_rows, 0, "still no age-based delete");
}

#[test]
fn rollup_ttl_deletes_buckets_older_than_the_rollup_window() {
    let (_dir, path, conn) = temp_store();
    let cfg = RetentionConfig::default();
    let now = BASE_US;
    // A rollup bucket two years old, inserted directly, must be reaped by the
    // one-year rollup TTL.
    let ancient_bucket = ((now - (730 * 24 * 60 * 60) * 1_000_000) / BUCKET_1M_US) * BUCKET_1M_US;
    conn.execute(
            "INSERT INTO metrics_1m (bucket_us, metric, tags_key, count, sum, min, max, last, last_us) \
             VALUES (?1, 'cpu.util.all', '', 5, 50.0, 5.0, 15.0, 10.0, ?1)",
            params![ancient_bucket],
        )
        .unwrap();
    // A recent rollup bucket must survive.
    let recent_bucket = (now / BUCKET_1M_US) * BUCKET_1M_US - BUCKET_1M_US;
    conn.execute(
            "INSERT INTO metrics_1m (bucket_us, metric, tags_key, count, sum, min, max, last, last_us) \
             VALUES (?1, 'cpu.util.all', '', 5, 50.0, 5.0, 15.0, 10.0, ?1)",
            params![recent_bucket],
        )
        .unwrap();

    let report = run_maintenance(
        &conn,
        &cfg,
        now,
        &path,
        false,
        &AtomicBool::new(false),
        ClockTrust::Established,
        None,
    )
    .unwrap();
    assert!(
        report.rollup_ttl_deleted_rows >= 1,
        "the two-year-old rollup bucket was reaped"
    );
    let remaining: i64 = conn
        .query_row(
            "SELECT count(*) FROM metrics_1m WHERE bucket_us=?1",
            params![recent_bucket],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(remaining, 1, "the recent rollup bucket survives");
    let ancient: i64 = conn
        .query_row(
            "SELECT count(*) FROM metrics_1m WHERE bucket_us=?1",
            params![ancient_bucket],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(ancient, 0, "the ancient rollup bucket is gone");
}

/// Insert `n` fat log rows spanning microsecond timestamps below `now`, the
/// oldest first, so the store crosses a small cap and the size guard has an
/// oldest-first ordering to evict by. Returns the timestamp of the oldest row.
/// All rows are recent enough that the TTL window never touches them, so the
/// size cap is the only mechanism that can evict here.
fn seed_bulk_logs(conn: &Connection, now: i64, n: i64, msg_len: usize) -> i64 {
    let tx = conn.unchecked_transaction().unwrap();
    let oldest = now - n * 1_000;
    for i in 0..n {
        tx.execute(
            "INSERT INTO logs (ts_us, source, level, msg) VALUES (?1, 'bulk', 2, ?2)",
            params![now - (n - i) * 1_000, "x".repeat(msg_len)],
        )
        .unwrap();
    }
    tx.commit().unwrap();
    oldest
}

#[test]
fn size_cap_evicts_oldest_first_down_to_low_water_and_reports_the_span() {
    let (_dir, path, conn) = temp_store();
    let now = BASE_US;
    // Cross the real floored cap deterministically: ~70k rows of half-kilobyte
    // messages is comfortably above the 16 MB floor, so eviction must run
    // regardless of host. Using the production floor as the cap exercises the
    // real clamp path, not a synthetic tiny cap.
    let oldest = seed_bulk_logs(&conn, now, 70_000, 512);
    conn.pragma_update(None, "wal_checkpoint", "TRUNCATE")
        .unwrap();

    let before = count(&conn, "logs");
    let cfg = RetentionConfig {
        max_bytes: MIN_MAX_BYTES,
        low_water_ratio: 0.5,
        ..RetentionConfig::default()
    }
    .clamped();

    // The seed is sized to exceed the cap; assert that precondition against
    // the measure eviction actually arms on, so the test fails loudly rather
    // than silently skipping the eviction path.
    let used = used_store_bytes(&conn, &path).unwrap();
    assert!(
        used > cfg.max_bytes,
        "the bulk seed must exceed the cap to exercise eviction: {used} <= {}",
        cfg.max_bytes
    );

    let report = run_maintenance(
        &conn,
        &cfg,
        now,
        &path,
        false,
        &AtomicBool::new(false),
        ClockTrust::Established,
        None,
    )
    .unwrap();
    assert!(report.had_eviction(), "eviction ran when over the cap");
    assert!(report.evicted_rows > 0);
    // Oldest-first: the freed span starts at the very oldest row.
    assert_eq!(
        report.evicted_from_us,
        Some(oldest),
        "eviction started at the globally oldest row"
    );
    let surviving_min: i64 = conn
        .query_row("SELECT min(ts_us) FROM logs", [], |r| r.get(0))
        .unwrap();
    assert!(
        surviving_min > report.evicted_from_us.unwrap(),
        "the oldest rows were the ones evicted"
    );
    assert!(count(&conn, "logs") < before, "rows were removed");
    // After eviction the used footprint is back at or below the cap.
    assert!(
        used_store_bytes(&conn, &path).unwrap() <= cfg.max_bytes,
        "eviction brought the store back under the cap"
    );
    // Eviction reclaims incrementally and MUST NOT rewrite the whole file.
    // A full VACUUM here would run on the eviction cadence forever, which on
    // an SD card is the single largest source of write wear on the box.
    assert!(
        !report.vacuumed,
        "eviction must never trigger a full VACUUM — that is the write treadmill"
    );
    assert!(
        report.reclaimed_pages > 0,
        "eviction reclaims freed pages incrementally"
    );
}

#[test]
fn a_store_parked_at_the_cap_stops_evicting_once_it_is_under_it() {
    // The regression this guards: reclaim is incremental, so the main file
    // does not shrink after an eviction. If the cap were armed on raw file
    // size, every subsequent pass would read the unchanged file as "still
    // over the cap" and evict again, draining the store to empty a batch at
    // a time while never being able to shrink the thing it was reacting to.
    let (_dir, path, conn) = temp_store();
    let now = BASE_US;
    seed_bulk_logs(&conn, now, 70_000, 512);
    conn.pragma_update(None, "wal_checkpoint", "TRUNCATE")
        .unwrap();
    let cfg = RetentionConfig {
        max_bytes: MIN_MAX_BYTES,
        low_water_ratio: 0.5,
        ..RetentionConfig::default()
    }
    .clamped();

    let first = run_maintenance(
        &conn,
        &cfg,
        now,
        &path,
        false,
        &AtomicBool::new(false),
        ClockTrust::Established,
        None,
    )
    .unwrap();
    assert!(
        first.had_eviction(),
        "the first pass evicts down to low water"
    );
    let remaining = count(&conn, "logs");
    assert!(remaining > 0, "eviction stopped at low water, not at empty");

    // Nothing new was written between passes, so the next two passes must be
    // clean no-ops on the eviction path.
    for pass in 0..2 {
        let again = run_maintenance(
            &conn,
            &cfg,
            now,
            &path,
            false,
            &AtomicBool::new(false),
            ClockTrust::Established,
            None,
        )
        .unwrap();
        assert!(
            !again.had_eviction(),
            "pass {pass} evicted again from a store already under the cap"
        );
    }
    assert_eq!(
        count(&conn, "logs"),
        remaining,
        "no rows were lost to a re-triggered eviction"
    );
}

#[test]
fn opening_a_legacy_store_never_rewrites_it_but_the_periodic_vacuum_converts_it() {
    // The regression, found on a real node: converting inside `db::open`
    // put a ~950 MB whole-file rewrite in front of daemon readiness. The
    // unit is Type=notify with TimeoutStartSec=5min and Restart=always,
    // so it sat in `activating` growing a 955 MB WAL and was minutes from
    // being killed mid-rewrite and restarted into the same rewrite — a
    // crash loop that tears the store, which is the exact failure this whole
    // change exists to stop.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("logs.db");

    // A store from before incremental mode existed: created with the
    // default auto_vacuum, then populated.
    {
        let legacy = Connection::open(&path).unwrap();
        legacy.pragma_update(None, "auto_vacuum", 0i64).unwrap();
        legacy.pragma_update(None, "journal_mode", "WAL").unwrap();
        legacy
            .execute_batch("CREATE TABLE t (id INTEGER PRIMARY KEY, blob TEXT)")
            .unwrap();
        let tx = legacy.unchecked_transaction().unwrap();
        for _ in 0..2_000 {
            tx.execute("INSERT INTO t (blob) VALUES (?1)", params!["x".repeat(256)])
                .unwrap();
        }
        tx.commit().unwrap();
    }

    // Opening it must NOT convert, and must NOT rewrite it.
    let before = std::fs::metadata(&path).unwrap().len();
    let conn = db::open(&path).unwrap();
    assert_eq!(
        db::auto_vacuum_mode(&conn).unwrap(),
        0,
        "opening a legacy store must not convert it — that is a whole-file \
             rewrite in front of readiness"
    );
    assert_eq!(
        std::fs::metadata(&path).unwrap().len(),
        before,
        "opening a legacy store must not rewrite it"
    );

    // The periodic vacuum — the one rewrite that was going to happen
    // anyway — is where the conversion rides along.
    let cfg = RetentionConfig::default();
    let report = run_maintenance(
        &conn,
        &cfg,
        BASE_US,
        &path,
        true,
        &AtomicBool::new(false),
        ClockTrust::Established,
        None,
    )
    .unwrap();
    assert!(report.vacuumed);
    assert_eq!(
        db::auto_vacuum_mode(&conn).unwrap(),
        db::AUTO_VACUUM_INCREMENTAL,
        "the periodic vacuum converts the store"
    );
    db::integrity_check(&conn).unwrap();
}

#[test]
fn a_fresh_store_opens_in_incremental_auto_vacuum_mode() {
    // Incremental reclaim is only available in this mode; without it
    // `incremental_vacuum` is a silent no-op and the file would drift up
    // until the weekly full vacuum.
    let (_dir, _path, conn) = temp_store();
    assert_eq!(
        db::auto_vacuum_mode(&conn).unwrap(),
        db::AUTO_VACUUM_INCREMENTAL,
        "a fresh store must open in incremental auto-vacuum mode"
    );
}

#[test]
fn no_eviction_when_under_the_cap() {
    let (_dir, path, conn) = temp_store();
    let now = BASE_US;
    seed_metric(&conn, now - 1_000, "cpu.util.all", 1.0);
    // The default 1 GB cap is far above a tiny store: no eviction.
    let cfg = RetentionConfig::default();
    let report = run_maintenance(
        &conn,
        &cfg,
        now,
        &path,
        false,
        &AtomicBool::new(false),
        ClockTrust::Established,
        None,
    )
    .unwrap();
    assert!(!report.had_eviction());
    assert_eq!(report.evicted_rows, 0);
    assert_eq!(report.evicted_from_us, None);
}

#[test]
fn the_periodic_vacuum_is_skipped_when_it_would_buy_nothing() {
    // A VACUUM runs as ONE transaction, so wal_autocheckpoint cannot fire
    // inside it and the entire rewrite lands in the WAL. On a ~1 GB store
    // that produced a 955 MB WAL on a real node, and the next start had to
    // recover it inside a TimeoutStartSec whose expiry restarts the daemon
    // straight back into the same recovery. Every unnecessary full VACUUM
    // is a scheduled opportunity to create that condition.
    //
    // A healthy store in incremental mode does not need one.
    let (_dir, path, conn) = temp_store();
    let now = BASE_US;
    seed_metric(&conn, now - 1_000, "cpu.util.all", 1.0);
    let cfg = RetentionConfig::default();
    let report = run_maintenance(
        &conn,
        &cfg,
        now,
        &path,
        true,
        &AtomicBool::new(false),
        ClockTrust::Established,
        None,
    )
    .unwrap();
    assert!(
        !report.vacuumed,
        "a healthy incremental store must not rewrite itself on a cadence"
    );
    db::integrity_check(&conn).unwrap();
}

#[test]
fn the_periodic_vacuum_still_runs_when_the_file_carries_real_slack() {
    // The escape valve: if incremental reclaim has genuinely fallen behind
    // and the free list is a large fraction of the file, a full compaction
    // is warranted and still happens.
    let (_dir, path, conn) = temp_store();
    let now = BASE_US;
    seed_bulk_logs(&conn, now, 40_000, 512);
    conn.pragma_update(None, "wal_checkpoint", "TRUNCATE")
        .unwrap();
    // Delete most of it WITHOUT reclaiming, so the free list is large.
    conn.execute("DELETE FROM logs", []).unwrap();

    let page_count: i64 = conn
        .query_row("PRAGMA page_count", [], |r| r.get(0))
        .unwrap();
    let freelist: i64 = conn
        .query_row("PRAGMA freelist_count", [], |r| r.get(0))
        .unwrap();
    assert!(
        freelist as f64 / page_count as f64 >= FULL_VACUUM_FREELIST_RATIO,
        "precondition: the free list must be a large fraction of the file"
    );

    let cfg = RetentionConfig::default();
    let report = run_maintenance(
        &conn,
        &cfg,
        now,
        &path,
        true,
        &AtomicBool::new(false),
        ClockTrust::Established,
        None,
    )
    .unwrap();
    assert!(
        report.vacuumed,
        "a file carrying real slack is still compacted"
    );
    db::integrity_check(&conn).unwrap();
}

#[test]
fn a_store_not_yet_in_incremental_mode_is_always_vacuumed_once() {
    // The conversion path: a legacy store adopts incremental mode only
    // across a VACUUM, so that one rewrite must still happen.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("logs.db");
    {
        let legacy = Connection::open(&path).unwrap();
        legacy.pragma_update(None, "auto_vacuum", 0i64).unwrap();
        legacy.pragma_update(None, "journal_mode", "WAL").unwrap();
        legacy
            .execute_batch("CREATE TABLE t (id INTEGER PRIMARY KEY)")
            .unwrap();
    }
    let conn = db::open(&path).unwrap();
    assert_eq!(db::auto_vacuum_mode(&conn).unwrap(), 0, "starts legacy");

    let cfg = RetentionConfig::default();
    let report = run_maintenance(
        &conn,
        &cfg,
        BASE_US,
        &path,
        true,
        &AtomicBool::new(false),
        ClockTrust::Established,
        None,
    )
    .unwrap();
    assert!(report.vacuumed, "the conversion rewrite still runs");
    assert_eq!(
        db::auto_vacuum_mode(&conn).unwrap(),
        db::AUTO_VACUUM_INCREMENTAL
    );
}

#[test]
fn periodic_vacuum_skipped_when_stopping() {
    let (_dir, path, conn) = temp_store();
    let now = BASE_US;
    seed_metric(&conn, now - 1_000, "cpu.util.all", 1.0);
    let cfg = RetentionConfig::default();
    // Even with the periodic cadence due (`do_vacuum = true`), a stop in flight
    // must skip the rewrite so it cannot overrun the shutdown join.
    let report = run_maintenance(
        &conn,
        &cfg,
        now,
        &path,
        true,
        &AtomicBool::new(true),
        ClockTrust::Established,
        None,
    )
    .unwrap();
    assert!(
        !report.vacuumed,
        "the periodic vacuum is skipped while stopping"
    );
}

#[test]
fn eviction_runs_but_vacuum_is_skipped_when_stopping() {
    let (_dir, path, conn) = temp_store();
    let now = BASE_US;
    // Same over-cap seed as the eviction test, so eviction must trigger.
    seed_bulk_logs(&conn, now, 70_000, 512);
    conn.pragma_update(None, "wal_checkpoint", "TRUNCATE")
        .unwrap();
    let cfg = RetentionConfig {
        max_bytes: MIN_MAX_BYTES,
        low_water_ratio: 0.5,
        ..RetentionConfig::default()
    }
    .clamped();
    assert!(
        used_store_bytes(&conn, &path).unwrap() > cfg.max_bytes,
        "the bulk seed must exceed the cap to exercise eviction"
    );
    // The cheap deletes still run; only the reclaim is withheld, so the file
    // stays at its high-water mark until a pass after a clean start reclaims.
    let report = run_maintenance(
        &conn,
        &cfg,
        now,
        &path,
        false,
        &AtomicBool::new(true),
        ClockTrust::Established,
        None,
    )
    .unwrap();
    assert!(report.had_eviction(), "eviction still runs while stopping");
    assert!(report.evicted_rows > 0);
    assert!(!report.vacuumed, "no full vacuum while stopping");
    assert_eq!(
        report.reclaimed_pages, 0,
        "the post-eviction reclaim is skipped while stopping"
    );
}

#[test]
fn config_clamp_floors_the_cap_and_bounds_the_ratio() {
    let cfg = RetentionConfig {
        max_bytes: 1_000, // absurdly small
        low_water_ratio: 2.0,
        ..RetentionConfig::default()
    }
    .clamped();
    assert_eq!(cfg.max_bytes, MIN_MAX_BYTES, "cap floored");
    assert!(cfg.low_water_ratio <= LOW_WATER_RATIO_MAX);
    assert!(cfg.low_water_ratio >= LOW_WATER_RATIO_MIN);

    let nan_cfg = RetentionConfig {
        low_water_ratio: f64::NAN,
        ..RetentionConfig::default()
    }
    .clamped();
    assert!(nan_cfg.low_water_ratio.is_finite(), "NaN ratio corrected");
}

#[test]
fn maintenance_on_an_empty_store_is_a_clean_no_op() {
    let (_dir, path, conn) = temp_store();
    let cfg = RetentionConfig::default();
    let report = run_maintenance(
        &conn,
        &cfg,
        BASE_US,
        &path,
        false,
        &AtomicBool::new(false),
        ClockTrust::Established,
        None,
    )
    .unwrap();
    assert_eq!(report, MaintenanceReport::default());
}

#[test]
fn oldest_raw_row_picks_the_global_minimum_across_tables() {
    let (_dir, _path, conn) = temp_store();
    seed_log(&conn, BASE_US + 500);
    seed_metric(&conn, BASE_US + 100, "m", 1.0); // the global oldest
    conn.execute(
        "INSERT INTO events (ts_us, kind, source, severity) VALUES (?1, 'k', 's', 2)",
        params![BASE_US + 900],
    )
    .unwrap();
    let (table, ts) = oldest_raw_row(&conn).unwrap().unwrap();
    assert_eq!(table, "metrics");
    assert_eq!(ts, BASE_US + 100);
}
