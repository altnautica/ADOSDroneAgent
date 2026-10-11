use std::sync::atomic::AtomicBool;

use rusqlite::params;

use super::*;
use crate::db;
use crate::retention::{
    allocated_bytes, run_maintenance, used_store_bytes, ClockTrust, RetentionConfig,
};

const BASE_US: i64 = 1_700_000_000_000_000;
const SEED_ROWS: i64 = 20_000;
const GIB: u64 = 1024 * 1024 * 1024;

/// A temp store holding `SEED_ROWS` ~400-byte log rows, checkpointed so the
/// allocated size is the main file.
fn seeded_store() -> (tempfile::TempDir, std::path::PathBuf, Connection) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("logs.db");
    let conn = db::open(&path).unwrap();
    let msg = "x".repeat(400);
    conn.execute_batch("BEGIN").unwrap();
    for i in 0..SEED_ROWS {
        conn.execute(
            "INSERT INTO logs (ts_us, source, level, msg) VALUES (?1, 'seed', 2, ?2)",
            params![BASE_US + i, msg],
        )
        .unwrap();
    }
    conn.execute_batch("COMMIT").unwrap();
    conn.pragma_update(None, "wal_checkpoint", "TRUNCATE")
        .unwrap();
    (dir, path, conn)
}

fn log_count(conn: &Connection) -> i64 {
    conn.query_row("SELECT count(*) FROM logs", [], |r| r.get(0))
        .unwrap()
}

fn pass(conn: &Connection, path: &Path, fs: FsUsage) -> crate::retention::MaintenanceReport {
    run_maintenance(
        conn,
        &RetentionConfig::default().clamped(),
        BASE_US,
        path,
        false,
        &AtomicBool::new(false),
        ClockTrust::Unestablished,
        Some(fs),
    )
    .unwrap()
}

#[test]
fn reserve_is_the_larger_of_the_percentage_and_the_floor() {
    let small = FsUsage {
        total_bytes: GIB,
        free_bytes: 0,
    };
    assert_eq!(small.reserve_bytes(), MIN_FREE_BYTES);
    let large = FsUsage {
        total_bytes: 100 * GIB,
        free_bytes: 0,
    };
    assert_eq!(large.reserve_bytes(), 100 * GIB / 100 * MIN_FREE_PERCENT);
    let healthy = FsUsage {
        total_bytes: 100 * GIB,
        free_bytes: 50 * GIB,
    };
    assert_eq!(healthy.deficit_bytes(), 0);
}

#[test]
fn retention_evicts_on_low_free_space() {
    let (_dir, path, conn) = seeded_store();
    let cfg = RetentionConfig::default().clamped();
    let used = used_store_bytes(&conn, &path).unwrap();
    assert!(
        used < cfg.max_bytes,
        "precondition: the store ({used}) is under its byte cap"
    );

    // A 4 GiB card whose free space sits half the store's size below the
    // reserve: only the headroom trigger can arm eviction here.
    let allocated = allocated_bytes(&conn).unwrap();
    let total_bytes = 4 * GIB;
    let reserve = FsUsage {
        total_bytes,
        free_bytes: 0,
    }
    .reserve_bytes();
    let fs = FsUsage {
        total_bytes,
        free_bytes: reserve - allocated / 2,
    };
    let deficit = fs.deficit_bytes();
    assert!(deficit > 0);

    let report = pass(&conn, &path, fs);
    assert!(
        report.had_eviction(),
        "low free space evicts even under the cap"
    );
    assert!(
        report.reclaimed_pages > 0,
        "freed pages went back to the fs"
    );
    let remaining = log_count(&conn);
    assert!(remaining < SEED_ROWS, "rows were evicted");
    assert!(
        remaining > 0,
        "eviction stops once the shortfall is covered"
    );
    assert_eq!(
        report.evicted_from_us,
        Some(BASE_US),
        "eviction is oldest-first"
    );
    assert!(
        allocated_bytes(&conn).unwrap() <= allocated - deficit,
        "the store shrank by at least the shortfall"
    );
}

#[test]
fn ample_free_space_evicts_nothing() {
    let (_dir, path, conn) = seeded_store();
    let fs = FsUsage {
        total_bytes: 4 * GIB,
        free_bytes: 2 * GIB,
    };
    let report = pass(&conn, &path, fs);
    assert!(!report.had_eviction());
    assert_eq!(report.reclaimed_pages, 0);
    assert_eq!(log_count(&conn), SEED_ROWS);
}

#[test]
fn emergency_evict_frees_one_oldest_batch() {
    let (_dir, _path, conn) = seeded_store();
    let out = emergency_evict(&conn).unwrap();
    assert_eq!(out.rows, crate::retention::DELETE_BATCH_ROWS as u64);
    assert_eq!(out.from_us, Some(BASE_US));
    assert!(out.reclaimed_pages > 0);
    assert_eq!(
        log_count(&conn),
        SEED_ROWS - crate::retention::DELETE_BATCH_ROWS as i64
    );
}
