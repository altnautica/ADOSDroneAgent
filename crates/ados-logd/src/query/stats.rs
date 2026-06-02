//! Store health and ingest health for `/v1/stats` and `/v1/healthz`.
//!
//! `stats` reads the store's own metadata (file sizes, per-table row counts,
//! oldest/newest timestamps, schema version, the integrity result, and the
//! unsynced-row watermark) plus the live ingest counters the accept loop keeps.
//! `healthz` is the cheap liveness shape derived from the same read.
//!
//! All reads use the read-only connection; the file sizes are a stat of the
//! `.db`/`.db-wal` paths, not a query.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use rusqlite::Connection;
use serde::Serialize;

use crate::db;
use crate::ingest::{FrameClass, IngestStats};

/// The `/v1/stats` payload: store + ingest + sync health.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Stats {
    /// Main database file size, bytes.
    pub db_size_bytes: i64,
    /// Write-ahead-log file size, bytes (0 when checkpointed away or absent).
    pub wal_size_bytes: i64,
    /// Schema version (`PRAGMA user_version`).
    pub schema_version: i64,
    /// `PRAGMA integrity_check` result this read (`ok` when healthy).
    pub integrity: String,
    /// Row counts per table.
    pub rows: BTreeMap<String, i64>,
    /// Oldest stored timestamp across the data tables, microsecond epoch.
    pub oldest_ts_us: Option<i64>,
    /// Newest stored timestamp across the data tables, microsecond epoch.
    pub newest_ts_us: Option<i64>,
    /// Frames accepted off the ingest socket since the daemon started.
    pub ingest_accepted: u64,
    /// Frames dropped under backpressure, per class.
    pub ingest_dropped: BTreeMap<String, u64>,
    /// Rows not yet pushed to the cloud, per table (the explicit-push watermark).
    pub unsynced: BTreeMap<String, i64>,
}

/// The `/v1/healthz` payload: the cheap liveness/readiness shape.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Health {
    /// True when the daemon is serving and the store reads cleanly.
    pub ok: bool,
    /// True when the read connection opened.
    pub db_open: bool,
    /// True when the writer is believed alive (the broadcast channel is open).
    pub writer_alive: bool,
    /// The integrity result (`ok` when healthy, the failure text otherwise).
    pub integrity: String,
}

/// The data tables that carry a `ts_us` column (for oldest/newest and unsynced).
const TS_TABLES: [&str; 4] = ["logs", "metrics", "events", "hw"];

/// All tables whose row counts are reported.
const ALL_TABLES: [&str; 7] = [
    "sessions",
    "logs",
    "metrics",
    "events",
    "hw",
    "metrics_1m",
    "metrics_1h",
];

/// Gather the full stats payload from a read-only connection and the live
/// ingest counters. `writer_alive` is supplied by the daemon (whether the
/// broadcast channel still has the writer on the far end).
pub fn gather(
    conn: &Connection,
    db_path: &Path,
    ingest: &Arc<IngestStats>,
    writer_alive: bool,
) -> rusqlite::Result<Stats> {
    let mut rows = BTreeMap::new();
    for table in ALL_TABLES {
        let n: i64 = conn.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))?;
        rows.insert(table.to_string(), n);
    }

    let mut oldest: Option<i64> = None;
    let mut newest: Option<i64> = None;
    let mut unsynced = BTreeMap::new();
    for table in TS_TABLES {
        let (min, max): (Option<i64>, Option<i64>) = conn.query_row(
            &format!("SELECT min(ts_us), max(ts_us) FROM {table}"),
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        oldest = merge_min(oldest, min);
        newest = merge_max(newest, max);
        let un: i64 = conn.query_row(
            &format!("SELECT count(*) FROM {table} WHERE synced = 0"),
            [],
            |r| r.get(0),
        )?;
        unsynced.insert(table.to_string(), un);
    }

    let integrity = match db::integrity_check(conn) {
        Ok(()) => "ok".to_string(),
        Err(db::DbError::Integrity(s)) => s,
        Err(e) => e.to_string(),
    };
    // `user_version` returns the crate's DbError; map it to the rusqlite error
    // this function returns so the `?` chain stays one error type.
    let schema_version = db::user_version(conn).map_err(|e| match e {
        db::DbError::Sqlite(e) => e,
        other => {
            rusqlite::Error::ToSqlConversionFailure(Box::new(IntegrityReadError(other.to_string())))
        }
    })?;
    let _ = writer_alive; // health() consumes it; stats reports store + ingest.

    let dropped = ingest.dropped_by_class();
    let mut ingest_dropped = BTreeMap::new();
    for class in [
        FrameClass::Log,
        FrameClass::Telemetry,
        FrameClass::Event,
        FrameClass::Hw,
    ] {
        ingest_dropped.insert(
            class.label().to_string(),
            *dropped.get(class.label()).unwrap_or(&0),
        );
    }

    Ok(Stats {
        db_size_bytes: file_size(db_path),
        wal_size_bytes: file_size(&wal_path(db_path)),
        schema_version,
        integrity,
        rows,
        oldest_ts_us: oldest,
        newest_ts_us: newest,
        ingest_accepted: ingest.accepted(),
        ingest_dropped,
        unsynced,
    })
}

