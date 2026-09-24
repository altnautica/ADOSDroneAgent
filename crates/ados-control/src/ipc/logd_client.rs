//! The logging-store query client.
//!
//! The logging daemon owns a read-only query API on its trusted local Unix socket
//! `/run/ados/logd-query.sock` (no key — on-box trust). The continuous hardware
//! collector samples CPU / memory / disk / temperature into the store, so the
//! status route reads those readings back from the store instead of probing the
//! host itself (which would duplicate the collector). This client is the read
//! side of that seam: a single bounded `GET /v1/query` over the socket, returning
//! the most-recent hardware snapshots merged into one signal map.
//!
//! A store snapshot is sparse per tick (each signal class fires on its own
//! cadence), so a single latest row does not carry every field; the merge folds
//! the most recent handful of snapshots into one map, newest value winning, so a
//! full picture is assembled from the last couple of seconds — the same merge the
//! Python `latest_hw_signals` helper does.
//!
//! When the store is unreachable, returns an error or `None` (not an empty map),
//! so the caller falls back to its own default rather than serving a half-empty
//! reply. Losing the store degrades the status route, never 500s it.

use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

/// The query socket file name under the runtime dir.
pub const LOGD_QUERY_SOCKET_NAME: &str = "logd-query.sock";

/// How many recent hardware snapshots to merge into one signal map. At the
/// collector's base tick this is ~2 s of history — comfortably more than the
/// slowest signal-class cadence, so every field appears at least once in the
/// window. Mirrors the Python helper's merge window.
const MERGE_ROWS: u32 = 20;

/// A hard ceiling on the response read. A `kind=hw&limit=20` page of signal maps
/// is a few KiB; this cap only guards against a runaway response, never a normal
/// read.
const MAX_READ_BYTES: usize = 4 * 1024 * 1024;

/// End-to-end bound on one query exchange (connect, request, read to EOF). The
/// store answers a page in milliseconds; a store that accepts and then stalls
/// must degrade the calling route, not hang it.
pub const QUERY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);

/// How old a hardware row may be and still count as a current reading. The
/// collector samples every couple of seconds; a row older than this is the last
/// thing a stopped collector wrote.
pub const HW_SIGNAL_MAX_AGE_US: i64 = 30_000_000;

/// The default query socket path, honouring the `ADOS_RUN_DIR` override the
/// sibling crates resolve the runtime root with, so a test points it at a tempdir
/// and a dev rig can move the whole `/run/ados` tree. Defaults to
/// `/run/ados/logd-query.sock`.
pub fn default_logd_socket() -> PathBuf {
    let run_dir = std::env::var("ADOS_RUN_DIR").unwrap_or_else(|_| "/run/ados".to_string());
    Path::new(&run_dir).join(LOGD_QUERY_SOCKET_NAME)
}

/// Reads the logging store's query API over its trusted local Unix socket.
///
/// Cheap to clone (just the socket path); the route surface holds one in the app
/// state. Each call opens a short-lived connection — the query API serves
/// `Connection: close`, so there is no connection to pool.
#[derive(Clone, Debug)]
pub struct LogdQueryClient {
    socket_path: PathBuf,
}

impl LogdQueryClient {
    /// Build a client for the given query socket path.
    pub fn new(socket_path: PathBuf) -> Self {
        Self { socket_path }
    }

    /// Build a client at the default query socket path (`ADOS_RUN_DIR`-aware).
    pub fn default_socket() -> Self {
        Self::new(default_logd_socket())
    }

    /// The socket path this client reads from.
    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    /// Merge the most-recent hardware snapshots into one signal map (newest wins).
    ///
    /// Only rows sampled within [`HW_SIGNAL_MAX_AGE_US`] count: the collector
    /// can stop while the query API stays up, and its last rows must not be
    /// served as the node's current readings.
    ///
    /// Returns `None` when the store is unreachable, the response does not parse,
    /// or there are no fresh hardware rows — so the caller falls back to its own
    /// default rather than to a half-populated or stale reply.
    pub async fn latest_hw_signals(&self) -> Option<Map<String, Value>> {
        let parsed = self
            .query_json(
                "/v1/query",
                &[("kind", "hw".into()), ("limit", MERGE_ROWS.to_string())],
            )
            .await?;
        merge_hw_signals(&parsed, unix_now_us())
    }

