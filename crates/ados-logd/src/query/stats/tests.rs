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
    let stats = gather(&ro, &path, &ingest, "ok".to_string()).unwrap();

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
fn health_is_degraded_without_a_connection_a_writer_or_a_clean_check() {
    assert!(!health(false, true, "ok".into()).ok);
    assert!(health(true, true, "ok".into()).ok);
    // Writer gone → not ok even though the db reads.
    assert!(!health(true, false, "ok".into()).ok);
    // A failed structural check is reported, and is not ok.
    let h = health(true, true, "*** in database main ***".into());
    assert!(!h.ok);
    assert_eq!(h.integrity, "*** in database main ***");
}

#[test]
fn the_refreshed_verdict_reflects_a_real_check() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("logs.db");
    let _ = db::open(&path).unwrap();
    let v = IntegrityVerdict::new();
    v.refresh_blocking(&path);
    assert_eq!(v.get(), "ok");
    // A store that cannot be opened is not reported as healthy.
    v.refresh_blocking(&dir.path().join("absent").join("logs.db"));
    assert_ne!(v.get(), "ok");
}
