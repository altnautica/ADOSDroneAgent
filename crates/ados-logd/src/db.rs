//! The durable local store: one WAL-mode SQLite database that only this daemon
//! opens read-write. Every other reader connects read-only.
//!
//! The schema is typed-denormalized: one row per log record, telemetry sample,
//! discrete event, and hardware snapshot, all grouped under a `sessions` row
//! (the boot/flight grouping). Open or sparse data rides in msgpack blob
//! columns, mirroring the open-state pattern elsewhere so new telemetry
//! round-trips without a schema change. Timestamps are microsecond-epoch
//! integers (sortable and keyset-friendly).
//!
//! The schema is versioned via `PRAGMA user_version` with embedded migration
//! strings applied in order. [`open`] applies the connection PRAGMAs (WAL,
//! `synchronous=NORMAL`, a busy timeout, autocheckpoint) and then runs any
//! pending migrations.

use std::path::Path;

use rusqlite::Connection;
use thiserror::Error;

/// The current schema version. Bump this and append a migration string whenever
/// the schema changes; never edit a migration that has shipped.
pub const SCHEMA_VERSION: i64 = 2;

/// Default on-disk path for the store.
pub const DEFAULT_DB_PATH: &str = "/var/ados/logd/logs.db";

/// Busy timeout applied to every connection, in milliseconds. A reader that
/// hits the single writer mid-checkpoint waits instead of failing immediately.
pub const BUSY_TIMEOUT_MS: u32 = 5000;

/// WAL autocheckpoint threshold in pages. The writer checkpoints the WAL back
/// into the main file after this many pages accumulate, bounding WAL growth on
/// the SD card.
pub const WAL_AUTOCHECKPOINT_PAGES: u32 = 1000;

/// Per-connection page-cache size for read-only connections, as SQLite's
/// negative-means-KiB form (`-512` = 512 KiB). Read-only connections are
/// short-lived and many open concurrently off the blocking pool; SQLite's
/// default cache (~2 MiB each) adds up across them and inflates the daemon's
/// resident set, while 512 KiB still keeps a working set of hot index and leaf
/// pages warm. The single writer keeps the larger default for write batching.
pub const RO_CACHE_SIZE_KIB: i32 = -512;

/// Errors raised opening or migrating the store.
#[derive(Debug, Error)]
pub enum DbError {
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("database integrity check failed: {0}")]
    Integrity(String),
}