    /// Read recent hardware snapshots with their timestamps, newest first.
    ///
    /// [`latest_hw_signals`](Self::latest_hw_signals) collapses history into one
    /// map, which answers "what is the value now" and cannot answer "how fast is
    /// this counter moving". Wear diagnosis needs the second question, so this
    /// returns the rows intact and leaves the delta arithmetic to the caller.
    ///
    /// Returns `None` when the store is unreachable or the response does not
    /// parse, so the caller reports "cannot measure" rather than inventing a rate
    /// from a half-read page.
    pub async fn hw_rows(&self, limit: u32) -> Option<Vec<HwRow>> {
        let parsed = self
            .query_json(
                "/v1/query",
                &[("kind", "hw".into()), ("limit", limit.to_string())],
            )
            .await?;
        parse_hw_rows(&parsed)
    }

    /// The newest `limit` rows of one table (`kind`), optionally narrowed to one
    /// `event_kind`: the query response's `data` array, or `None` when the store
    /// is unreachable, answers an error, or the body does not parse.
    pub async fn rows(
        &self,
        kind: &str,
        limit: i64,
        event_kind: Option<&str>,
    ) -> Option<Vec<Value>> {
        let mut params: Vec<(&str, String)> =
            vec![("kind", kind.to_string()), ("limit", limit.to_string())];
        if let Some(ek) = event_kind {
            params.push(("event_kind", ek.to_string()));
        }
        let parsed = self.query_json("/v1/query", &params).await?;
        parsed.get("data").and_then(Value::as_array).cloned()
    }

    /// The `detail` body of the newest event of one `event_kind`, or `None` when
    /// the store is unreachable, holds no such event, or the detail is absent,
    /// not an object, or empty. Carries no age judgement: for events emitted only
    /// on change, where the newest row stays the current state however old it is.
    pub async fn latest_event_detail(&self, event_kind: &str) -> Option<Map<String, Value>> {
        let rows = self.rows("events", 1, Some(event_kind)).await?;
        let detail = rows.first()?.as_object()?.get("detail")?.as_object()?;
        (!detail.is_empty()).then(|| detail.clone())
    }

    /// [`latest_event_detail`](Self::latest_event_detail), but only when the
    /// row was stamped within `max_age`. For events a poll loop writes on a fixed
    /// cadence: the store keeps rows for days, so once the producer dies its last
    /// row would otherwise be served as the current state. A row with no `ts_us`
    /// cannot be shown to be current and is refused; a row stamped slightly in the
    /// future is same-host jitter and counts as fresh.
    pub async fn fresh_event_detail(
        &self,
        event_kind: &str,
        max_age: std::time::Duration,
    ) -> Option<Map<String, Value>> {
        let rows = self.rows("events", 1, Some(event_kind)).await?;
        let row = rows.first()?.as_object()?;
        if !row_is_fresh(row, unix_now_us(), max_age) {
            return None;
        }
        let detail = row.get("detail")?.as_object()?;
        (!detail.is_empty()).then(|| detail.clone())
    }

    /// `GET <endpoint>?<params>` against the query API, decoded as JSON. `None`
    /// on an unreachable store, a 4xx/5xx, a timeout, or an unparseable body.
    pub async fn query_json(&self, endpoint: &str, params: &[(&str, String)]) -> Option<Value> {
        let path = format!("{endpoint}?{}", encode_query(params));
        let (status, body) = self.get(&path).await.ok()?;
        if status >= 400 {
            return None;
        }
        serde_json::from_slice(&body).ok()
    }

