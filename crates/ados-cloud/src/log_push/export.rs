//! Per-kind window export: the query-socket reads, the cloud upload and the synced marks.

use super::*;

/// One kind's totals across the windows it was exported in.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct KindOutcome {
    pub(super) pushed: u32,
    pub(super) deduped: u32,
    pub(super) bytes: u64,
}

/// What exporting one window yielded.
pub(super) enum WindowExport {
    /// The window's `jsonl.zst` bytes (empty when it holds no unsynced rows).
    Bytes(Vec<u8>),
    /// The window is over [`MAX_WINDOW_BYTES`]; it must be split.
    TooLarge,
}

/// The store and cloud operations the window loop drives for one kind. A seam
/// so the split-and-push progression is testable without a socket or a cloud.
#[async_trait::async_trait]
pub(super) trait WindowSink {
    /// Export the kind's unsynced rows in `[from_us, to_us)`.
    async fn export(&mut self, from_us: Option<i64>, to_us: i64) -> Result<WindowExport, String>;
    /// The oldest stored timestamp, to split a window with no lower bound.
    async fn oldest_us(&mut self) -> Option<i64>;
    /// Upload one window, then mark exactly that window synced when the cloud
    /// stored it or already had it.
    async fn upload(
        &mut self,
        from_us: Option<i64>,
        to_us: i64,
        bytes: Vec<u8>,
    ) -> Result<IngestDecision, String>;
}

/// Push one kind's unsynced rows in `[since_us, to_us)`.
///
/// A window whose export is over [`MAX_WINDOW_BYTES`] is split at its time
/// midpoint into two consecutive sub-windows, and the earlier one is exported
/// first, until every window fits. Each window is uploaded and marked synced on
/// its own, so a backlog of any size exports oldest first, and a failure part
/// way leaves the windows already sent marked and the rest unsynced for the
/// next push. Empty windows are skipped. Errors: `empty_window` when nothing
/// in the range was unsynced; `window_too_large` when a window cannot be split
/// further (one microsecond wide, or no lower bound is known); otherwise the
/// export or upload code.
pub(super) async fn push_kind_windows<S: WindowSink + Send>(
    sink: &mut S,
    since_us: Option<i64>,
    to_us: i64,
) -> Result<KindOutcome, String> {
    // A stack of pending windows; the later half is pushed first so the
    // earlier half pops (exports) first.
    let mut pending: Vec<(Option<i64>, i64)> = vec![(since_us, to_us)];
    let mut out = KindOutcome::default();
    let mut any_rows = false;
    while let Some((from, to)) = pending.pop() {
        match sink.export(from, to).await? {
            WindowExport::Bytes(bytes) if bytes.is_empty() => {}
            WindowExport::Bytes(bytes) => {
                any_rows = true;
                let size = bytes.len() as u64;
                let decision = sink.upload(from, to, bytes).await?;
                if decision.deduped {
                    out.deduped += 1;
                } else {
                    out.pushed += 1;
                    out.bytes += size;
                }
            }
            WindowExport::TooLarge => {
                let lower = match from {
                    Some(f) => f,
                    None => sink
                        .oldest_us()
                        .await
                        .ok_or_else(|| "window_too_large".to_string())?,
                };
                let mid = lower.saturating_add(to.saturating_sub(lower) / 2);
                if mid <= lower || mid >= to {
                    return Err("window_too_large".to_string());
                }
                pending.push((Some(mid), to));
                pending.push((from, mid));
            }
        }
    }
    if !any_rows {
        return Err("empty_window".to_string());
    }
    Ok(out)
}

/// The production [`WindowSink`]: the logging daemon's trusted local socket for
/// export and mark, the cloud ingest route for upload.
pub(super) struct LiveWindowSink<'a> {
    pub(super) http: &'a reqwest::Client,
    pub(super) base_url: &'a str,
    pub(super) api_key: &'a str,
    pub(super) device_id: &'a str,
    pub(super) kind: &'a str,
    pub(super) session: Option<i64>,
    pub(super) socket: &'a Path,
}

