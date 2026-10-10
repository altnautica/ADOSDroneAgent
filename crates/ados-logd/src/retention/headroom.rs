//! Filesystem headroom: the second eviction trigger, and the emergency reclaim
//! the writer runs when a write hits a full disk.
//!
//! The byte cap bounds the store against itself; it says nothing about the card
//! the store lives on. When something else fills the filesystem, the store can
//! sit well under its cap while every insert fails with `SQLITE_FULL` — and a
//! writer whose commit fails exits, so the Black Box stops recording exactly
//! when the box is in trouble. Retention therefore also keeps a free-space
//! reserve of `max(5% of the filesystem, 256 MiB)`. Below it, the same
//! oldest-first eviction loop the byte cap uses runs until the pages it frees
//! cover the shortfall, or the store is empty.
//!
//! The free-space measurement is a plain value ([`FsUsage`]) handed in by the
//! caller, so a test drives the trigger with a fixed number instead of whatever
//! the host filesystem happens to hold.

use std::path::Path;

use rusqlite::{Connection, ErrorCode};

use super::{evict_batch, incremental_vacuum, oldest_raw_row, MAX_INCREMENTAL_VACUUM_PAGES};

/// The floor of the free-space reserve. On a small card 5% is too thin to
/// absorb a burst of WAL growth, so the reserve never drops below this.
pub const MIN_FREE_BYTES: u64 = 256 * 1024 * 1024;

/// The free-space reserve as a percentage of the filesystem size.
pub const MIN_FREE_PERCENT: u64 = 5;

/// Size and free space of the filesystem holding the store, in bytes. `free`
/// is what the daemon can still allocate (`f_bavail`), not the root-reserved
/// total.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FsUsage {
    pub total_bytes: u64,
    pub free_bytes: u64,
}

impl FsUsage {
    /// The free space retention keeps in reserve:
    /// `max(MIN_FREE_PERCENT% of the filesystem, MIN_FREE_BYTES)`.
    pub fn reserve_bytes(&self) -> u64 {
        (self.total_bytes / 100 * MIN_FREE_PERCENT).max(MIN_FREE_BYTES)
    }

    /// How far free space is below the reserve; zero when the reserve holds.
    pub fn deficit_bytes(&self) -> u64 {
        self.reserve_bytes().saturating_sub(self.free_bytes)
    }
}

/// Measure the filesystem holding `db_dir` via `statvfs`. `None` when the
/// syscall fails, or off Linux where the crate has no `statvfs`; retention then
/// arms on the byte cap alone.
pub fn read_fs_usage(db_dir: &Path) -> Option<FsUsage> {
    let (total_bytes, free_bytes) = crate::hw::disk::read_fs_space(db_dir)?;
    Some(FsUsage {
        total_bytes,
        free_bytes,
    })
}

/// The number of `page_size` pages needed to cover `bytes`, as the `i64` the
/// `incremental_vacuum` pragma takes.
pub(super) fn pages_for(bytes: u64, page_size: u64) -> i64 {
    if page_size == 0 {
        return 0;
    }
    i64::try_from(bytes.div_ceil(page_size)).unwrap_or(i64::MAX)
}

/// Whether a SQLite error is `SQLITE_FULL`: the filesystem (or the database's
/// page limit) has no room for the write.
pub fn is_disk_full(e: &rusqlite::Error) -> bool {
    e.sqlite_error_code() == Some(ErrorCode::DiskFull)
}

/// What one emergency reclaim freed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EmergencyEviction {
    /// Raw rows evicted (one oldest-first batch at most).
    pub rows: u64,
    /// Oldest timestamp (µs) evicted, if any rows were.
    pub from_us: Option<i64>,
    /// Newest timestamp (µs) evicted, if any rows were.
    pub to_us: Option<i64>,
    /// Pages the incremental vacuum returned to the filesystem.
    pub reclaimed_pages: u64,
}

/// The writer's response to a write that failed with `SQLITE_FULL`: evict the
/// single globally-oldest batch, return the freed pages to the filesystem with a
/// bounded `incremental_vacuum`, and truncate the WAL so the reclaim actually
/// lands on the card. The caller retries its write once afterwards.
///
/// Deliberately one batch, not a pass: this runs inline on the ingest path, and
/// the periodic pass (armed by the same shortfall through [`FsUsage`]) does the
/// thorough work on its own cadence.
pub fn emergency_evict(conn: &Connection) -> Result<EmergencyEviction, rusqlite::Error> {
    let mut out = EmergencyEviction::default();
    if let Some((table, _oldest)) = oldest_raw_row(conn)? {
        let batch = evict_batch(conn, table)?;
        if batch.rows > 0 {
            out.rows = batch.rows;
            out.from_us = Some(batch.from_us);
            out.to_us = Some(batch.to_us);
        }
    }
    out.reclaimed_pages = incremental_vacuum(conn, MAX_INCREMENTAL_VACUUM_PAGES)?;
    conn.pragma_update(None, "wal_checkpoint", "TRUNCATE")?;
    Ok(out)
}

#[cfg(test)]
mod tests {
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
}
