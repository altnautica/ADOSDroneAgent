//! Writer run-loop and control-plane tests.
//!
//! These drive the writer on a real dedicated thread (the production threading
//! model) over the bounded channel, and exercise the mark-synced flip directly
//! against a seeded store, so both the loop wiring and the single-writer control
//! path are covered.

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{mpsc, oneshot};

use ados_protocol::logd::{
    EventFrame, HwSnapshot, IngestFrame, Level, LogFrame, SyncRequest, SyncTable, TelemetryFrame,
};
use rmpv::Value as MpVal;
use rusqlite::Connection;

use super::control::apply_mark_synced;
use super::{now_us, ControlMsg, MarkResult, Writer, WriterConfig};
use crate::db;
use crate::retention::RetentionConfig;

fn temp_db() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("logs.db");
    (dir, path)
}

/// Spawn the writer on a real dedicated thread (the production threading
/// model), feed it frames over the bounded channel, and return after the
/// thread has committed and exited so the DB is safe to read.
fn run_writer_to_completion(path: &Path, config: WriterConfig, frames: Vec<IngestFrame>) {
    let (tx, rx) = mpsc::channel::<IngestFrame>(64);
    let writer = Writer::new(path, rx, config, Arc::new(AtomicBool::new(false))).unwrap();
    let handle = std::thread::spawn(move || writer.run().unwrap());
    // A small blocking runtime feeds the async-side sender from this test
    // thread; the writer itself is the blocking thread above.
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        for f in frames {
            tx.send(f).await.unwrap();
        }
        // Dropping the sender closes the channel, which ends the writer.
        drop(tx);
    });
    handle.join().unwrap();
}

#[test]
fn writer_inserts_each_frame_into_the_right_table() {
    let (_dir, path) = temp_db();
    let mut log = LogFrame::new(1_000, "test-src", Level::Warn, "a message");
    log.target = Some("mod::path".to_string());
    log.fields.insert("attempt".to_string(), MpVal::from(3u64));
    let frames = vec![
        IngestFrame::Log(log),
        IngestFrame::Telemetry(TelemetryFrame::new(1_001, "cpu.load", 0.5)),
        IngestFrame::Event(EventFrame::new(
            1_002,
            "radio.lock",
            "test-src",
            Level::Info,
        )),
        IngestFrame::Hw({
            let mut h = HwSnapshot::new(1_003);
            h.signals
                .insert("thermal.soc_c".to_string(), MpVal::from(42.0));
            h
        }),
    ];
    run_writer_to_completion(&path, WriterConfig::default(), frames);

    let ro = db::open_readonly(&path).unwrap();
    for (table, want) in [("logs", 1), ("metrics", 1), ("events", 1), ("hw", 1)] {
        let n: i64 = ro
            .query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, want, "{table} row count");
    }
    // Every data row is attributed to the boot session.
    let boot_open: i64 = ro
        .query_row(
            "SELECT count(*) FROM logs WHERE session = (SELECT id FROM sessions WHERE kind='boot')",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(boot_open, 1);
    // A log that carries only a non-secret field is not flagged redacted:
    // the flag tracks an actual redaction, not the mere presence of fields.
    let redacted: i64 = ro
        .query_row("SELECT redacted FROM logs", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        redacted, 0,
        "a non-secret field must not flag the row redacted"
    );
    db::integrity_check(&ro).unwrap();
}

