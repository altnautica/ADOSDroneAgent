use super::*;
use http_body_util::BodyExt;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixListener;

fn row(id: i64, level: &str, target: &str, msg: &str) -> Value {
    json!({"id": id, "ts_us": 1_700_000_000_000_000i64 + id, "level": level, "target": target, "source": "src", "msg": msg})
}

/// Two store rows: one whose logger is its `target` (a module path that no
/// service name equals) and one whose logger falls back to its `source`.
fn pilot_and_video_rows() -> (Value, Value) {
    let pilot = json!({"id": 2, "ts_us": 1_700_000_000_000_002i64, "level": "info",
        "target": "ados_pilot::core", "source": "ados-pilot", "msg": "handoff"});
    let video = json!({"id": 1, "ts_us": 1_700_000_000_000_001i64, "level": "info",
        "target": "", "source": "ados-video", "msg": "encoder up"});
    (pilot, video)
}

/// A one-shot store on `path`: records the request line it was asked and
/// answers with `response`, then closes the connection.
fn fake_store(path: &std::path::Path, response: String) -> tokio::task::JoinHandle<String> {
    let listener = UnixListener::bind(path).unwrap();
    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut head = Vec::new();
        let mut byte = [0u8; 1];
        while !head.ends_with(b"\r\n\r\n") {
            if stream.read(&mut byte).await.unwrap() == 0 {
                break;
            }
            head.push(byte[0]);
        }
        stream.write_all(response.as_bytes()).await.unwrap();
        stream.shutdown().await.unwrap();
        let head = String::from_utf8_lossy(&head).into_owned();
        head.lines().next().unwrap_or("").to_string()
    })
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

#[tokio::test]
async fn the_logger_filter_is_a_contains_match_on_the_shown_logger() {
    for (service, want_logger) in [("pilot", "ados_pilot::core"), ("video", "ados-video")] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("logd.sock");
        let (pilot, video) = pilot_and_video_rows();
        let body = json!({ "data": [pilot, video] }).to_string();
        let store = fake_store(
            &path,
            format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            ),
        );
        let q = LogsQuery {
            level: None,
            service: Some(service.to_string()),
            limit: 50,
            offset: 0,
        };
        let resp = logs_from(&LogdQueryClient::new(path), q).await;
        let request = store.await.unwrap();
        // The store's `source` filter is an exact match on a column the logger
        // column often does not show, so the filter must not be pushed down.
        assert!(!request.contains("source="), "{service}: {request}");
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&bytes).unwrap();
        let loggers: Vec<&str> = v["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["logger"].as_str().unwrap())
            .collect();
        assert_eq!(loggers, [want_logger], "{service}: {v}");
        assert_eq!(v["total"], 1, "{service}");
    }
}

#[tokio::test]
async fn the_live_tail_applies_the_same_logger_filter() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("logd.sock");
    let (pilot, video) = pilot_and_video_rows();
    let store = fake_store(
        &path,
        format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n: keep-alive\n\ndata: {video}\n\ndata: {pilot}\n\n"
        ),
    );
    let q = StreamQuery {
        level: None,
        service: Some("pilot".to_string()),
    };
    let (mut tx, body) = http_body_util::channel::Channel::<bytes::Bytes>::new(16);
    pump_tail(&path, &q, &mut tx).await;
    drop(tx);
    let request = store.await.unwrap();
    assert!(!request.contains("source="), "{request}");
    let out = body.collect().await.unwrap().to_bytes();
    let out = String::from_utf8_lossy(&out);
    let loggers: Vec<String> = out
        .split("\n\n")
        .filter_map(|f| f.strip_prefix("data: "))
        .map(|d| serde_json::from_str::<Value>(d).unwrap()["logger"].to_string())
        .collect();
    assert_eq!(loggers, ["\"ados_pilot::core\""], "{out}");
    // Comment frames still pass, so the EventSource keep-alive survives a filter.
    assert!(out.starts_with(": keep-alive\n\n"), "{out}");
}

#[test]
fn tail_lines_become_entry_frames_and_notices_become_comments() {
    assert_eq!(
        sse_frame(": keep-alive", None).as_deref(),
        Some(": keep-alive\n\n")
    );
    assert_eq!(
        sse_frame(r#"data: {"kind":"lagged","skipped":4}"#, None).as_deref(),
        Some(": tail lagged\n\n")
    );
    let line = format!("data: {}", row(3, "info", "ados_radio", "up"));
    let frame = sse_frame(&line, None).unwrap();
    let v: Value = serde_json::from_str(frame.trim().strip_prefix("data: ").unwrap()).unwrap();
    assert_eq!(v["logger"], "ados_radio");
    assert_eq!(v["level"], "INFO");
    assert!(sse_frame(&line, Some("radio")).is_some());
    assert!(sse_frame(&line, Some("video")).is_none());
    assert!(sse_frame("event: x", None).is_none());
    assert!(sse_frame("data: not json", None).is_none());
}
