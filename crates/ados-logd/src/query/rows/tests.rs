use super::*;
use crate::db;
use ados_protocol::logd::{EventFrame, HwSnapshot, LogFrame, TelemetryFrame};
use rmpv::Value as MpVal;

/// Build a store with a writer-shaped row set so the read path is tested
/// against the real schema (and a real session row the foreign key needs).
fn seed(path: &std::path::Path) {
    let conn = db::open(path).unwrap();
    conn.execute(
        "INSERT INTO sessions (started_us, kind, reason) VALUES (?1, 'boot', 'start')",
        [1_000i64],
    )
    .unwrap();
    let session = conn.last_insert_rowid();
    for i in 0..5i64 {
        let mut l = LogFrame::new(
            2_000 + i,
            "api",
            Level::from_u8((i % 5) as u8),
            format!("m{i}"),
        );
        l.target = Some(format!("mod{i}"));
        l.fields
            .insert("attempt".to_string(), MpVal::from(i as u64));
        let fields = rmp_serde::to_vec_named(&l.fields).unwrap();
        conn.execute(
            "INSERT INTO logs (ts_us, session, source, level, target, msg, fields, redacted) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 0)",
            rusqlite::params![
                l.ts_us,
                session,
                l.source,
                i64::from(l.level.as_u8()),
                l.target,
                l.msg,
                fields
            ],
        )
        .unwrap();
    }
    // One event, one metric, one hw row to exercise the other tables.
    let evt = EventFrame::new(3_000, "radio.lock", "ados-radio", Level::Info);
    conn.execute(
        "INSERT INTO events (ts_us, session, kind, source, severity) VALUES (?1, ?2, ?3, ?4, ?5)",
        rusqlite::params![
            evt.ts_us,
            session,
            evt.kind,
            evt.source,
            i64::from(evt.severity.as_u8())
        ],
    )
    .unwrap();
    let m = TelemetryFrame::new(3_100, "cpu.load", 0.42);
    conn.execute(
        "INSERT INTO metrics (ts_us, session, metric, value) VALUES (?1, ?2, ?3, ?4)",
        rusqlite::params![m.ts_us, session, m.metric, m.value],
    )
    .unwrap();
    let mut hw = HwSnapshot::new(3_200);
    hw.signals
        .insert("thermal.soc_c".to_string(), MpVal::from(48.0));
    let signals = rmp_serde::to_vec_named(&hw.signals).unwrap();
    conn.execute(
        "INSERT INTO hw (ts_us, session, signals) VALUES (?1, ?2, ?3)",
        rusqlite::params![hw.ts_us, session, signals],
    )
    .unwrap();
}

fn filters(query: &str) -> QueryFilters {
    QueryFilters::parse(&super::super::params::QueryParams::parse(query), 0).unwrap()
}

#[test]
fn logs_come_back_newest_first_and_decode_fields() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("logs.db");
    seed(&path);
    let ro = db::open_readonly(&path).unwrap();
    let page = query_logs(&ro, &filters("kind=logs&limit=10"), None).unwrap();
    assert_eq!(page.rows.len(), 5);
    // Newest first: ts_us strictly decreasing.
    assert!(page.rows.windows(2).all(|w| w[0].ts_us > w[1].ts_us));
    // Fields decoded back to JSON.
    assert!(page.rows[0].fields.get("attempt").is_some());
    // No further page.
    assert!(page.last_key.is_none());
}

#[test]
fn keyset_pagination_returns_disjoint_pages() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("logs.db");
    seed(&path);
    let ro = db::open_readonly(&path).unwrap();
    let f = filters("kind=logs&limit=2");
    let p1 = query_logs(&ro, &f, None).unwrap();
    assert_eq!(p1.rows.len(), 2);
    let key = p1.last_key.unwrap();
    let cursor = Cursor::new(key.0, key.1, f.fingerprint());
    let p2 = query_logs(&ro, &f, Some(&cursor)).unwrap();
    assert_eq!(p2.rows.len(), 2);
    // No overlap between page 1 and page 2.
    let ids1: Vec<i64> = p1.rows.iter().map(|r| r.id).collect();
    let ids2: Vec<i64> = p2.rows.iter().map(|r| r.id).collect();
    assert!(ids1.iter().all(|id| !ids2.contains(id)));
}

