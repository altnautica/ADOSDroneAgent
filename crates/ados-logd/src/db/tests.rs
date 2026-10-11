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