#[test]
fn secret_fields_are_redacted_before_insert() {
    let (_dir, path) = temp_db();
    let mut log = LogFrame::new(2_000, "test-src", Level::Info, "with a secret");
    log.fields
        .insert("api_key".to_string(), MpVal::from("ABCDEFGHIJ1234567890"));
    run_writer_to_completion(&path, WriterConfig::default(), vec![IngestFrame::Log(log)]);

    let ro = db::open_readonly(&path).unwrap();
    let (fields_blob, redacted): (Vec<u8>, i64) = ro
        .query_row(
            "SELECT fields, redacted FROM logs WHERE source='test-src'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(redacted, 1);
    let fields: ados_protocol::logd::Fields = rmp_serde::from_slice(&fields_blob).unwrap();
    let api_key = fields.get("api_key").and_then(|v| v.as_str()).unwrap();
    assert!(
        api_key.starts_with("redacted:"),
        "api_key must be redacted on disk: {api_key}"
    );
}

#[test]
fn size_boundary_commits_at_max_rows() {
    // A tiny batch cap and a long time bound: the writer must commit on the
    // size boundary, not the timer. Feed exactly two full batches.
    let (_dir, path) = temp_db();
    let config = WriterConfig {
        batch_max_rows: 5,
        batch_max: Duration::from_secs(3600),
        checkpoint_interval_frames: 1_000_000,
        ..WriterConfig::default()
    };
    let frames: Vec<IngestFrame> = (0..10)
        .map(|i| IngestFrame::Telemetry(TelemetryFrame::new(i, "cpu.load", i as f64)))
        .collect();
    run_writer_to_completion(&path, config, frames);

    let ro = db::open_readonly(&path).unwrap();
    let n: i64 = ro
        .query_row("SELECT count(*) FROM metrics", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 10);
}

#[test]
fn time_boundary_commits_a_partial_batch() {
    // A large size cap and a short time bound: a single frame must still be
    // committed once the time boundary passes (it never waits for the cap).
    let (_dir, path) = temp_db();
    let config = WriterConfig {
        batch_max_rows: 10_000,
        batch_max: Duration::from_millis(20),
        checkpoint_interval_frames: 1_000_000,
        ..WriterConfig::default()
    };
    run_writer_to_completion(
        &path,
        config,
        vec![IngestFrame::Telemetry(TelemetryFrame::new(
            1, "cpu.load", 1.0,
        ))],
    );
    let ro = db::open_readonly(&path).unwrap();
    let n: i64 = ro
        .query_row("SELECT count(*) FROM metrics", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 1);
}

#[test]
fn arm_event_opens_a_flight_session_and_disarm_closes_it() {
    let (_dir, path) = temp_db();
    let mut arm = EventFrame::new(3_000, "state", "test-src", Level::Info);
    arm.detail.insert("reason".to_string(), MpVal::from("arm"));
    // A log emitted while armed must carry the flight session.
    let mid = IngestFrame::Log(LogFrame::new(3_001, "test-src", Level::Info, "in flight"));
    let mut disarm = EventFrame::new(3_002, "state", "test-src", Level::Info);
    disarm
        .detail
        .insert("reason".to_string(), MpVal::from("disarm"));
    run_writer_to_completion(
        &path,
        WriterConfig::default(),
        vec![IngestFrame::Event(arm), mid, IngestFrame::Event(disarm)],
    );

    let ro = db::open_readonly(&path).unwrap();
    let (flight_id, ended): (i64, Option<i64>) = ro
        .query_row(
            "SELECT id, ended_us FROM sessions WHERE kind='flight'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert!(ended.is_some(), "flight session must be closed on disarm");
    // The in-flight log row is attributed to the flight session.
    let in_flight: i64 = ro
        .query_row(
            "SELECT count(*) FROM logs WHERE session = ?1 AND msg = 'in flight'",
            [flight_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(in_flight, 1);
}

#[test]
fn shutdown_closes_the_boot_session_with_reason_shutdown() {
    let (_dir, path) = temp_db();
    run_writer_to_completion(
        &path,
        WriterConfig::default(),
        vec![IngestFrame::Telemetry(TelemetryFrame::new(1, "m", 1.0))],
    );
    let ro = db::open_readonly(&path).unwrap();
    let (ended, reason): (Option<i64>, Option<String>) = ro
        .query_row(
            "SELECT ended_us, reason FROM sessions WHERE kind='boot'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert!(ended.is_some());
    assert_eq!(reason.as_deref(), Some("shutdown"));
}

#[test]
fn periodic_checkpoint_keeps_the_store_intact() {
    // A checkpoint every frame exercises the WAL-truncate path many times;
    // the store stays consistent and the integrity check passes.
    let (_dir, path) = temp_db();
    let config = WriterConfig {
        batch_max_rows: 1,
        batch_max: Duration::from_secs(3600),
        checkpoint_interval_frames: 1,
        ..WriterConfig::default()
    };
    let frames: Vec<IngestFrame> = (0..20)
        .map(|i| IngestFrame::Telemetry(TelemetryFrame::new(i, "cpu.load", i as f64)))
        .collect();
    run_writer_to_completion(&path, config, frames);
    let ro = db::open_readonly(&path).unwrap();
    let n: i64 = ro
        .query_row("SELECT count(*) FROM metrics", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 20);
    db::integrity_check(&ro).unwrap();
}

/// Drive the writer through its real run loop with a near-immediate
/// maintenance interval and a forced size cap, holding the channel open long
/// enough for at least one maintenance pass to fire while the writer is idle.
/// The pass must run on the writer's own connection (no second connection
/// exists), evict the oldest rows down toward the cap, and write a
/// `retention.evicted` event row into the store. Proves the retention path is
/// wired through the writer loop, not only callable as a free function.
#[test]
fn maintenance_runs_on_the_writer_thread_and_records_an_eviction_event() {
    let (_dir, path) = temp_db();
    // Maintenance fires almost immediately and then every 50 ms; the size cap
    // is floored, and the low-water target is half the cap so eviction frees
    // a real chunk. The vacuum interval is long so the only vacuum is the
    // post-eviction one.
    let retention = RetentionConfig {
        maintenance_interval: Duration::from_millis(20),
        vacuum_interval: Duration::from_secs(3600),
        max_bytes: crate::retention::MIN_MAX_BYTES,
        low_water_ratio: 0.5,
        ..RetentionConfig::default()
    };
    let config = WriterConfig {
        batch_max_rows: 4096,
        batch_max: Duration::from_millis(20),
        retention,
        ..WriterConfig::default()
    };

    // A bounded channel small enough that the writer must be draining for the
    // sends to make progress (proving the loop runs), large enough not to
    // deadlock the feed.
    let (tx, rx) = mpsc::channel::<IngestFrame>(4096);
    let writer = Writer::new(&path, rx, config, Arc::new(AtomicBool::new(false))).unwrap();
    let handle = std::thread::spawn(move || writer.run().unwrap());

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let total: i64 = 70_000;
    rt.block_on(async {
        // Push enough fat log rows that the store crosses the floored cap, so
        // the idle maintenance pass has something to evict.
        let base = now_us();
        for i in 0..total {
            let mut log = LogFrame::new(base + i * 1_000, "bulk", Level::Info, "x".repeat(512));
            log.target = Some("t".to_string());
            tx.send(IngestFrame::Log(log)).await.unwrap();
        }
        // Hold the channel open while the idle maintenance timer fires, then
        // close it so the writer drains and exits.
        tokio::time::sleep(Duration::from_millis(500)).await;
        drop(tx);
    });
    handle.join().unwrap();

    let ro = db::open_readonly(&path).unwrap();
    // The seed is sized to exceed the floored cap, so the writer's own
    // maintenance pass must have evicted and recorded the event.
    let evictions: i64 = ro
        .query_row(
            "SELECT count(*) FROM events WHERE kind='retention.evicted'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(
        evictions >= 1,
        "the writer recorded a retention.evicted event on its own thread"
    );
    // The event carries a positive row count and the freed span in its blob.
    let detail_blob: Vec<u8> = ro
        .query_row(
            "SELECT detail FROM events WHERE kind='retention.evicted' \
             ORDER BY id LIMIT 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let detail: ados_protocol::logd::Fields = rmp_serde::from_slice(&detail_blob).unwrap();
    let rows = detail.get("rows").and_then(|v| v.as_u64()).unwrap();
    assert!(rows > 0, "the eviction event reports the rows it freed");
    assert!(
        detail.contains_key("from_us") && detail.contains_key("to_us"),
        "the eviction event reports the freed span"
    );
    // Bulk rows were removed.
    let remaining: i64 = ro
        .query_row("SELECT count(*) FROM logs WHERE source='bulk'", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert!(remaining < total, "the size cap removed rows");
    db::integrity_check(&ro).unwrap();
}

/// Seed a store directly with a mix of synced/unsynced rows across all four
/// tables, then call `apply_mark_synced` against it and return the result so
/// the flip can be asserted without spinning up the whole writer loop.
fn seed_unsynced(path: &Path) {
    let conn = db::open(path).unwrap();
    conn.execute(
        "INSERT INTO sessions (started_us, kind) VALUES (1000, 'boot')",
        [],
    )
    .unwrap();
    let session = conn.last_insert_rowid();
    // logs at ts 2000..2005, all unsynced.
    for i in 0..6i64 {
        conn.execute(
            "INSERT INTO logs (ts_us, session, source, level, msg, synced) \
             VALUES (?1, ?2, 'api', 2, ?3, 0)",
            rusqlite::params![2000 + i, session, format!("m{i}")],
        )
        .unwrap();
    }
    // a couple of metrics, events, and one hw row, all unsynced.
    for i in 0..3i64 {
        conn.execute(
            "INSERT INTO metrics (ts_us, session, metric, value, synced) \
             VALUES (?1, ?2, 'cpu.load', 0.5, 0)",
            rusqlite::params![2100 + i, session],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO events (ts_us, session, kind, source, severity, synced) \
             VALUES (?1, ?2, 'radio.lock', 'ados-radio', 2, 0)",
            rusqlite::params![2200 + i, session],
        )
        .unwrap();
    }
    conn.execute(
        "INSERT INTO hw (ts_us, session, signals, synced) VALUES (2300, ?1, ?2, 0)",
        rusqlite::params![
            session,
            rmp_serde::to_vec_named(&ados_protocol::logd::Fields::new()).unwrap()
        ],
    )
    .unwrap();
}

fn unsynced_count(conn: &Connection, table: &str) -> i64 {
    conn.query_row(
        &format!("SELECT count(*) FROM {table} WHERE synced = 0"),
        [],
        |r| r.get(0),
    )
    .unwrap()
}

#[test]
fn mark_synced_flips_exactly_the_window() {
    let (_dir, path) = temp_db();
    seed_unsynced(&path);
    let conn = db::open(&path).unwrap();
    // Mark only the logs in [2000, 2003): rows at 2000, 2001, 2002 (three).
    let req = SyncRequest {
        session: None,
        from_us: Some(2000),
        to_us: Some(2003),
        tables: vec![SyncTable::Logs],
    };
    let res = apply_mark_synced(&conn, &req).unwrap();
    assert_eq!(res.marked.get("logs"), Some(&3));
    // The remaining logs (2003, 2004, 2005) are still unsynced; the other
    // tables are untouched.
    assert_eq!(res.unsynced_after.get("logs"), Some(&3));
    assert_eq!(unsynced_count(&conn, "logs"), 3);
    assert_eq!(unsynced_count(&conn, "metrics"), 3);
    assert_eq!(unsynced_count(&conn, "events"), 3);
    assert_eq!(unsynced_count(&conn, "hw"), 1);
    db::integrity_check(&conn).unwrap();
}

#[test]
fn mark_synced_empty_tables_marks_all_four() {
    let (_dir, path) = temp_db();
    seed_unsynced(&path);
    let conn = db::open(&path).unwrap();
    // An empty selector marks every unsynced row in all four tables.
    let res = apply_mark_synced(&conn, &SyncRequest::default()).unwrap();
    assert_eq!(res.marked.get("logs"), Some(&6));
    assert_eq!(res.marked.get("metrics"), Some(&3));
    assert_eq!(res.marked.get("events"), Some(&3));
    assert_eq!(res.marked.get("hw"), Some(&1));
    for t in ["logs", "metrics", "events", "hw"] {
        assert_eq!(res.unsynced_after.get(t), Some(&0));
        assert_eq!(unsynced_count(&conn, t), 0);
    }
}

#[test]
fn mark_synced_writes_a_pushed_window_event() {
    // Drive the real run loop: open the channels, mark a window over the
    // control channel, await the ack, and confirm a durable
    // `blackbox.pushed_window` event landed (proving the write went through
    // the writer's own connection) while the store stays consistent.
    let (_dir, path) = temp_db();
    let (tx, rx) = mpsc::channel::<IngestFrame>(64);
    let writer = Writer::new(
        &path,
        rx,
        WriterConfig::default(),
        Arc::new(AtomicBool::new(false)),
    )
    .unwrap();
    let control = writer.control_handle();
    let handle = std::thread::spawn(move || writer.run().unwrap());

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        // Ingest a few rows first so there is something to mark.
        for i in 0..4i64 {
            tx.send(IngestFrame::Telemetry(TelemetryFrame::new(
                1000 + i,
                "cpu.load",
                i as f64,
            )))
            .await
            .unwrap();
        }
        // Give the writer a moment to commit them, then mark all metrics.
        tokio::time::sleep(Duration::from_millis(50)).await;
        let (ack_tx, ack_rx) = oneshot::channel::<MarkResult>();
        control
            .send(ControlMsg::MarkSynced {
                req: SyncRequest {
                    tables: vec![SyncTable::Metrics],
                    ..SyncRequest::default()
                },
                ack: ack_tx,
            })
            .await
            .unwrap();
        let res = tokio::time::timeout(Duration::from_secs(5), ack_rx)
            .await
            .expect("the writer acknowledged in time")
            .expect("the writer did not drop the ack");
        assert!(res.marked.get("metrics").copied().unwrap_or(0) >= 1);
        drop(tx);
    });
    handle.join().unwrap();

    let ro = db::open_readonly(&path).unwrap();
    let count: i64 = ro
        .query_row(
            "SELECT count(*) FROM events WHERE kind='blackbox.pushed_window'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count, 1, "exactly one pushed-window event was recorded");
    // The event detail records a positive marked-row count.
    let detail_blob: Vec<u8> = ro
        .query_row(
            "SELECT detail FROM events WHERE kind='blackbox.pushed_window'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let detail: ados_protocol::logd::Fields = rmp_serde::from_slice(&detail_blob).unwrap();
    assert!(
        detail.get("rows").and_then(|v| v.as_i64()).unwrap_or(0) >= 1,
        "the pushed-window event reports the rows it marked"
    );
    assert!(
        detail.contains_key("tables"),
        "it records the tables marked"
    );
    db::integrity_check(&ro).unwrap();
}

#[test]
fn mark_synced_ack_returns_while_ingest_flows() {
    // Drive the run loop with a steady ingest stream and a concurrent
    // mark-synced. The ack must come back (the control channel is not starved
    // by ingest) and the ingested frames must still land (the mark does not
    // stall ingest).
    let (_dir, path) = temp_db();
    let (tx, rx) = mpsc::channel::<IngestFrame>(64);
    let writer = Writer::new(
        &path,
        rx,
        WriterConfig::default(),
        Arc::new(AtomicBool::new(false)),
    )
    .unwrap();
    let control = writer.control_handle();
    let handle = std::thread::spawn(move || writer.run().unwrap());

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let total: i64 = 200;
    rt.block_on(async {
        let base = now_us();
        for i in 0..total {
            tx.send(IngestFrame::Telemetry(TelemetryFrame::new(
                base + i,
                "cpu.load",
                i as f64,
            )))
            .await
            .unwrap();
            // Halfway through the stream, fire a mark and await the ack while
            // ingest is still flowing.
            if i == total / 2 {
                let (ack_tx, ack_rx) = oneshot::channel::<MarkResult>();
                control
                    .send(ControlMsg::MarkSynced {
                        req: SyncRequest::default(),
                        ack: ack_tx,
                    })
                    .await
                    .unwrap();
                let res = tokio::time::timeout(Duration::from_secs(5), ack_rx)
                    .await
                    .expect("the ack returned while ingest was flowing")
                    .expect("the writer did not drop the ack");
                // The mark covered whatever had been committed by then.
                assert!(res.unsynced_after.contains_key("metrics"));
            }
        }
        drop(tx);
    });
    handle.join().unwrap();

    let ro = db::open_readonly(&path).unwrap();
    let n: i64 = ro
        .query_row("SELECT count(*) FROM metrics", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, total, "every ingested frame still landed");
    db::integrity_check(&ro).unwrap();
}