    /// A minimal HTTP/1.1 `GET` over the query Unix socket. Returns the status code
    /// and the response body bytes. `Connection: close` lets the body be read to
    /// EOF; a chunked body is de-chunked. Bounded by [`MAX_READ_BYTES`] so a
    /// runaway response cannot exhaust memory, and by [`QUERY_TIMEOUT`] end to
    /// end so a store that accepts and then stalls cannot hang the route.
    pub async fn get(&self, path: &str) -> std::io::Result<(u16, Vec<u8>)> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let exchange = async {
            let mut stream = tokio::net::UnixStream::connect(&self.socket_path).await?;
            let head = format!("GET {path} HTTP/1.1\r\nHost: logd\r\nConnection: close\r\n\r\n");
            stream.write_all(head.as_bytes()).await?;
            stream.flush().await?;

            let mut raw = Vec::new();
            let mut buf = [0u8; 64 * 1024];
            loop {
                let n = stream.read(&mut buf).await?;
                if n == 0 {
                    break; // EOF (Connection: close).
                }
                if raw.len() + n > MAX_READ_BYTES {
                    return Err(std::io::Error::other("logd response too large"));
                }
                raw.extend_from_slice(&buf[..n]);
            }
            parse_http_response(&raw)
        };
        tokio::time::timeout(QUERY_TIMEOUT, exchange)
            .await
            .unwrap_or_else(|_| {
                Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "logd query did not answer in time",
                ))
            })
    }
}

/// Whether a store row's `ts_us` is within `max_age` of `now_us`. A row with no
/// parseable `ts_us` is NOT fresh: an unstamped row cannot be shown to be
/// current. A row stamped slightly in the future is same-host jitter, not an
/// unprovable age, and counts as fresh.
pub(crate) fn row_is_fresh(
    row: &Map<String, Value>,
    now_us: i64,
    max_age: std::time::Duration,
) -> bool {
    let Some(ts_us) = row.get("ts_us").and_then(Value::as_i64) else {
        return false;
    };
    now_us.saturating_sub(ts_us) <= max_age.as_micros() as i64
}

/// Wall-clock microseconds since the epoch, the unit the store stamps rows in.
fn unix_now_us() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_micros() as i64)
        .unwrap_or(0)
}

/// Percent-encode a query-parameter list into a `key=value&...` string.
pub(crate) fn encode_query(params: &[(&str, String)]) -> String {
    params
        .iter()
        .map(|(k, v)| format!("{}={}", percent_encode(k), percent_encode(v)))
        .collect::<Vec<_>>()
        .join("&")
}

/// Conservative percent-encoding: pass through the unreserved set
/// (`A-Za-z0-9-._~`) verbatim and percent-encode every other byte.
pub(crate) fn percent_encode(s: &str) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(b as char);
        } else {
            let _ = write!(out, "%{b:02X}");
        }
    }
    out
}

/// One hardware snapshot as stored: when it was taken, and what it carried.
///
/// The store's own row shape (`ts_us`, `signals`) surfaced intact, because a
/// snapshot's value is only half of what wear diagnosis needs — the other half is
/// when it was taken.
#[derive(Clone, Debug, PartialEq)]
pub struct HwRow {
    /// Sample time, microseconds since the epoch, as the store recorded it.
    pub ts_us: i64,
    /// The signal map for this tick. Sparse: each signal class fires on its own
    /// cadence, so a given key is absent from most rows.
    pub signals: Map<String, Value>,
}

impl HwRow {
    /// This row's value for `key`, if present and numeric.
    pub fn num(&self, key: &str) -> Option<f64> {
        self.signals.get(key).and_then(Value::as_f64)
    }
}

