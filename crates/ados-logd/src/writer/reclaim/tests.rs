use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use ados_protocol::logd::{IngestFrame, Level, LogFrame};
use rusqlite::params;
use tokio::sync::mpsc;

use super::super::{Writer, WriterConfig};
use crate::retention::DELETE_BATCH_ROWS;

const SEED_ROWS: i64 = 3_000;
const FRESH_ROWS: usize = 100;

fn count(writer: &Writer, source: &str) -> i64 {
    writer
        .conn
        .query_row(
            "SELECT count(*) FROM logs WHERE source = ?1",
            params![source],
            |r| r.get(0),
        )
        .unwrap()
}

#[test]
fn writer_emergency_evict_then_retry() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("logs.db");
    let (_tx, rx) = mpsc::channel::<IngestFrame>(4);
    let mut writer = Writer::new(
        &path,
        rx,
        WriterConfig::hermetic(),
        Arc::new(AtomicBool::new(false)),
    )
    .unwrap();

    // Old history the reclaim can evict.
    let seed = "s".repeat(300);
    {
        let tx = writer.conn.transaction().unwrap();
        for i in 0..SEED_ROWS {
            tx.execute(
                "INSERT INTO logs (ts_us, source, level, msg) VALUES (?1, 'seed', 2, ?2)",
                params![1_000 + i, seed],
            )
            .unwrap();
        }
        tx.commit().unwrap();
    }

    // Pin the store at its current size with an empty free list. Any write
    // that needs a new page now fails with SQLITE_FULL, exactly as it does
    // on a full card — a real disk-full, not a mocked error.
    {
        let mut stmt = writer.conn.prepare("PRAGMA incremental_vacuum").unwrap();
        let mut rows = stmt.query([]).unwrap();
        while rows.next().unwrap().is_some() {}
    }
    let pages: i64 = writer
        .conn
        .query_row("PRAGMA page_count", [], |r| r.get(0))
        .unwrap();
    let max: i64 = writer
        .conn
        .query_row(&format!("PRAGMA max_page_count = {pages}"), [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(max, pages, "the page limit is pinned at the current size");

    // A batch far larger than any slack left in the existing pages.
    let msg = "f".repeat(1_000);
    let mut batch: Vec<IngestFrame> = (0..FRESH_ROWS)
        .map(|i| {
            IngestFrame::Log(LogFrame::new(
                2_000_000 + i as i64,
                "fresh",
                Level::Info,
                msg.as_str(),
            ))
        })
        .collect();

    writer
        .commit_batch(&mut batch)
        .expect("the retry after the emergency eviction succeeds");

    assert_eq!(
        writer.emergency_evictions, 1,
        "the first attempt hit the full disk and triggered exactly one eviction"
    );
    assert_eq!(
        count(&writer, "fresh"),
        FRESH_ROWS as i64,
        "the whole batch landed on the retry"
    );
    assert_eq!(
        count(&writer, "seed"),
        SEED_ROWS - DELETE_BATCH_ROWS as i64,
        "exactly one oldest batch was evicted"
    );
    assert!(
        batch.is_empty(),
        "persisted frames were drained to the tail"
    );
}