#[async_trait::async_trait]
impl WindowSink for LiveWindowSink<'_> {
    async fn export(&mut self, from_us: Option<i64>, to_us: i64) -> Result<WindowExport, String> {
        // The read is bounded; an over-cap window is reported as such rather
        // than masking as an unreachable store.
        match export_window(self.socket, self.kind, self.session, from_us, to_us).await {
            Ok(bytes) if bytes.len() > MAX_WINDOW_BYTES => Ok(WindowExport::TooLarge),
            Ok(bytes) => Ok(WindowExport::Bytes(bytes)),
            Err(e) if e.to_string().contains("window_too_large") => Ok(WindowExport::TooLarge),
            Err(_) => Err("store_unreachable".to_string()),
        }
    }

    async fn oldest_us(&mut self) -> Option<i64> {
        let (status, body) = uds_request(self.socket, "GET", "/v1/stats", None)
            .await
            .ok()?;
        if !(200..300).contains(&status) {
            return None;
        }
        let stats: serde_json::Value = serde_json::from_slice(&body).ok()?;
        stats.get("oldest_ts_us")?.as_i64()
    }

    async fn upload(
        &mut self,
        from_us: Option<i64>,
        to_us: i64,
        bytes: Vec<u8>,
    ) -> Result<IngestDecision, String> {
        let kind = self.kind;
        let content_hash = window_hash(&bytes);
        let size = bytes.len();
        // The window is `jsonl.zst`: one record per line. Decode locally to
        // count the rows so the cloud window-list records the real count.
        let row_count = count_jsonl_rows(&bytes);

        // Upload in one authenticated binary POST. The cloud recomputes the
        // hash server-side; the local hash only labels the result.
        let url = format!("{}/agent/logd/window", self.base_url.trim_end_matches('/'));
        let resp = self
            .http
            .post(&url)
            .header("X-ADOS-Key", self.api_key)
            .header("X-ADOS-Device", self.device_id)
            .header(
                "X-ADOS-Session",
                self.session.map(|s| s.to_string()).unwrap_or_default(),
            )
            .header("X-ADOS-Kind", kind)
            .header("X-ADOS-Format", "jsonl.zst")
            .header("X-ADOS-Window-Start-Us", from_us.unwrap_or(0).to_string())
            .header("X-ADOS-Window-End-Us", to_us.to_string())
            .header("X-ADOS-Row-Count", row_count.to_string())
            .header("Content-Type", "application/zstd")
            .body(bytes)
            .send()
            .await
            .map_err(|_| "cloud_error".to_string())?;
        if !resp.status().is_success() {
            return Err("cloud_error".to_string());
        }
        let status_field = resp
            .json::<serde_json::Value>()
            .await
            .ok()
            .and_then(|v| v.get("status").and_then(|s| s.as_str()).map(str::to_string))
            .unwrap_or_else(|| "inserted".to_string());
        let decision = IngestDecision::from_status(&status_field);
        tracing::info!(
            kind = %kind, deduped = decision.deduped, hash = %content_hash, bytes = size,
            rows = row_count, "log window uploaded"
        );

        // Mark the exact window synced, only on a stored (or already-present)
        // window. A mark failure is non-fatal: the cloud copy exists, and a
        // later push re-dedupes and re-marks.
        if decision.mark_synced {
            if let Err(e) = mark_synced(self.socket, kind, self.session, from_us, to_us).await {
                tracing::warn!(kind = %kind, error = %e, "mark-synced failed after upload");
            }
        }
        Ok(decision)
    }
}

/// GET the unsynced export window from the logging daemon over its local socket,
/// returning the raw `jsonl.zst` bytes. The window is `[since_us, to_us)`,
/// optionally scoped to one session, restricted to the rows not yet marked
/// synced — byte-identical to what the upload sends.
pub(super) async fn export_window(
    socket: &Path,
    kind: &str,
    session: Option<i64>,
    since_us: Option<i64>,
    to_us: i64,
) -> std::io::Result<Vec<u8>> {
    let mut query = format!("kind={kind}&unsynced=1&format=jsonl.zst&to={to_us}");
    if let Some(s) = session {
        query.push_str(&format!("&session={s}"));
    }
    if let Some(f) = since_us {
        query.push_str(&format!("&from={f}"));
    }
    let path = format!("/v1/export?{query}");
    let (status, body) = uds_request(socket, "GET", &path, None).await?;
    if !(200..300).contains(&status) {
        return Err(std::io::Error::other(format!(
            "logd export returned status {status}"
        )));
    }
    Ok(body)
}

/// POST `/v1/synced` over the local socket to flip the exported rows to synced.
/// The marked window MUST equal the exported window, so the same selector is
/// used: the session, the lower bound, and the closed upper bound.
pub(super) async fn mark_synced(
    socket: &Path,
    kind: &str,
    session: Option<i64>,
    since_us: Option<i64>,
    to_us: i64,
) -> std::io::Result<()> {
    let body = serde_json::json!({
        "session": session,
        "from_us": since_us,
        "to_us": to_us,
        "tables": [kind],
    });
    let body_bytes = serde_json::to_vec(&body).unwrap_or_default();
    let (status, _resp) = uds_request(socket, "POST", "/v1/synced", Some(&body_bytes)).await?;
    if !(200..300).contains(&status) {
        return Err(std::io::Error::other(format!(
            "logd mark-synced returned status {status}"
        )));
    }
    Ok(())
}

