use super::*;
use crate::db;
use ados_protocol::logd::{IngestFrame, Level, LogFrame, TelemetryFrame};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

/// A store seeded with a session and a handful of rows the read path queries.
fn seed(path: &Path) {
    let conn = db::open(path).unwrap();
    conn.execute(
        "INSERT INTO sessions (started_us, kind) VALUES (1000, 'boot')",
        [],
    )
    .unwrap();
    let s = conn.last_insert_rowid();
    for i in 0..6i64 {
        conn.execute(
            "INSERT INTO logs (ts_us, session, source, level, target, msg) \
                 VALUES (?1, ?2, 'api', ?3, 'mod', ?4)",
            rusqlite::params![2000 + i, s, (i % 5), format!("line {i}")],
        )
        .unwrap();
    }
    conn.execute(
        "INSERT INTO metrics (ts_us, session, metric, value) VALUES (2100, ?1, 'cpu.load', 0.5)",
        [s],
    )
    .unwrap();
}

/// Bring the server up against a temp store and temp sockets/port, returning
/// the socket path, the chosen TCP port, the broadcast sender, and a stop
/// trigger. The caller drives requests, then fires stop.
struct Harness {
    socket: PathBuf,
    port: u16,
    broadcast: broadcast::Sender<IngestFrame>,
    stop: Option<oneshot::Sender<()>>,
    join: tokio::task::JoinHandle<Result<()>>,
    // Held alive so the writer side of the control channel never closes
    // while the read tests run; the real writer (mark tests) replaces this
    // by keeping its own thread alive.
    _writer: Option<std::thread::JoinHandle<()>>,
}

/// The read-only harness: seed the store directly and serve it with a control
/// sender whose receiver is held alive but never drained (the read tests do
/// not POST `/v1/synced`).
async fn start(dir: &Path, pairing_body: Option<&str>) -> Harness {
    let db_path = dir.join("logs.db");
    seed(&db_path);
    // A control channel whose receiver is parked alive for the test, so the
    // sender handed to the server never sees a closed channel.
    let (mark_tx, mark_rx) = mpsc::channel::<ControlMsg>(8);
    let parked = std::thread::spawn(move || {
        // Hold the receiver until the channel closes (all senders dropped).
        let mut rx = mark_rx;
        while rx.blocking_recv().is_some() {}
    });
    start_with_control(
        dir,
        pairing_body,
        db_path,
        mark_tx,
        crate::writer::WriterHealth::new(),
        Some(parked),
    )
    .await
}

/// The writer-backed harness: spawn a real writer thread over the same store
/// so the mark-synced control path is serviced end-to-end, and hand the
/// server the writer's control handle. Returns the harness plus an ingest
/// sender the caller uses to seed rows through the writer.
async fn start_with_writer(
    dir: &Path,
    pairing_body: Option<&str>,
) -> (Harness, mpsc::Sender<IngestFrame>) {
    let db_path = dir.join("logs.db");
    let (ingest_tx, ingest_rx) = mpsc::channel::<IngestFrame>(256);
    let writer = crate::writer::Writer::new(
        &db_path,
        ingest_rx,
        crate::writer::WriterConfig::hermetic(),
        std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
    )
    .unwrap();
    let mark_tx = writer.control_handle();
    let health = writer.health_handle();
    let writer_thread = std::thread::spawn(move || writer.run().unwrap());
    let h = start_with_control(
        dir,
        pairing_body,
        db_path,
        mark_tx,
        health,
        Some(writer_thread),
    )
    .await;
    (h, ingest_tx)
}