/// Parse the `data` rows of a `/v1/query?kind=hw` response into timestamped rows,
/// dropping any row missing a usable timestamp or signal map. Returns `None` when
/// the envelope itself is unusable, so "malformed response" stays distinguishable
/// from "store is up and has no rows yet".
fn parse_hw_rows(body: &Value) -> Option<Vec<HwRow>> {
    let rows = body.get("data")?.as_array()?;
    let parsed = rows
        .iter()
        .filter_map(|row| {
            let ts_us = row.get("ts_us").and_then(Value::as_i64)?;
            let signals = row.get("signals").and_then(Value::as_object)?.clone();
            Some(HwRow { ts_us, signals })
        })
        .collect();
    Some(parsed)
}

/// Merge the `data` rows of a `/v1/query?kind=hw` response into one signal map,
/// newest value winning. Rows are newest-first, so the first time a signal key is
/// seen is its freshest value (a plain "insert if absent" keeps it). A row with
/// no timestamp, or one older than [`HW_SIGNAL_MAX_AGE_US`] at `now_us`, is not
/// a current reading and is skipped. Returns `None` when no fresh row carries
/// any signal.
fn merge_hw_signals(body: &Value, now_us: i64) -> Option<Map<String, Value>> {
    let rows = body.get("data")?.as_array()?;
    let mut merged: Map<String, Value> = Map::new();
    for row in rows {
        let fresh = row
            .get("ts_us")
            .and_then(Value::as_i64)
            .is_some_and(|ts| now_us.saturating_sub(ts) <= HW_SIGNAL_MAX_AGE_US);
        if !fresh {
            continue;
        }
        let Some(signals) = row.get("signals").and_then(Value::as_object) else {
            continue;
        };
        for (key, value) in signals {
            merged.entry(key.clone()).or_insert_with(|| value.clone());
        }
    }
    if merged.is_empty() {
        None
    } else {
        Some(merged)
    }
}

/// Split a raw HTTP/1.1 response into the status code and the decoded body bytes.
/// De-chunks a `Transfer-Encoding: chunked` body; otherwise returns the body
/// after the header terminator as-is.
pub(crate) fn parse_http_response(raw: &[u8]) -> std::io::Result<(u16, Vec<u8>)> {
    let sep = b"\r\n\r\n";
    let split = raw
        .windows(sep.len())
        .position(|w| w == sep)
        .ok_or_else(|| std::io::Error::other("malformed http response (no header terminator)"))?;
    let head = &raw[..split];
    let body = &raw[split + sep.len()..];

    let head_str = String::from_utf8_lossy(head);
    let status = head_str
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse::<u16>().ok())
        .ok_or_else(|| std::io::Error::other("malformed http status line"))?;

    let chunked = head_str
        .to_ascii_lowercase()
        .contains("transfer-encoding: chunked");
    let body = if chunked {
        de_chunk(body)
    } else {
        body.to_vec()
    };
    Ok((status, body))
}

