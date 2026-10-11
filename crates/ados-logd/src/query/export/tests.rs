use super::*;
use crate::query::params::QueryParams;
use ados_protocol::logd::Level;
use std::io::Read;

fn seed(path: &std::path::Path, n: i64) {
    let conn = db::open(path).unwrap();
    conn.execute(
        "INSERT INTO sessions (started_us, kind) VALUES (0, 'boot')",
        [],
    )
    .unwrap();
    let s = conn.last_insert_rowid();
    for i in 0..n {
        conn.execute(
            "INSERT INTO logs (ts_us, session, source, level, msg) VALUES (?1, ?2, 'api', 2, ?3)",
            rusqlite::params![i, s, format!("line {i}")],
        )
        .unwrap();
    }
    let _ = Level::Info;
}

fn filters(q: &str) -> QueryFilters {
    QueryFilters::parse(&QueryParams::parse(q), 0).unwrap()
}

/// Drive an export to completion on a thread, collecting the body bytes.
fn collect_export(path: PathBuf, filters: QueryFilters, format: Format) -> Vec<u8> {
    let (tx, mut rx) = mpsc::channel::<Vec<u8>>(8);
    let handle = std::thread::spawn(move || run_export(path, filters, format, tx));
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let mut body = Vec::new();
    rt.block_on(async {
        while let Some(chunk) = rx.recv().await {
            body.extend_from_slice(&chunk);
        }
    });
    handle.join().unwrap();
    body
}

#[test]
fn plain_jsonl_streams_every_row_one_per_line() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("logs.db");
    // More than one keyset batch so paging is exercised.
    seed(&path, (EXPORT_BATCH as i64) + 250);
    let body = collect_export(path, filters("kind=logs"), Format::Jsonl);
    let text = String::from_utf8(body).unwrap();
    let lines: Vec<&str> = text.lines().filter(|l| !l.is_empty()).collect();
    assert_eq!(lines.len() as i64, (EXPORT_BATCH as i64) + 250);
    // Each line is a JSON object carrying the log shape.
    let first: Json = serde_json::from_str(lines[0]).unwrap();
    assert!(first.get("msg").is_some());
    assert!(first.get("source").is_some());
}

#[test]
fn zstd_export_decompresses_to_the_same_lines() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("logs.db");
    seed(&path, 50);
    let body = collect_export(path, filters("kind=logs"), Format::JsonlZst);
    // Decompress and check the line count round-trips.
    let mut decoder = zstd::stream::Decoder::new(&body[..]).unwrap();
    let mut text = String::new();
    decoder.read_to_string(&mut text).unwrap();
    let lines: Vec<&str> = text.lines().filter(|l| !l.is_empty()).collect();
    assert_eq!(lines.len(), 50);
}

#[test]
fn export_stops_when_the_client_disconnects() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("logs.db");
    seed(&path, (EXPORT_BATCH as i64) * 3);
    let (tx, rx) = mpsc::channel::<Vec<u8>>(1);
    // Drop the receiver immediately: the export must end without panicking.
    drop(rx);
    let handle =
        std::thread::spawn(move || run_export(path, filters("kind=logs"), Format::Jsonl, tx));
    handle.join().unwrap();
}
