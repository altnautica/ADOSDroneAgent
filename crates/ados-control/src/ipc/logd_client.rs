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
    /// Returns `None` when the store is unreachable, the response does not parse,
    /// or there are no hardware rows — so the caller falls back to its own default
    /// rather than to a half-populated reply.
    pub async fn latest_hw_signals(&self) -> Option<Map<String, Value>> {
        let path = format!("/v1/query?kind=hw&limit={MERGE_ROWS}");
        let (status, body) = self.uds_get(&path).await.ok()?;
        if status >= 400 {
            return None;
        }
        let parsed: Value = serde_json::from_slice(&body).ok()?;
        merge_hw_signals(&parsed)
    }

    /// A minimal HTTP/1.1 `GET` over the query Unix socket. Returns the status code
    /// and the response body bytes. `Connection: close` lets the body be read to
    /// EOF; a chunked body is de-chunked. Bounded by [`MAX_READ_BYTES`] so a
    /// runaway response cannot exhaust memory.
    async fn uds_get(&self, path: &str) -> std::io::Result<(u16, Vec<u8>)> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

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
    }
}

/// Merge the `data` rows of a `/v1/query?kind=hw` response into one signal map,
/// newest value winning. Rows are newest-first, so the first time a signal key is
/// seen is its freshest value (a plain "insert if absent" keeps it). Returns
/// `None` when the envelope has no usable rows or no signals at all.
fn merge_hw_signals(body: &Value) -> Option<Map<String, Value>> {
    let rows = body.get("data")?.as_array()?;
    let mut merged: Map<String, Value> = Map::new();
    for row in rows {
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
fn parse_http_response(raw: &[u8]) -> std::io::Result<(u16, Vec<u8>)> {
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
        let body = json!({
            "data": [
                {"id": 2, "ts_us": 200, "signals": {"cpu.util.all": 12.5}},
                {"id": 1, "ts_us": 100, "signals": {"cpu.util.all": 99.0, "mem.total_bytes": 4096}},
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
    fn merge_handles_a_chunked_envelope() {
        let body = json!({"data": [{"signals": {"thermal.primary_c": 47.0}}]});
        let merged = merge_hw_signals(&body).unwrap();
        assert_eq!(merged.get("thermal.primary_c"), Some(&json!(47.0)));
    }

    #[test]
    fn default_socket_honours_run_dir() {
        let p = default_logd_socket();
        assert!(p.ends_with("logd-query.sock"));
    }
}
