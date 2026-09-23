//! The agent-log tail for the diagnostics page, read from the logging store.
//!
//! The logging daemon serves a read-only query API on its trusted local Unix
//! socket. One bounded `GET /v1/query?kind=logs` returns the newest rows,
//! newest first; this turns them into at most [`TAIL_LINES`] display lines,
//! oldest first, each prefixed with its stored level so the page can tint it.
//! The request, the response and every line are bounded, so a wedged or
//! runaway store costs the render loop a timeout, never memory.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

use serde_json::Value;

/// The logging store's query socket.
pub const LOGD_QUERY_SOCKET: &str = "/run/ados/logd-query.sock";

/// How many log lines the tail keeps.
pub const TAIL_LINES: usize = 32;

/// Longest single display line, in characters.
const MAX_LINE_CHARS: usize = 160;

/// Hard ceiling on one response. A 32-row page is a few KiB.
const MAX_RESPONSE_BYTES: u64 = 256 * 1024;

/// Per-read and per-write timeout, matching the panel's agent polls.
const IO_TIMEOUT: Duration = Duration::from_millis(900);

/// Fetch the newest agent log lines, oldest first. `None` when the store is
/// unreachable or answers with anything but a parseable page, so the page keeps
/// showing nothing rather than a guess.
pub fn fetch(socket: &Path) -> Option<Vec<String>> {
    let mut stream = UnixStream::connect(socket).ok()?;
    stream.set_read_timeout(Some(IO_TIMEOUT)).ok()?;
    stream.set_write_timeout(Some(IO_TIMEOUT)).ok()?;
    let request = format!(
        "GET /v1/query?kind=logs&level=info&limit={TAIL_LINES} HTTP/1.1\r\n\
         Host: logd\r\nConnection: close\r\n\r\n"
    );
    stream.write_all(request.as_bytes()).ok()?;
    let mut raw = Vec::new();
    stream.take(MAX_RESPONSE_BYTES).read_to_end(&mut raw).ok()?;
    let body = http_body(&raw)?;
    lines_from_page(&body)
}

/// The body of a `200` response, de-chunked when the response is chunked.
fn http_body(raw: &[u8]) -> Option<Vec<u8>> {
    let split = raw.windows(4).position(|w| w == b"\r\n\r\n")?;
    let head = std::str::from_utf8(&raw[..split]).ok()?;
    let body = &raw[split + 4..];
    let status: u16 = head.split_whitespace().nth(1)?.parse().ok()?;
    if status != 200 {
        return None;
    }
    let chunked = head.lines().any(|l| {
        let l = l.to_ascii_lowercase();
        l.starts_with("transfer-encoding:") && l.contains("chunked")
    });
    if chunked {
        dechunk(body)
    } else {
        Some(body.to_vec())
    }
}

/// Decode an HTTP/1.1 chunked body.
fn dechunk(mut body: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    loop {
        let line_end = body.windows(2).position(|w| w == b"\r\n")?;
        let size_text = std::str::from_utf8(&body[..line_end]).ok()?;
        let size = usize::from_str_radix(size_text.split(';').next()?.trim(), 16).ok()?;
        body = &body[line_end + 2..];
        if size == 0 {
            return Some(out);
        }
        out.extend_from_slice(body.get(..size)?);
        body = body.get(size + 2..)?;
    }
}

/// Turn a query page (`{"data": [row, ...]}`, newest first) into display lines,
/// oldest first: `LEVEL source: message`.
fn lines_from_page(body: &[u8]) -> Option<Vec<String>> {
    let page: Value = serde_json::from_slice(body).ok()?;
    let rows = page.get("data")?.as_array()?;
    let mut lines: Vec<String> = rows
        .iter()
        .take(TAIL_LINES)
        .filter_map(|row| {
            let level = row.get("level")?.as_str()?.to_ascii_uppercase();
            let source = row.get("source").and_then(Value::as_str).unwrap_or("");
            let msg = row.get("msg")?.as_str()?;
            let line = format!("{level} {source}: {msg}");
            Some(line.chars().take(MAX_LINE_CHARS).collect())
        })
        .collect();
    lines.reverse();
    Some(lines)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;

    fn page() -> String {
        serde_json::json!({
            "meta": {"source": "logd"},
            "data": [
                {"id": 3, "ts_us": 3, "source": "ados-radio", "level": "error", "msg": "wfb_tx exited"},
                {"id": 2, "ts_us": 2, "source": "ados-net", "level": "warn", "msg": "uplink flapping"},
                {"id": 1, "ts_us": 1, "source": "ados-supervisor", "level": "info", "msg": "started"}
            ],
            "page": {"next_cursor": null}
        })
        .to_string()
    }

    /// Serve one canned response on a Unix socket and return its path.
    fn serve_once(dir: &Path, response: String) -> std::path::PathBuf {
        let path = dir.join("logd-query.sock");
        let listener = UnixListener::bind(&path).unwrap();
        std::thread::spawn(move || {
            if let Ok((mut s, _)) = listener.accept() {
                let mut buf = [0u8; 1024];
                let _ = s.read(&mut buf);
                let _ = s.write_all(response.as_bytes());
            }
        });
        path
    }

    /// The store answers newest first; the pane shows oldest first, each line
    /// carrying its stored level so the page tints it.
    #[test]
    fn a_query_page_becomes_level_prefixed_lines_oldest_first() {
        let dir = tempfile::tempdir().unwrap();
        let body = page();
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{body}",
            body.len()
        );
        let socket = serve_once(dir.path(), response);
        assert_eq!(
            fetch(&socket).unwrap(),
            vec![
                "INFO ados-supervisor: started".to_string(),
                "WARN ados-net: uplink flapping".to_string(),
                "ERROR ados-radio: wfb_tx exited".to_string(),
            ]
        );
    }

    #[test]
    fn a_chunked_response_is_decoded() {
        let dir = tempfile::tempdir().unwrap();
        let body = page();
        let (a, b) = body.split_at(40);
        let response = format!(
            "HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\n\r\n{:x}\r\n{a}\r\n{:x}\r\n{b}\r\n0\r\n\r\n",
            a.len(),
            b.len()
        );
        let socket = serve_once(dir.path(), response);
        assert_eq!(fetch(&socket).unwrap().len(), 3);
    }

    /// No store, an error status, or a body that is not a page: no lines, never
    /// a fabricated one.
    #[test]
    fn an_unreachable_or_failing_store_yields_nothing() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(fetch(&dir.path().join("absent.sock")), None);
        let socket = serve_once(
            dir.path(),
            "HTTP/1.1 503 Service Unavailable\r\ncontent-length: 0\r\n\r\n".to_string(),
        );
        assert_eq!(fetch(&socket), None);
        assert_eq!(lines_from_page(b"not json"), None);
    }

    #[test]
    fn every_line_is_bounded() {
        let long = "x".repeat(1000);
        let body = serde_json::json!({"data": [{"level": "info", "source": "s", "msg": long}]});
        let lines = lines_from_page(body.to_string().as_bytes()).unwrap();
        assert_eq!(lines[0].chars().count(), MAX_LINE_CHARS);
    }
}