/// The cheap health shape. Opens nothing the caller has not already opened: the
/// caller passes a read connection (or `None` if the open itself failed).
pub fn health(conn: Option<&Connection>, writer_alive: bool) -> Health {
    match conn {
        None => Health {
            ok: false,
            db_open: false,
            writer_alive,
            integrity: "db not open".to_string(),
        },
        Some(conn) => {
            let integrity = match db::integrity_check(conn) {
                Ok(()) => "ok".to_string(),
                Err(db::DbError::Integrity(s)) => s,
                Err(e) => e.to_string(),
            };
            let healthy = integrity == "ok" && writer_alive;
            Health {
                ok: healthy,
                db_open: true,
                writer_alive,
                integrity,
            }
        }
    }
}

fn merge_min(acc: Option<i64>, v: Option<i64>) -> Option<i64> {
    match (acc, v) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, None) => a,
        (None, b) => b,
    }
}

fn merge_max(acc: Option<i64>, v: Option<i64>) -> Option<i64> {
    match (acc, v) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (a, None) => a,
        (None, b) => b,
    }
}

/// The `-wal` sidecar path for a database file.
fn wal_path(db_path: &Path) -> std::path::PathBuf {
    let mut s = db_path.as_os_str().to_os_string();
    s.push("-wal");
    std::path::PathBuf::from(s)
}

/// File size in bytes, or 0 when the file is absent.
fn file_size(path: &Path) -> i64 {
    std::fs::metadata(path).map(|m| m.len() as i64).unwrap_or(0)
}

/// A thin error wrapper used only to carry a non-sqlite `DbError` message
/// through the `rusqlite::Result` the stats reader returns. The common case
/// (`DbError::Sqlite`) is unwrapped directly; this covers the integrity arm.
#[derive(Debug)]
struct IntegrityReadError(String);

impl std::fmt::Display for IntegrityReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for IntegrityReadError {}

#[cfg(test)]
mod tests {
    use super::*;
    use ados_protocol::logd::Level;

    fn seed(path: &Path) {
        let conn = db::open(path).unwrap();
        conn.execute(
            "INSERT INTO sessions (started_us, kind) VALUES (100, 'boot')",
            [],
        )
        .unwrap();
        let s = conn.last_insert_rowid();
        for i in 0..3i64 {
            conn.execute(
                "INSERT INTO logs (ts_us, session, source, level, msg, synced) VALUES (?1, ?2, 'api', 2, 'm', ?3)",
                rusqlite::params![200 + i, s, i % 2],
            )
            .unwrap();
        }
        conn.execute(
            "INSERT INTO metrics (ts_us, session, metric, value) VALUES (300, ?1, 'cpu.load', 1.0)",
            [s],
        )
        .unwrap();
        let _ = Level::Info;
    }

    #[test]
    fn gather_reports_counts_span_and_unsynced() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("logs.db");
        seed(&path);
        let ro = db::open_readonly(&path).unwrap();
        let ingest = Arc::new(IngestStats::default());
        let stats = gather(&ro, &path, &ingest, true).unwrap();

        assert_eq!(stats.rows.get("logs"), Some(&3));
        assert_eq!(stats.rows.get("metrics"), Some(&1));
        assert_eq!(stats.rows.get("sessions"), Some(&1));
        assert_eq!(stats.integrity, "ok");
        assert_eq!(stats.oldest_ts_us, Some(200));
        assert_eq!(stats.newest_ts_us, Some(300));
        // Two of three logs were written synced=0 (i % 2 → 0,1,0 → two unsynced).
        assert_eq!(stats.unsynced.get("logs"), Some(&2));
        // The db file has a size; the wal may be 0 after a checkpoint.
        assert!(stats.db_size_bytes > 0);
        assert!(stats.ingest_dropped.contains_key("log"));
    }

    #[test]
    fn health_is_degraded_without_a_connection_or_a_writer() {
        assert!(!health(None, true).ok);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("logs.db");
        let _ = db::open(&path).unwrap();
        let ro = db::open_readonly(&path).unwrap();
        assert!(health(Some(&ro), true).ok);
        // Writer gone → not ok even though the db reads.
        assert!(!health(Some(&ro), false).ok);
    }
}