/// De-chunk a `Transfer-Encoding: chunked` body byte-safely:
/// `<hexlen>\r\n<data>\r\n` repeated until a zero-length chunk.
fn de_chunk(body: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut rest = body;
    while let Some(crlf) = rest.windows(2).position(|w| w == b"\r\n") {
        let len_line = &rest[..crlf];
        let len = usize::from_str_radix(String::from_utf8_lossy(len_line).trim(), 16).unwrap_or(0);
        if len == 0 {
            break;
        }
        let data_start = crlf + 2;
        if rest.len() < data_start + len {
            out.extend_from_slice(&rest[data_start..]);
            break;
        }
        out.extend_from_slice(&rest[data_start..data_start + len]);
        let next = data_start + len;
        rest = if rest.len() >= next + 2 {
            &rest[next + 2..]
        } else {
            &[]
        };
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::UnixListener;

    /// Serve one canned HTTP response on a Unix socket, then exit. Reads the
    /// request line first so the connection is well-formed.
    fn serve_once(listener: UnixListener, response: Vec<u8>) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            if let Ok((mut conn, _addr)) = listener.accept().await {
                // Drain the request head (up to the blank line) so the client's
                // write completes before we reply.
                let mut buf = [0u8; 1024];
                let _ = conn.read(&mut buf).await;
                let _ = conn.write_all(&response).await;
                let _ = conn.flush().await;
            }
        })
    }

    fn http_ok(json_body: &str) -> Vec<u8> {
        format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            json_body.len(),
            json_body
        )
        .into_bytes()
    }

    #[tokio::test]
    async fn merges_signals_newest_first_across_sparse_rows() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("logd-query.sock");
        let listener = UnixListener::bind(&path).unwrap();
        // Newest-first: row 0 (newest) has cpu; row 1 (older) has cpu + mem; the
        // newest cpu must win, mem fills from the older row.
        let now = unix_now_us();
        let body = json!({
            "data": [
                {"id": 2, "ts_us": now, "signals": {"cpu.util.all": 12.5}},
                {"id": 1, "ts_us": now - 1_000_000, "signals": {"cpu.util.all": 99.0, "mem.total_bytes": 4096}},
            ]
        })
        .to_string();
        let server = serve_once(listener, http_ok(&body));

        let client = LogdQueryClient::new(path);
        let merged = client.latest_hw_signals().await.unwrap();
        assert_eq!(merged.get("cpu.util.all"), Some(&json!(12.5)));
        assert_eq!(merged.get("mem.total_bytes"), Some(&json!(4096)));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn absent_socket_is_none() {
        let dir = tempfile::tempdir().unwrap();
        let client = LogdQueryClient::new(dir.path().join("absent.sock"));
        assert!(client.latest_hw_signals().await.is_none());
    }

    #[tokio::test]
    async fn empty_data_is_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("logd-query.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let server = serve_once(listener, http_ok(r#"{"data": []}"#));
        let client = LogdQueryClient::new(path);
        assert!(client.latest_hw_signals().await.is_none());
        server.await.unwrap();
    }

    #[tokio::test]
    async fn a_500_response_is_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("logd-query.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let resp =
            b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                .to_vec();
        let server = serve_once(listener, resp);
        let client = LogdQueryClient::new(path);
        assert!(client.latest_hw_signals().await.is_none());
        server.await.unwrap();
    }

    #[test]
    fn de_chunk_reassembles_a_chunked_body() {
        // "hello world" split across two chunks, then the zero terminator.
        let chunked = b"5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n";
        assert_eq!(de_chunk(chunked), b"hello world");
    }

    #[test]
    fn rows_the_collector_wrote_long_ago_are_not_current_readings() {
        let now = 1_700_000_000_000_000;
        let body = json!({"data": [
            {"ts_us": now - HW_SIGNAL_MAX_AGE_US - 1, "signals": {"thermal.primary_c": 47.0}},
            {"signals": {"cpu.util.all": 5.0}},
        ]});
        assert!(merge_hw_signals(&body, now).is_none());
        let fresh =
            json!({"data": [{"ts_us": now - 1_000_000, "signals": {"thermal.primary_c": 47.0}}]});
        let merged = merge_hw_signals(&fresh, now).unwrap();
        assert_eq!(merged.get("thermal.primary_c"), Some(&json!(47.0)));
    }

    #[tokio::test(start_paused = true)]
    async fn a_store_that_accepts_and_stalls_times_out() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("logd-query.sock");
        let listener = UnixListener::bind(&path).unwrap();
        // Accept and hold the connection open without ever answering.
        let server = tokio::spawn(async move {
            let (conn, _) = listener.accept().await.unwrap();
            tokio::time::sleep(QUERY_TIMEOUT * 10).await;
            drop(conn);
        });
        let client = LogdQueryClient::new(path);
        let err = client.get("/v1/query?kind=hw").await.unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::TimedOut);
        server.abort();
    }

    #[test]
    fn default_socket_honours_run_dir() {
        let p = default_logd_socket();
        assert!(p.ends_with("logd-query.sock"));
    }
}
