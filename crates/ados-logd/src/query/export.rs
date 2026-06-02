//! Streamed bulk export.
//!
//! A window export must not buffer the whole result in memory on a small board:
//! rows are read in bounded keyset batches and serialized to newline-delimited
//! JSON, one chunk at a time, so a multi-GB window streams through constant
//! memory. The `jsonl.zst` variant runs the same line serializer through a
//! streaming zstd encoder, sharing the content shape with the explicit
//! cloud-push path so a downloaded window and a pushed window are byte-identical.
//!
//! Reading happens on a blocking thread (rusqlite is synchronous); the bytes
//! are handed to the async response body over a bounded channel, so the read
//! pace is naturally limited by how fast the client consumes the body and the
//! single writer is never blocked (read-only WAL connection).

use std::io::Write;
use std::path::PathBuf;

use serde_json::Value as Json;
use tokio::sync::mpsc;

use super::params::{QueryFilters, Table};
use super::rows::{query_events, query_hw, query_logs, query_metrics};
use crate::db;

/// The output encoding for an export.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// One JSON object per line.
    Jsonl,
    /// The same JSONL stream, zstd-compressed.
    JsonlZst,
}

impl Format {
    /// Parse the `format` selector, defaulting to plain `jsonl`.
    pub fn parse(s: Option<&str>) -> Result<Format, super::params::ParamError> {
        match s.unwrap_or("jsonl") {
            "jsonl" => Ok(Format::Jsonl),
            "jsonl.zst" | "zst" => Ok(Format::JsonlZst),
            other => Err(super::params::ParamError::new(
                "bad_format",
                format!("unknown format '{other}', expected jsonl|jsonl.zst"),
            )),
        }
    }

    /// The response content type.
    pub fn content_type(self) -> &'static str {
        match self {
            Format::Jsonl => "application/x-ndjson",
            Format::JsonlZst => "application/zstd",
        }
    }

    /// A bland suggested filename for the download.
    pub fn filename(self) -> &'static str {
        match self {
            Format::Jsonl => "ados-logs.jsonl",
            Format::JsonlZst => "ados-logs.jsonl.zst",
        }
    }
}

/// How many rows are read per keyset batch while exporting. Bounds the working
/// set; the export keeps paging until the window is exhausted.
const EXPORT_BATCH: u32 = 1000;

/// The zstd level used for `jsonl.zst`. A low level keeps CPU bounded on a small
/// board while still compressing the highly-repetitive JSONL well.
const ZSTD_LEVEL: i32 = 3;

/// Run a streamed export on a blocking thread, sending body chunks over `tx`.
/// The window is read in keyset batches against a fresh read-only connection;
/// each row is serialized to one JSON line; the lines are optionally
/// zstd-compressed. The function returns when the window is exhausted, the
/// receiver is dropped (client disconnected), or an unrecoverable read error
/// occurs (logged; the partial stream simply ends).
pub fn run_export(
    db_path: PathBuf,
    filters: QueryFilters,
    format: Format,
    tx: mpsc::Sender<Vec<u8>>,
) {
    let conn = match db::open_readonly(&db_path) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, "export could not open the store read-only");
            return;
        }
    };

    // Force the batch size for the export read regardless of the request limit.
    let mut batch_filters = filters;
    batch_filters.limit = EXPORT_BATCH;

    let mut sink = ChunkSink::new(format, tx);
    let mut cursor: Option<super::pagination::Cursor> = None;
    let fingerprint = batch_filters.fingerprint();

    loop {
        let result = read_batch(&conn, &batch_filters, cursor.as_ref());
        let (lines, next_key) = match result {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(error = %e, "export read failed; ending stream");
                break;
            }
        };
        for line in &lines {
            if sink.write_line(line).is_err() {
                // The client disconnected; stop reading.
                return;
            }
        }
        match next_key {
            Some((ts_us, id)) => {
                cursor = Some(super::pagination::Cursor::new(ts_us, id, fingerprint));
            }
            None => break,
        }
    }
    let _ = sink.finish();
}

/// One keyset batch of serialized JSON lines plus the next keyset boundary
/// (`None` when the window is exhausted).
type Batch = (Vec<Json>, super::rows::Keyset);