/// Ordered migration strings. Index `i` migrates the schema from version `i` to
/// version `i + 1`. The first entry creates the full v1 schema.
const MIGRATIONS: &[&str] = &[
    // v0 -> v1: full initial schema.
    r#"
    -- Boot/flight session grouping. A flight session opens and closes on the
    -- arm/disarm transitions observed on the state stream; a boot session spans
    -- one power cycle.
    CREATE TABLE sessions (
        id          INTEGER PRIMARY KEY,
        started_us  INTEGER NOT NULL,
        ended_us    INTEGER,
        kind        TEXT    NOT NULL,            -- flight | boot | manual
        reason      TEXT,
        meta        BLOB                         -- msgpack open map
    );
    CREATE INDEX idx_sessions_started ON sessions (started_us);

    -- One log record from any producer.
    CREATE TABLE logs (
        id        INTEGER PRIMARY KEY,
        ts_us     INTEGER NOT NULL,
        session   INTEGER REFERENCES sessions (id),
        source    TEXT    NOT NULL,
        level     INTEGER NOT NULL,              -- 0..4
        target    TEXT,
        msg       TEXT    NOT NULL,
        fields    BLOB,                          -- msgpack open map
        redacted  INTEGER NOT NULL DEFAULT 0,    -- 1 if redaction changed a value
        synced    INTEGER NOT NULL DEFAULT 0
    );
    CREATE INDEX idx_logs_ts ON logs (ts_us);
    CREATE INDEX idx_logs_source_level_ts ON logs (source, level, ts_us);
    CREATE INDEX idx_logs_unsynced ON logs (synced) WHERE synced = 0;

    -- One telemetry sample: a dotted metric key with a numeric value.
    CREATE TABLE metrics (
        id       INTEGER PRIMARY KEY,
        ts_us    INTEGER NOT NULL,
        session  INTEGER REFERENCES sessions (id),
        metric   TEXT    NOT NULL,
        value    REAL    NOT NULL,
        tags     BLOB,                           -- msgpack open map
        synced   INTEGER NOT NULL DEFAULT 0
    );
    CREATE INDEX idx_metrics_metric_ts ON metrics (metric, ts_us);
    CREATE INDEX idx_metrics_ts ON metrics (ts_us);
    CREATE INDEX idx_metrics_unsynced ON metrics (synced) WHERE synced = 0;

    -- One discrete event: a state transition, a radio lock change, a sidecar
    -- drop, a pairing change, a transport error.
    CREATE TABLE events (
        id        INTEGER PRIMARY KEY,
        ts_us     INTEGER NOT NULL,
        session   INTEGER REFERENCES sessions (id),
        kind      TEXT    NOT NULL,
        source    TEXT    NOT NULL,
        severity  INTEGER NOT NULL,              -- 0..4
        detail    BLOB,                          -- msgpack open map
        synced    INTEGER NOT NULL DEFAULT 0
    );
    CREATE INDEX idx_events_kind_ts ON events (kind, ts_us);
    CREATE INDEX idx_events_ts ON events (ts_us);
    CREATE INDEX idx_events_unsynced ON events (synced) WHERE synced = 0;

    -- A periodic hardware sample. Every reading rides in the open signals blob
    -- so a new signal does not need a schema bump.
    CREATE TABLE hw (
        id       INTEGER PRIMARY KEY,
        ts_us    INTEGER NOT NULL,
        session  INTEGER REFERENCES sessions (id),
        signals  BLOB    NOT NULL,               -- msgpack open map
        synced   INTEGER NOT NULL DEFAULT 0
    );
    CREATE INDEX idx_hw_ts ON hw (ts_us);
    CREATE INDEX idx_hw_unsynced ON hw (synced) WHERE synced = 0;
    "#,
    // v1 -> v2: downsample rollup tables. Long-range charts must not scan raw
    // rows, and the raw rows age out well before the long-horizon shape should.
    // The maintenance step folds closed raw windows into these tables before the
    // raw rows are deleted, so a year-scale trend survives a thirty-day raw
    // window. Each row aggregates one metric+tags grouping inside one time
    // bucket; `avg` is derived at read time from `sum`/`count` so coarser grains
    // compose from finer ones. `WITHOUT ROWID` with the composite primary key
    // keeps the tables compact and makes the per-bucket upsert a key lookup.
    r#"
    CREATE TABLE metrics_1m (
        bucket_us INTEGER NOT NULL,   -- floor(ts_us / 60_000_000) * 60_000_000
        metric    TEXT    NOT NULL,
        tags_key  TEXT    NOT NULL DEFAULT '',
        count     INTEGER NOT NULL,
        sum       REAL    NOT NULL,
        min       REAL    NOT NULL,
        max       REAL    NOT NULL,
        last      REAL    NOT NULL,
        last_us   INTEGER NOT NULL,
        PRIMARY KEY (metric, tags_key, bucket_us)
    ) WITHOUT ROWID;

    CREATE TABLE metrics_1h (
        bucket_us INTEGER NOT NULL,   -- floor to the hour
        metric    TEXT    NOT NULL,
        tags_key  TEXT    NOT NULL DEFAULT '',
        count     INTEGER NOT NULL,
        sum       REAL    NOT NULL,
        min       REAL    NOT NULL,
        max       REAL    NOT NULL,
        last      REAL    NOT NULL,
        last_us   INTEGER NOT NULL,
        PRIMARY KEY (metric, tags_key, bucket_us)
    ) WITHOUT ROWID;

    -- Range queries over a grain scan by bucket time.
    CREATE INDEX idx_metrics_1m_bucket ON metrics_1m (bucket_us);
    CREATE INDEX idx_metrics_1h_bucket ON metrics_1h (bucket_us);
    "#,
];

/// Open the store read-write at `path`, apply the connection PRAGMAs, and run
/// any pending migrations. Creates the parent directory and the file if absent.
/// This is the only read-write opener; readers use [`open_readonly`].
pub fn open(path: impl AsRef<Path>) -> Result<Connection, DbError> {
    let path = path.as_ref();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let conn = Connection::open(path)?;
    apply_pragmas(&conn)?;
    migrate(&conn)?;
    Ok(conn)
}

