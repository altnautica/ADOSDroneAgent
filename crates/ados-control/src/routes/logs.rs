//! `GET /api/logs` — recent log entries from the durable store, in the
//! `{seq, timestamp, level, logger, message}` entry shape the GCS log tier and
//! the relayed-read path consume.
//!
//! The store answers newest first. The route asks for enough rows to cover the
//! `offset` window (capped at [`MAX_LIMIT`]) and pages here, so the caller keeps
//! its offset/limit contract. An unreachable or failing store is an empty page
//! with a `warning` naming why, not a `500`: log history is observability, not
//! flight.

use axum::extract::Query;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

use crate::ipc::logd_client::LogdQueryClient;

/// Hard ceiling on the page the route will request from the store.
pub const MAX_LIMIT: i64 = 1000;

#[derive(Debug, Deserialize)]
pub struct LogsQuery {
    #[serde(default)]
    pub level: Option<String>,
    #[serde(default)]
    pub service: Option<String>,
    #[serde(default = "default_limit")]
    pub limit: i64,
    #[serde(default)]
    pub offset: i64,
}

fn default_limit() -> i64 {
    50
}

/// `GET /api/logs?level=&service=&limit=&offset=`.
pub async fn get_logs(Query(q): Query<LogsQuery>) -> Response {
    logs_from(&LogdQueryClient::default_socket(), q).await
}

async fn logs_from(client: &LogdQueryClient, q: LogsQuery) -> Response {
    if !(1..=MAX_LIMIT).contains(&q.limit) || q.offset < 0 {
        return crate::routes::detail(
            axum::http::StatusCode::UNPROCESSABLE_ENTITY,
            format!("limit must be 1..={MAX_LIMIT} and offset must be >= 0"),
        );
    }
    let level = q.level.as_deref().filter(|l| !l.is_empty());
    let service = q.service.as_deref().filter(|s| !s.is_empty());
    let want = (q.offset + q.limit).min(MAX_LIMIT);
    let mut params: Vec<(&str, String)> =
        vec![("kind", "logs".to_string()), ("limit", want.to_string())];
    if let Some(l) = level {
        params.push(("level", l.to_ascii_lowercase()));
    }
    if let Some(s) = service {
        params.push(("source", s.to_string()));
    }
    let path = format!(
        "/v1/query?{}",
        crate::ipc::logd_client::encode_query(&params)
    );
    let empty = |warning: String| {
        Json(json!({
            "entries": [],
            "total": 0,
            "limit": q.limit,
            "offset": q.offset,
            "warning": warning,
        }))
        .into_response()
    };
    let (status, body) = match client.get(&path).await {
        Ok(r) => r,
        Err(_) => return empty("logging store unavailable".to_string()),
    };
    if status >= 400 {
        return empty(format!("logging store returned {status}"));
    }
    let Ok(parsed) = serde_json::from_slice::<Value>(&body) else {
        return empty("logging store response was not JSON".to_string());
    };
    let rows = parsed
        .get("data")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let level_upper = level.map(str::to_ascii_uppercase);
    let entries: Vec<Value> = rows
        .iter()
        .filter_map(Value::as_object)
        .map(legacy_entry)
        .filter(|e| service.is_none_or(|s| e["logger"].as_str().unwrap_or("").contains(s)))
        .filter(|e| {
            level_upper
                .as_deref()
                .is_none_or(|l| e["level"].as_str() == Some(l))
        })
        .collect();
    let total = entries.len();
    let window: Vec<Value> = entries
        .into_iter()
        .skip(q.offset as usize)
        .take(q.limit as usize)
        .collect();
    Json(json!({
        "entries": window,
        "total": total,
        "limit": q.limit,
        "offset": q.offset,
    }))
    .into_response()
}

/// One store row (`{id, ts_us, source, level, target, msg, ..}`) as a log entry.
/// A row without a timestamp keeps `timestamp: null`: the time it was logged is
/// unknown, and the time it was read is not a substitute.
fn legacy_entry(row: &serde_json::Map<String, Value>) -> Value {
    let timestamp = row
        .get("ts_us")
        .and_then(Value::as_i64)
        .and_then(|us| OffsetDateTime::from_unix_timestamp_nanos(i128::from(us) * 1_000).ok())
        .and_then(|t| t.format(&Rfc3339).ok());
    let text = |k: &str| row.get(k).and_then(Value::as_str).filter(|s| !s.is_empty());
    json!({
        "seq": row.get("id").cloned().unwrap_or(Value::Null),
        "timestamp": timestamp,
        "level": text("level").unwrap_or("").to_ascii_uppercase(),
        "logger": text("target").or_else(|| text("source")).unwrap_or(""),
        "message": text("msg").unwrap_or(""),
    })
}

/// The `GET /api/logs/stream` filters.
#[derive(Debug, Deserialize)]
pub struct StreamQuery {
    #[serde(default)]
    pub level: Option<String>,
    #[serde(default)]
    pub service: Option<String>,
}

/// Rows the live tail replays first, so a fresh stream shows recent context.
const TAIL_REPLAY: u32 = 100;

/// How long the store may take to answer the tail's head.
const TAIL_HEAD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// `GET /api/logs/stream` — Server-Sent Events from the store's live tail,
/// each row re-shaped to the log-entry form. Keep-alive comment frames pass
/// through; an unreachable store ends the stream with a comment so the
/// EventSource reconnects.
pub async fn get_logs_stream(Query(q): Query<StreamQuery>) -> Response {
    let (mut tx, body) = http_body_util::channel::Channel::<bytes::Bytes>::new(16);
    let socket = crate::ipc::logd_client::default_logd_socket();
    tokio::spawn(async move { pump_tail(&socket, &q, &mut tx).await });
    axum::http::Response::builder()
        .header(axum::http::header::CONTENT_TYPE, "text/event-stream")
        .header(axum::http::header::CACHE_CONTROL, "no-cache")
        .body(axum::body::Body::new(body))
        .map_or_else(
            |_| axum::http::StatusCode::INTERNAL_SERVER_ERROR.into_response(),
            IntoResponse::into_response,
        )
}