#[test]
fn source_and_level_and_text_filters_apply() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("logs.db");
    seed(&path);
    let ro = db::open_readonly(&path).unwrap();
    // Only WARN+ (levels 3,4) survive the floor.
    let warn = query_logs(&ro, &filters("kind=logs&level=warn"), None).unwrap();
    assert!(warn
        .rows
        .iter()
        .all(|r| r.level == "warn" || r.level == "error"));
    // Text match against the message.
    let m3 = query_logs(&ro, &filters("kind=logs&text=m3"), None).unwrap();
    assert_eq!(m3.rows.len(), 1);
    assert_eq!(m3.rows[0].msg, "m3");
    // A source that does not exist returns nothing.
    let none = query_logs(&ro, &filters("kind=logs&source=does-not-exist"), None).unwrap();
    assert!(none.rows.is_empty());
}

#[test]
fn like_metacharacters_are_escaped() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("logs.db");
    let conn = db::open(&path).unwrap();
    conn.execute(
        "INSERT INTO sessions (started_us, kind) VALUES (1, 'boot')",
        [],
    )
    .unwrap();
    let s = conn.last_insert_rowid();
    for (i, msg) in ["100% full", "plain text"].iter().enumerate() {
        conn.execute(
            "INSERT INTO logs (ts_us, session, source, level, msg) VALUES (?1, ?2, 'api', 2, ?3)",
            rusqlite::params![10 + i as i64, s, msg],
        )
        .unwrap();
    }
    drop(conn);
    let ro = db::open_readonly(&path).unwrap();
    // A literal '%' must match only the row that actually contains it.
    let hit = query_logs(&ro, &filters("kind=logs&text=100%25"), None).unwrap();
    assert_eq!(hit.rows.len(), 1);
    assert_eq!(hit.rows[0].msg, "100% full");
}

#[test]
fn events_metrics_and_hw_tables_read() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("logs.db");
    seed(&path);
    let ro = db::open_readonly(&path).unwrap();
    assert_eq!(
        query_events(&ro, &filters("kind=events&limit=10"), None)
            .unwrap()
            .rows
            .len(),
        1
    );
    assert_eq!(
        query_metrics(&ro, &filters("kind=metrics&limit=10"), None)
            .unwrap()
            .rows
            .len(),
        1
    );
    let hw = query_hw(&ro, &filters("kind=hw&limit=10"), None).unwrap();
    assert_eq!(hw.rows.len(), 1);
    assert!(hw.rows[0].signals.get("thermal.soc_c").is_some());
}

#[test]
fn unsynced_only_filter_returns_only_synced_zero() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("logs.db");
    let conn = db::open(&path).unwrap();
    conn.execute(
        "INSERT INTO sessions (started_us, kind) VALUES (1, 'boot')",
        [],
    )
    .unwrap();
    let s = conn.last_insert_rowid();
    // Six logs: the even-indexed ones marked synced, the odd ones not.
    for i in 0..6i64 {
        conn.execute(
            "INSERT INTO logs (ts_us, session, source, level, msg, synced) \
                 VALUES (?1, ?2, 'api', 2, ?3, ?4)",
            rusqlite::params![100 + i, s, format!("m{i}"), i % 2],
        )
        .unwrap();
    }
    drop(conn);
    let ro = db::open_readonly(&path).unwrap();
    // Three rows carry synced=0 (i = 0, 2, 4 → 0; i = 1,3,5 → 1, so the
    // unsynced ones are i=0,2,4).
    let all = query_logs(&ro, &filters("kind=logs&limit=10"), None).unwrap();
    assert_eq!(all.rows.len(), 6);
    let unsynced = query_logs(&ro, &filters("kind=logs&limit=10&unsynced=1"), None).unwrap();
    assert_eq!(unsynced.rows.len(), 3, "only the synced=0 rows come back");
    // The count matches the stats watermark for the same table.
    let stat_unsynced: i64 = ro
        .query_row("SELECT count(*) FROM logs WHERE synced = 0", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(unsynced.rows.len() as i64, stat_unsynced);
}

#[test]
fn sessions_carry_counts_and_span() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("logs.db");
    seed(&path);
    // Close the seeded session so the span is computed.
    {
        let w = db::open(&path).unwrap();
        w.execute("UPDATE sessions SET ended_us = 9000 WHERE kind='boot'", [])
            .unwrap();
    }
    let ro = db::open_readonly(&path).unwrap();
    let (rows, _) = query_sessions(&ro, None, None, None, false, 10, None).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].log_count, 5);
    assert_eq!(rows[0].event_count, 1);
    assert_eq!(rows[0].span_us, Some(9000 - 1000));
}