/// A minimal binary-safe HTTP/1.1 request over a Unix socket. The logging daemon
/// is reached only on its trusted local socket (the mark endpoint is socket-only
/// by design), and the export body is binary `jsonl.zst`, so a byte-safe reader
/// is used rather than a string one. `Connection: close` lets the body be read
/// to EOF; a chunked body is de-chunked.
pub(super) async fn uds_request(
    socket: &Path,
    method: &str,
    path: &str,
    body: Option<&[u8]>,
) -> std::io::Result<(u16, Vec<u8>)> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let mut stream = tokio::net::UnixStream::connect(socket).await?;
    let mut head = format!("{method} {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n");
    if let Some(b) = body {
        head.push_str("Content-Type: application/json\r\n");
        head.push_str(&format!("Content-Length: {}\r\n", b.len()));
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes()).await?;
    if let Some(b) = body {
        stream.write_all(b).await?;
    }
    stream.flush().await?;

    // Read the response with a hard memory ceiling. A bare `read_to_end` would
    // buffer the whole body before any size check, so a no-`from` window over a
    // multi-day backlog could exhaust memory on a small board before the cap was
    // ever consulted. Read in a loop and abort the moment the accumulated bytes
    // exceed [`MAX_READ_BYTES`], dropping the connection instead.
    let mut raw = Vec::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = stream.read(&mut buf).await?;
        if n == 0 {
            break; // EOF (Connection: close).
        }
        if raw.len() + n > MAX_READ_BYTES {
            return Err(std::io::Error::other("window_too_large"));
        }
        raw.extend_from_slice(&buf[..n]);
    }
    parse_http_response(raw)
}

/// Split a raw HTTP/1.1 response into the status code and the decoded body
/// bytes, reusing `raw`'s allocation: the header is drained off the front and a
/// `Transfer-Encoding: chunked` body is de-chunked in place, so a window is
/// never held twice.
pub(super) fn parse_http_response(mut raw: Vec<u8>) -> std::io::Result<(u16, Vec<u8>)> {
    let sep = b"\r\n\r\n";
    let split = raw
        .windows(sep.len())
        .position(|w| w == sep)
        .ok_or_else(|| std::io::Error::other("malformed http response (no header terminator)"))?;
    let (status, chunked) = {
        let head_str = String::from_utf8_lossy(&raw[..split]);
        let status = head_str
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .and_then(|code| code.parse::<u16>().ok())
            .ok_or_else(|| std::io::Error::other("malformed http status line"))?;
        let chunked = head_str
            .to_ascii_lowercase()
            .contains("transfer-encoding: chunked");
        (status, chunked)
    };
    raw.drain(..split + sep.len());
    if chunked {
        de_chunk_in_place(&mut raw);
    }
    Ok((status, raw))
}

/// De-chunk a `Transfer-Encoding: chunked` body byte-safely and in place:
/// `<hexlen>\r\n<data>\r\n` repeated until a zero-length chunk. The decoded
/// data is compacted toward the front of `body`, which is then truncated.
pub(super) fn de_chunk_in_place(body: &mut Vec<u8>) {
    let mut read = 0;
    let mut write = 0;
    // The chunk-size line ends at the first CRLF; an absent CRLF ends the body.
    while let Some(crlf) = body[read..].windows(2).position(|w| w == b"\r\n") {
        let len_line = &body[read..read + crlf];
        let len = usize::from_str_radix(String::from_utf8_lossy(len_line).trim(), 16).unwrap_or(0);
        if len == 0 {
            break;
        }
        let data_start = read + crlf + 2;
        // A truncated final chunk keeps what is present and stops.
        let data_end = (data_start + len).min(body.len());
        body.copy_within(data_start..data_end, write);
        write += data_end - data_start;
        if data_end < data_start + len {
            break;
        }
        // Skip the trailing CRLF after the chunk data.
        read = (data_end + 2).min(body.len());
    }
    body.truncate(write);
}

/// Write the result file atomically (write a temp sibling, then rename) so a
/// reader never sees a partial file.
pub(super) fn write_result(path: &Path, result: &PushResult) {
    let Ok(json) = serde_json::to_vec(result) else {
        return;
    };
    let tmp = path.with_extension("json.tmp");
    if std::fs::write(&tmp, &json).is_ok() {
        let _ = std::fs::rename(&tmp, path);
    }
}