async fn start_with_control(
    dir: &Path,
    pairing_body: Option<&str>,
    db_path: PathBuf,
    mark_tx: mpsc::Sender<ControlMsg>,
    writer_health: crate::writer::WriterHealth,
    writer: Option<std::thread::JoinHandle<()>>,
) -> Harness {
    let socket = dir.join("logd-query.sock");
    let pairing_path = dir.join("pairing.json");
    if let Some(body) = pairing_body {
        std::fs::write(&pairing_path, body).unwrap();
    }
    // Bind an ephemeral TCP port by asking the OS, then release it for the
    // server to rebind (a tiny race window that is fine for a test).
    let probe = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let port = probe.local_addr().unwrap().port();
    drop(probe);

    let (broadcast, _keep) = broadcast::channel::<IngestFrame>(64);
    let ingest = Arc::new(IngestStats::default());
    let (stop_tx, stop_rx) = oneshot::channel::<()>();
    let bcast = broadcast.clone();
    let socket_clone = socket.clone();
    let join = tokio::spawn(async move {
        spawn_query_server(
            db_path,
            socket_clone,
            port,
            bcast,
            ingest,
            pairing_path,
            mark_tx,
            writer_health,
            async move {
                let _ = stop_rx.await;
            },
        )
        .await
    });
    // Wait for the socket to appear.
    for _ in 0..200 {
        if socket.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    Harness {
        socket,
        port,
        broadcast,
        stop: Some(stop_tx),
        join,
        _writer: writer,
    }
}

impl Harness {
    async fn stop(mut self) {
        if let Some(tx) = self.stop.take() {
            let _ = tx.send(());
        }
        let _ = self.join.await;
    }
}

/// Connect to the unix socket, retrying briefly: under parallel test load
/// the listener can have bound (so the path exists) a hair before its accept
/// loop is polling, which surfaces as a transient `ConnectionRefused`. The
/// retry is short-bounded so a genuinely unreachable socket still fails.
async fn connect_unix(socket: &Path) -> UnixStream {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    loop {
        match UnixStream::connect(socket).await {
            Ok(s) => return s,
            Err(_) if tokio::time::Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            Err(e) => panic!("connect {}: {e}", socket.display()),
        }
    }
}

/// Minimal HTTP/1.1 GET over the unix socket: write the request, read the
/// whole response, return (status_line, body).
async fn unix_get(
    socket: &Path,
    path_and_query: &str,
    header: Option<(&str, &str)>,
) -> (String, String) {
    let mut stream = connect_unix(socket).await;
    let mut req =
        format!("GET {path_and_query} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n");
    if let Some((k, v)) = header {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    req.push_str("\r\n");
    stream.write_all(req.as_bytes()).await.unwrap();
    stream.flush().await.unwrap();
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await.unwrap();
    split_http(&buf)
}

/// Same minimal GET over a TCP connection to 127.0.0.1:port.
async fn tcp_get(
    port: u16,
    path_and_query: &str,
    header: Option<(&str, &str)>,
) -> (String, String) {
    let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .unwrap();
    let mut req =
        format!("GET {path_and_query} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n");
    if let Some((k, v)) = header {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    req.push_str("\r\n");
    stream.write_all(req.as_bytes()).await.unwrap();
    stream.flush().await.unwrap();
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await.unwrap();
    split_http(&buf)
}

/// Minimal HTTP/1.1 POST of a JSON body over the unix socket.
async fn unix_post(socket: &Path, path: &str, body: &str) -> (String, String) {
    let mut stream = connect_unix(socket).await;
    let req = format!(
        "POST {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\
             Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(req.as_bytes()).await.unwrap();
    stream.flush().await.unwrap();
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await.unwrap();
    split_http(&buf)
}

/// Same minimal POST over a TCP connection to 127.0.0.1:port.
async fn tcp_post(
    port: u16,
    path: &str,
    body: &str,
    header: Option<(&str, &str)>,
) -> (String, String) {
    let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .unwrap();
    let mut req = format!(
        "POST {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\
             Content-Type: application/json\r\nContent-Length: {}\r\n",
        body.len()
    );
    if let Some((k, v)) = header {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    req.push_str(&format!("\r\n{body}"));
    stream.write_all(req.as_bytes()).await.unwrap();
    stream.flush().await.unwrap();
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await.unwrap();
    split_http(&buf)
}

/// Split a raw HTTP response into the status line and the body (after the
/// blank line). De-chunks a `Transfer-Encoding: chunked` body crudely enough
/// for the test envelopes.
fn split_http(buf: &[u8]) -> (String, String) {
    let text = String::from_utf8_lossy(buf).into_owned();
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    let status = head.lines().next().unwrap_or("").to_string();
    let body = if head.to_lowercase().contains("transfer-encoding: chunked") {
        de_chunk(body)
    } else {
        body.to_string()
    };
    (status, body)
}

/// Crude de-chunking: parse `<hexlen>\r\n<data>\r\n` repeatedly until a
/// zero-length chunk. Good enough for the small JSON bodies under test.
fn de_chunk(body: &str) -> String {
    let mut out = String::new();
    let mut rest = body;
    while let Some((len_line, after)) = rest.split_once("\r\n") {
        let len = usize::from_str_radix(len_line.trim(), 16).unwrap_or(0);
        if len == 0 {
            break;
        }
        if after.len() < len {
            out.push_str(after);
            break;
        }
        out.push_str(&after[..len]);
        rest = after[len..].strip_prefix("\r\n").unwrap_or(&after[len..]);
    }
    out
}

#[tokio::test]
async fn query_over_the_unix_socket_returns_rows() {
    let dir = tempfile::tempdir().unwrap();
    let h = start(dir.path(), None).await;
    let (status, body) = unix_get(&h.socket, "/v1/query?kind=logs&limit=3", None).await;
    assert!(status.contains("200"), "status was {status}");
    let json: serde_json::Value =
        serde_json::from_str(&body).unwrap_or_else(|_| panic!("body: {body}"));
    assert_eq!(json["meta"]["source"], "logd");
    let data = json["data"].as_array().unwrap();
    assert_eq!(data.len(), 3);
    // Keyset paging: a next_cursor is present because there are more rows.
    assert!(json["page"]["next_cursor"].is_string());
    h.stop().await;
}

#[tokio::test]
async fn keyset_pagination_over_the_socket_is_disjoint() {
    let dir = tempfile::tempdir().unwrap();
    let h = start(dir.path(), None).await;
    let (_s1, b1) = unix_get(&h.socket, "/v1/query?kind=logs&limit=3", None).await;
    let p1: serde_json::Value = serde_json::from_str(&b1).unwrap();
    let cursor = p1["page"]["next_cursor"].as_str().unwrap().to_string();
    let (_s2, b2) = unix_get(
        &h.socket,
        &format!("/v1/query?kind=logs&limit=3&cursor={cursor}"),
        None,
    )
    .await;
    let p2: serde_json::Value = serde_json::from_str(&b2).unwrap();
    let ids1: Vec<i64> = p1["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_i64().unwrap())
        .collect();
    let ids2: Vec<i64> = p2["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_i64().unwrap())
        .collect();
    assert!(
        ids1.iter().all(|id| !ids2.contains(id)),
        "pages overlap: {ids1:?} {ids2:?}"
    );
    h.stop().await;
}

#[tokio::test]
async fn a_bad_cursor_is_rejected_with_four_hundred() {
    let dir = tempfile::tempdir().unwrap();
    let h = start(dir.path(), None).await;
    let (status, body) = unix_get(
        &h.socket,
        "/v1/query?kind=logs&cursor=not-a-real-cursor",
        None,
    )
    .await;
    assert!(status.contains("400"), "status was {status}");
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(json["error"]["code"], "bad_cursor");
    h.stop().await;
}

#[tokio::test]
async fn healthz_and_openapi_are_public_on_both_edges_even_when_paired() {
    let dir = tempfile::tempdir().unwrap();
    let h = start(dir.path(), Some(r#"{"paired": true, "api_key": "ados_k"}"#)).await;
    // Public on the TCP edge with no key.
    let (status, body) = tcp_get(h.port, "/v1/healthz", None).await;
    assert!(status.contains("200"), "status {status}");
    assert!(body.contains("\"ok\""));
    let (ostatus, obody) = tcp_get(h.port, "/v1/openapi.json", None).await;
    assert!(ostatus.contains("200"));
    assert!(obody.contains("/v1/query"));
    h.stop().await;
}

#[tokio::test]
async fn unix_socket_carries_no_auth_even_when_paired() {
    let dir = tempfile::tempdir().unwrap();
    let h = start(dir.path(), Some(r#"{"paired": true, "api_key": "ados_k"}"#)).await;
    // No key on the trusted socket: still answered.
    let (status, _body) = unix_get(&h.socket, "/v1/query?limit=1", None).await;
    assert!(status.contains("200"), "status {status}");
    h.stop().await;
}

#[tokio::test]
async fn tcp_requires_the_key_when_paired() {
    let dir = tempfile::tempdir().unwrap();
    let h = start(
        dir.path(),
        Some(r#"{"paired": true, "api_key": "ados_secret"}"#),
    )
    .await;
    // No key → 401.
    let (no_key, _b) = tcp_get(h.port, "/v1/query?limit=1", None).await;
    assert!(no_key.contains("401"), "status {no_key}");
    // Wrong key → 401.
    let (wrong, _b) = tcp_get(h.port, "/v1/query?limit=1", Some(("X-ADOS-Key", "nope"))).await;
    assert!(wrong.contains("401"), "status {wrong}");
    // Right key → 200.
    let (ok, body) = tcp_get(
        h.port,
        "/v1/query?limit=1",
        Some(("X-ADOS-Key", "ados_secret")),
    )
    .await;
    assert!(ok.contains("200"), "status {ok}");
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(json["meta"]["source"], "logd");
    h.stop().await;
}

#[tokio::test]
async fn tcp_serves_the_local_operator_when_unpaired() {
    let dir = tempfile::tempdir().unwrap();
    // No pairing file → unpaired → a loopback caller is the local operator.
    let h = start(dir.path(), None).await;
    let (status, _b) = tcp_get(h.port, "/v1/query?limit=1", None).await;
    assert!(status.contains("200"), "status {status}");
    h.stop().await;
}

/// A tunnel or reverse proxy terminating on this host delivers internet
/// callers from 127.0.0.1 with a forwarding header. On an unpaired node
/// that caller must not read the store.
#[tokio::test]
async fn tcp_refuses_a_relayed_caller_when_unpaired() {
    let dir = tempfile::tempdir().unwrap();
    let h = start(dir.path(), None).await;
    let (status, _b) = tcp_get(
        h.port,
        "/v1/query?limit=1",
        Some(("X-Forwarded-For", "203.0.113.7")),
    )
    .await;
    assert!(status.contains("401"), "status {status}");
    h.stop().await;
}

/// A pairing file that exists but cannot be parsed fails closed on the LAN
/// edge instead of reading as unpaired.
#[tokio::test]
async fn tcp_refuses_every_caller_when_the_pairing_file_is_unreadable() {
    let dir = tempfile::tempdir().unwrap();
    let h = start(dir.path(), Some("{ truncated")).await;
    let (status, _b) = tcp_get(h.port, "/v1/query?limit=1", None).await;
    assert!(status.contains("401"), "status {status}");
    h.stop().await;
}

#[tokio::test]
async fn public_paths_answer_even_after_the_rate_budget_is_drained() {
    let dir = tempfile::tempdir().unwrap();
    let h = start(dir.path(), None).await;
    // Drain the per-second read budget with non-public requests. The default
    // budget is 30/s; firing more than that in one window exhausts it (the
    // test runs well under a second, so no window refill intervenes).
    let mut saw_rate_limit = false;
    for _ in 0..40 {
        let (status, _b) = tcp_get(h.port, "/v1/query?limit=1", None).await;
        if status.contains("429") {
            saw_rate_limit = true;
            break;
        }
    }
    assert!(
        saw_rate_limit,
        "the rate limiter should reject a non-public flood with 429"
    );
    // With the budget drained, a non-public path is still rate-limited...
    let (q, _b) = tcp_get(h.port, "/v1/query?limit=1", None).await;
    assert!(q.contains("429"), "query is still rate-limited: {q}");
    // ...but the public liveness and discovery paths must still answer, so a
    // watchdog or reachability probe is never starved by a query flood.
    let (hz, hb) = tcp_get(h.port, "/v1/healthz", None).await;
    assert!(
        hz.contains("200"),
        "healthz starved by the rate limiter: {hz}"
    );
    assert!(hb.contains("\"ok\""));
    let (op, _ob) = tcp_get(h.port, "/v1/openapi.json", None).await;
    assert!(
        op.contains("200"),
        "openapi starved by the rate limiter: {op}"
    );
    h.stop().await;
}

#[tokio::test]
async fn aggregate_and_sessions_and_stats_answer() {
    let dir = tempfile::tempdir().unwrap();
    let h = start(dir.path(), None).await;

    let (a_s, a_b) = unix_get(
        &h.socket,
        "/v1/aggregate?metric=cpu.load&bucket=1s&agg=avg",
        None,
    )
    .await;
    assert!(a_s.contains("200"), "{a_s}");
    let aj: serde_json::Value = serde_json::from_str(&a_b).unwrap();
    assert!(aj["data"]
        .as_array()
        .unwrap()
        .iter()
        .any(|b| b["metric"] == "cpu.load"));

    let (s_s, s_b) = unix_get(&h.socket, "/v1/sessions", None).await;
    assert!(s_s.contains("200"));
    let sj: serde_json::Value = serde_json::from_str(&s_b).unwrap();
    assert_eq!(sj["data"].as_array().unwrap()[0]["log_count"], 6);

    let (st_s, st_b) = unix_get(&h.socket, "/v1/stats", None).await;
    assert!(st_s.contains("200"));
    let stj: serde_json::Value = serde_json::from_str(&st_b).unwrap();
    assert_eq!(stj["data"]["rows"]["logs"], 6);
    assert_eq!(stj["data"]["integrity"], "ok");

    h.stop().await;
}

#[tokio::test]
async fn export_streams_jsonl_rows() {
    let dir = tempfile::tempdir().unwrap();
    let h = start(dir.path(), None).await;
    let (status, body) = unix_get(&h.socket, "/v1/export?kind=logs&format=jsonl", None).await;
    assert!(status.contains("200"), "status {status}");
    let lines: Vec<&str> = body.lines().filter(|l| !l.is_empty()).collect();
    assert_eq!(lines.len(), 6);
    let first: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
    assert!(first["msg"].is_string());
    h.stop().await;
}

#[tokio::test]
async fn tail_replays_backlog_then_streams_a_live_frame() {
    let dir = tempfile::tempdir().unwrap();
    let h = start(dir.path(), None).await;
    // Open an SSE tail with a small replay, read the replay events, then
    // publish a live frame on the broadcast and read it off the stream.
    let mut stream = connect_unix(&h.socket).await;
    let req = "GET /v1/tail?kind=logs&replay=2 HTTP/1.1\r\nHost: localhost\r\n\r\n";
    stream.write_all(req.as_bytes()).await.unwrap();
    stream.flush().await.unwrap();

    // Read until we have seen the two replay events.
    let mut acc = String::new();
    let mut buf = [0u8; 4096];
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    // Give the handler a moment to subscribe + emit the replay.
    loop {
        if acc.matches("data:").count() >= 2 {
            break;
        }
        let n = tokio::time::timeout_at(deadline, stream.read(&mut buf))
            .await
            .unwrap()
            .unwrap();
        if n == 0 {
            break;
        }
        acc.push_str(&String::from_utf8_lossy(&buf[..n]));
    }
    assert!(
        acc.matches("data:").count() >= 2,
        "expected two replay events, got: {acc}"
    );

    // Publish a live frame and confirm it shows up on the stream.
    let live = IngestFrame::Log(LogFrame::new(
        crate::writer::now_us(),
        "api",
        Level::Error,
        "live event",
    ));
    let _ = h.broadcast.send(live);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if acc.contains("live event") {
            break;
        }
        let n = tokio::time::timeout_at(deadline, stream.read(&mut buf))
            .await
            .unwrap()
            .unwrap();
        if n == 0 {
            break;
        }
        acc.push_str(&String::from_utf8_lossy(&buf[..n]));
    }
    assert!(
        acc.contains("live event"),
        "live frame did not reach the tail: {acc}"
    );
    drop(stream);

    // Make sure a non-matching live frame would have been filtered: publish a
    // metric on a logs tail and confirm the stream does not error on it (it
    // is simply not emitted). We just verify the server is still serving.
    let _ = h.broadcast.send(IngestFrame::Telemetry(TelemetryFrame::new(
        1, "cpu.load", 1.0,
    )));
    let (status, _b) = unix_get(&h.socket, "/v1/stats", None).await;
    assert!(status.contains("200"));
    h.stop().await;
}

#[tokio::test]
async fn unknown_path_is_a_clean_404_envelope() {
    let dir = tempfile::tempdir().unwrap();
    let h = start(dir.path(), None).await;
    let (status, body) = unix_get(&h.socket, "/v1/nope", None).await;
    assert!(status.contains("404"), "status {status}");
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(json["error"]["code"], "not_found");
    h.stop().await;
}

#[tokio::test]
async fn synced_over_unix_socket_marks_and_returns_200() {
    let dir = tempfile::tempdir().unwrap();
    // A real writer thread services the mark; seed rows through ingest.
    let (h, ingest) = start_with_writer(dir.path(), None).await;
    for i in 0..5i64 {
        ingest
            .send(IngestFrame::Telemetry(TelemetryFrame::new(
                crate::writer::now_us() + i,
                "cpu.load",
                i as f64,
            )))
            .await
            .unwrap();
    }
    // Wait for the writer to commit them and confirm they are unsynced.
    let mut before = 0i64;
    for _ in 0..200 {
        let (_s, b) = unix_get(&h.socket, "/v1/stats", None).await;
        let j: serde_json::Value = serde_json::from_str(&b).unwrap();
        before = j["data"]["unsynced"]["metrics"].as_i64().unwrap_or(0);
        if before >= 5 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(before >= 5, "metrics should be unsynced before the mark");

    // Mark the metrics table over the trusted socket.
    let (status, body) = unix_post(&h.socket, "/v1/synced", r#"{"tables":["metrics"]}"#).await;
    assert!(status.contains("200"), "status {status}: {body}");
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert!(json["data"]["marked"]["metrics"].as_i64().unwrap() >= 5);
    assert_eq!(json["data"]["unsynced_after"]["metrics"], 0);
    assert_eq!(json["meta"]["source"], "logd");

    // The stats watermark for metrics has dropped to zero.
    let (_s, sb) = unix_get(&h.socket, "/v1/stats", None).await;
    let sj: serde_json::Value = serde_json::from_str(&sb).unwrap();
    assert_eq!(sj["data"]["unsynced"]["metrics"], 0);
    drop(ingest);
    h.stop().await;
}

#[tokio::test]
async fn synced_is_forbidden_on_tcp_paired_and_unpaired() {
    // Unpaired: still forbidden on TCP.
    {
        let dir = tempfile::tempdir().unwrap();
        let h = start(dir.path(), None).await;
        let (status, body) = tcp_post(h.port, "/v1/synced", r#"{"tables":["logs"]}"#, None).await;
        assert!(status.contains("403"), "unpaired tcp status {status}");
        let json: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(json["error"]["code"], "local_only");
        h.stop().await;
    }
    // Paired, with the right key: still forbidden — being on the LAN never
    // unlocks the write, key or no key.
    {
        let dir = tempfile::tempdir().unwrap();
        let h = start(
            dir.path(),
            Some(r#"{"paired": true, "api_key": "ados_secret"}"#),
        )
        .await;
        let (status, body) = tcp_post(
            h.port,
            "/v1/synced",
            r#"{"tables":["logs"]}"#,
            Some(("X-ADOS-Key", "ados_secret")),
        )
        .await;
        assert!(status.contains("403"), "paired tcp status {status}");
        let json: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(json["error"]["code"], "local_only");
        h.stop().await;
    }
}

#[tokio::test]
async fn synced_bad_range_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let (h, ingest) = start_with_writer(dir.path(), None).await;
    let (status, body) = unix_post(&h.socket, "/v1/synced", r#"{"from_us":200,"to_us":100}"#).await;
    assert!(status.contains("400"), "status {status}: {body}");
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(json["error"]["code"], "bad_range");
    drop(ingest);
    h.stop().await;
}