/// Open the store read-only. WAL mode lets read-only connections proceed
/// concurrently with the single writer without blocking it.
pub fn open_readonly(path: impl AsRef<Path>) -> Result<Connection, DbError> {
    let conn = Connection::open_with_flags(
        path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    conn.busy_timeout(std::time::Duration::from_millis(BUSY_TIMEOUT_MS as u64))?;
    // Cap the page cache so the many concurrent short-lived readers do not each
    // carry SQLite's ~2 MiB default and inflate the daemon's resident set.
    conn.pragma_update(None, "cache_size", RO_CACHE_SIZE_KIB)?;
    Ok(conn)
}

/// Apply the connection PRAGMAs tuned for an append-heavy store on flash media:
/// WAL journaling, `synchronous=NORMAL`, a busy timeout, and WAL autocheckpoint.
fn apply_pragmas(conn: &Connection) -> Result<(), DbError> {
    // WAL: concurrent read-only readers do not block the writer.
    conn.pragma_update(None, "journal_mode", "WAL")?;
    // NORMAL: one fsync per checkpoint rather than per transaction; safe under
    // WAL (a crash can lose the last transactions but never corrupts the file).
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.busy_timeout(std::time::Duration::from_millis(BUSY_TIMEOUT_MS as u64))?;
    conn.pragma_update(None, "wal_autocheckpoint", WAL_AUTOCHECKPOINT_PAGES)?;
    // Enforce the session foreign keys.
    conn.pragma_update(None, "foreign_keys", "ON")?;
    Ok(())
}

/// Apply pending migrations in order, advancing `PRAGMA user_version` after each.
fn migrate(conn: &Connection) -> Result<(), DbError> {
    let mut version = user_version(conn)?;
    while (version as usize) < MIGRATIONS.len() {
        let sql = MIGRATIONS[version as usize];
        conn.execute_batch(sql)?;
        version += 1;
        conn.pragma_update(None, "user_version", version)?;
    }
    Ok(())
}

/// Read `PRAGMA user_version`.
pub fn user_version(conn: &Connection) -> Result<i64, DbError> {
    let v: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    Ok(v)
}

/// Run `PRAGMA integrity_check`, returning `Ok(())` only when it reports `ok`.
/// The full check cross-validates every index against its table, so its cost
/// scales with the database size — appropriate for a deliberate deep check, not
/// the boot-critical path.
pub fn integrity_check(conn: &Connection) -> Result<(), DbError> {
    let result: String = conn.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
    if result == "ok" {
        Ok(())
    } else {
        Err(DbError::Integrity(result))
    }
}

/// Run `PRAGMA quick_check`, returning `Ok(())` only when it reports `ok`. This
/// is the boot-path corruption guard: it catches gross structural corruption
/// (bad page headers, broken b-tree links) without the full check's per-index
/// cross-validation, so it stays fast on a large store. The store is a recreate-
/// on-corruption history cache, so the cheaper guard is the right one to gate
/// startup on; the full `integrity_check` remains for a deliberate deep audit.
pub fn quick_check(conn: &Connection) -> Result<(), DbError> {
    let result: String = conn.query_row("PRAGMA quick_check", [], |row| row.get(0))?;
    if result == "ok" {
        Ok(())
    } else {
        Err(DbError::Integrity(result))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_runs_migrations_and_passes_integrity_check() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("logs.db");
        let conn = open(&path).unwrap();

        // Migrations ran: user_version is at the current schema version.
        assert_eq!(user_version(&conn).unwrap(), SCHEMA_VERSION);
        // The store is structurally sound by both the deep and the boot-path
        // (quick) checks.
        integrity_check(&conn).unwrap();
        quick_check(&conn).unwrap();
    }

    #[test]
    fn wal_journal_mode_is_active() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("logs.db");
        let conn = open(&path).unwrap();
        let mode: String = conn
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .unwrap();
        assert_eq!(mode.to_lowercase(), "wal");
    }

    #[test]
    fn all_tables_and_a_session_foreign_key_exist() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("logs.db");
        let conn = open(&path).unwrap();

        for table in [
            "sessions",
            "logs",
            "metrics",
            "events",
            "hw",
            "metrics_1m",
            "metrics_1h",
        ] {
            let count: i64 = conn
                .query_row(
                    "SELECT count(*) FROM sqlite_master WHERE type='table' AND name=?1",
                    [table],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(count, 1, "table {table} should exist");
        }

        // A row can be inserted under a session and read back, exercising the
        // schema end to end.
        conn.execute(
            "INSERT INTO sessions (started_us, kind) VALUES (?1, ?2)",
            rusqlite::params![1_700_000_000_000_000i64, "boot"],
        )
        .unwrap();
        let session_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO logs (ts_us, session, source, level, msg) VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![
                1_700_000_000_000_001i64,
                session_id,
                "ados-logd",
                2i64,
                "hi"
            ],
        )
        .unwrap();
        let logged: i64 = conn
            .query_row(
                "SELECT count(*) FROM logs WHERE session=?1",
                [session_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(logged, 1);
    }

    #[test]
    fn migrate_is_idempotent_on_a_second_open() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("logs.db");
        {
            let _ = open(&path).unwrap();
        }
        // Re-opening an already-migrated store does not re-run migrations and
        // leaves the version where it was.
        let conn = open(&path).unwrap();
        assert_eq!(user_version(&conn).unwrap(), SCHEMA_VERSION);
        integrity_check(&conn).unwrap();
    }

    #[test]
    fn readonly_open_can_read_but_not_write() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("logs.db");
        {
            let conn = open(&path).unwrap();
            conn.execute(
                "INSERT INTO sessions (started_us, kind) VALUES (?1, ?2)",
                rusqlite::params![1i64, "manual"],
            )
            .unwrap();
        }
        let ro = open_readonly(&path).unwrap();
        let count: i64 = ro
            .query_row("SELECT count(*) FROM sessions", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1);
        // A write through the read-only connection is refused.
        let err = ro.execute(
            "INSERT INTO sessions (started_us, kind) VALUES (?1, ?2)",
            rusqlite::params![2i64, "manual"],
        );
        assert!(err.is_err());
    }
}