async fn pump_tail(
    socket: &std::path::Path,
    q: &StreamQuery,
    tx: &mut http_body_util::channel::Sender<bytes::Bytes>,
) {
    use http_body_util::BodyExt;

    let mut params: Vec<(&str, String)> = vec![
        ("kind", "logs".to_string()),
        ("replay", TAIL_REPLAY.to_string()),
    ];
    if let Some(l) = q.level.as_deref().filter(|l| !l.is_empty()) {
        params.push(("level", l.to_ascii_lowercase()));
    }
    if let Some(s) = q.service.as_deref().filter(|s| !s.is_empty()) {
        params.push(("source", s.to_string()));
    }
    let path = format!(
        "/v1/tail?{}",
        crate::ipc::logd_client::encode_query(&params)
    );
    let Some(mut body) = open_tail(socket, &path).await else {
        let _ = tx
            .send_data(bytes::Bytes::from_static(
                b": logging store unavailable\n\n",
            ))
            .await;
        return;
    };
    let mut pending: Vec<u8> = Vec::new();
    while let Some(frame) = body.frame().await {
        let Ok(frame) = frame else { return };
        let Ok(data) = frame.into_data() else {
            continue;
        };
        pending.extend_from_slice(&data);
        while let Some(end) = pending.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = pending.drain(..=end).collect();
            let line = String::from_utf8_lossy(&line);
            if let Some(out) = sse_frame(line.trim_end_matches(['\r', '\n'])) {
                // The client went away: dropping the upstream body closes the tail.
                if tx.send_data(bytes::Bytes::from(out)).await.is_err() {
                    return;
                }
            }
        }
    }
}

/// Open the store's tail over its Unix socket: the streaming response body, or
/// `None` when the store is absent or refuses.
async fn open_tail(socket: &std::path::Path, path: &str) -> Option<hyper::body::Incoming> {
    let stream = tokio::net::UnixStream::connect(socket).await.ok()?;
    let (mut sender, conn) =
        hyper::client::conn::http1::handshake(hyper_util::rt::TokioIo::new(stream))
            .await
            .ok()?;
    tokio::spawn(async move {
        let _ = conn.await;
    });
    let request = axum::http::Request::get(path)
        .header(axum::http::header::HOST, "logd")
        .body(http_body_util::Empty::<bytes::Bytes>::new())
        .ok()?;
    let resp = tokio::time::timeout(TAIL_HEAD_TIMEOUT, sender.send_request(request))
        .await
        .ok()?
        .ok()?;
    resp.status().is_success().then(|| resp.into_body())
}

/// One upstream SSE line as the downstream frame, or `None` to drop it.
fn sse_frame(line: &str) -> Option<String> {
    if line.starts_with(':') {
        return Some(format!("{line}\n\n"));
    }
    let payload = line.strip_prefix("data:")?.trim();
    let row: Value = serde_json::from_str(payload).ok()?;
    let row = row.as_object()?;
    if row.get("kind").and_then(Value::as_str) == Some("lagged") {
        return Some(": tail lagged\n\n".to_string());
    }
    Some(format!("data: {}\n\n", legacy_entry(row)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: i64, level: &str, target: &str, msg: &str) -> Value {
        json!({"id": id, "ts_us": 1_700_000_000_000_000i64 + id, "level": level, "target": target, "source": "src", "msg": msg})
    }

    #[test]
    fn a_store_row_maps_to_the_entry_shape() {
        let e = legacy_entry(row(7, "warn", "ados_video", "hi").as_object().unwrap());
        assert_eq!(e["seq"], 7);
        assert_eq!(e["level"], "WARN");
        assert_eq!(e["logger"], "ados_video");
        assert_eq!(e["message"], "hi");
        assert!(e["timestamp"]
            .as_str()
            .unwrap()
            .starts_with("2023-11-14T22:13:20"));
    }

    #[test]
    fn a_row_without_a_timestamp_is_not_stamped_now() {
        let e = legacy_entry(json!({"id": 1, "msg": "x"}).as_object().unwrap());
        assert!(e["timestamp"].is_null());
    }

    #[tokio::test]
    async fn an_unreachable_store_is_an_empty_page_with_a_warning() {
        let dir = tempfile::tempdir().unwrap();
        let client = LogdQueryClient::new(dir.path().join("absent.sock"));
        let q = LogsQuery {
            level: None,
            service: None,
            limit: 50,
            offset: 0,
        };
        let resp = logs_from(&client, q).await;
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["entries"], json!([]));
        assert_eq!(v["warning"], "logging store unavailable");
    }

    #[test]
    fn tail_lines_become_entry_frames_and_notices_become_comments() {
        assert_eq!(
            sse_frame(": keep-alive").as_deref(),
            Some(": keep-alive\n\n")
        );
        assert_eq!(
            sse_frame(r#"data: {"kind":"lagged","skipped":4}"#).as_deref(),
            Some(": tail lagged\n\n")
        );
        let frame = sse_frame(&format!("data: {}", row(3, "info", "ados_radio", "up"))).unwrap();
        let v: Value = serde_json::from_str(frame.trim().strip_prefix("data: ").unwrap()).unwrap();
        assert_eq!(v["logger"], "ados_radio");
        assert_eq!(v["level"], "INFO");
        assert!(sse_frame("event: x").is_none());
        assert!(sse_frame("data: not json").is_none());
    }
}