/// Read one keyset batch for the configured table and return the serialized
/// lines plus the next keyset boundary (None when exhausted).
fn read_batch(
    conn: &rusqlite::Connection,
    filters: &QueryFilters,
    after: Option<&super::pagination::Cursor>,
) -> rusqlite::Result<Batch> {
    match filters.table {
        Table::Logs => {
            let page = query_logs(conn, filters, after)?;
            Ok((rows_to_json(&page.rows), page.last_key))
        }
        Table::Events => {
            let page = query_events(conn, filters, after)?;
            Ok((rows_to_json(&page.rows), page.last_key))
        }
        Table::Metrics => {
            let page = query_metrics(conn, filters, after)?;
            Ok((rows_to_json(&page.rows), page.last_key))
        }
        Table::Hw => {
            let page = query_hw(conn, filters, after)?;
            Ok((rows_to_json(&page.rows), page.last_key))
        }
    }
}

/// Serialize a row slice to JSON values; a row that fails to serialize is
/// skipped rather than aborting the export.
fn rows_to_json<T: serde::Serialize>(rows: &[T]) -> Vec<Json> {
    rows.iter()
        .filter_map(|r| serde_json::to_value(r).ok())
        .collect()
}

/// Accumulates serialized JSONL, flushing whole chunks to the channel. For the
/// compressed format the lines feed a streaming zstd encoder whose output is
/// flushed in chunks; `finish` drains the encoder's trailing frame.
struct ChunkSink {
    tx: mpsc::Sender<Vec<u8>>,
    encoder: Option<zstd::stream::Encoder<'static, Vec<u8>>>,
    plain: Vec<u8>,
}

/// Flush the plain sink when it crosses this many bytes, so a chunk is a sane
/// network write size rather than one-line-per-chunk.
const PLAIN_FLUSH_BYTES: usize = 32 * 1024;

impl ChunkSink {
    fn new(format: Format, tx: mpsc::Sender<Vec<u8>>) -> Self {
        let encoder = match format {
            Format::Jsonl => None,
            Format::JsonlZst => Some(
                zstd::stream::Encoder::new(Vec::new(), ZSTD_LEVEL)
                    .expect("zstd encoder init never fails for an in-memory writer"),
            ),
        };
        Self {
            tx,
            encoder,
            plain: Vec::new(),
        }
    }

    /// Serialize one JSON value as a line and buffer/flush it. Returns `Err(())`
    /// when the receiver is gone (client disconnected).
    fn write_line(&mut self, value: &Json) -> Result<(), ()> {
        let mut line = serde_json::to_vec(value).unwrap_or_default();
        line.push(b'\n');
        match &mut self.encoder {
            Some(enc) => {
                // The encoder writes into its inner Vec; flush that Vec out when
                // it grows past a chunk boundary.
                let _ = enc.write_all(&line);
                if enc.get_ref().len() >= PLAIN_FLUSH_BYTES {
                    let drained = std::mem::take(enc.get_mut());
                    self.send(drained)?;
                }
            }
            None => {
                self.plain.extend_from_slice(&line);
                if self.plain.len() >= PLAIN_FLUSH_BYTES {
                    let chunk = std::mem::take(&mut self.plain);
                    self.send(chunk)?;
                }
            }
        }
        Ok(())
    }

    /// Flush any buffered bytes and, for the compressed stream, finalize the
    /// zstd frame so the output is a complete, decodable archive.
    fn finish(mut self) -> Result<(), ()> {
        match self.encoder.take() {
            Some(enc) => {
                // finish() flushes the trailing frame into the inner Vec.
                if let Ok(buf) = enc.finish() {
                    if !buf.is_empty() {
                        self.send(buf)?;
                    }
                }
            }
            None => {
                if !self.plain.is_empty() {
                    let chunk = std::mem::take(&mut self.plain);
                    self.send(chunk)?;
                }
            }
        }
        Ok(())
    }

    /// Blocking-send a chunk to the async body. This runs on the blocking
    /// export thread, so a blocking send is correct and applies natural
    /// backpressure from a slow client.
    fn send(&self, chunk: Vec<u8>) -> Result<(), ()> {
        if chunk.is_empty() {
            return Ok(());
        }
        self.tx.blocking_send(chunk).map_err(|_| ())
    }
}

#[cfg(test)]
mod tests {
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
}
