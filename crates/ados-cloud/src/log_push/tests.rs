use super::*;

// ── request / result (de)serialization ──────────────────────────────

#[test]
fn push_request_round_trips_and_omits_absent_fields() {
    let req = PushRequest {
        session: Some(7),
        since_us: Some(1_700_000_000_000_000),
        kinds: vec!["logs".to_string(), "events".to_string()],
    };
    let json = serde_json::to_string(&req).unwrap();
    let back: PushRequest = serde_json::from_str(&json).unwrap();
    assert_eq!(req, back);

    // A bare object parses to all-defaults.
    let bare: PushRequest = serde_json::from_str("{}").unwrap();
    assert_eq!(bare, PushRequest::default());

    // Null fields parse as None / empty.
    let nulls: PushRequest =
        serde_json::from_str(r#"{"session":null,"since_us":null,"kinds":[]}"#).unwrap();
    assert_eq!(nulls, PushRequest::default());
}

#[test]
fn resolved_kinds_defaults_to_all_and_filters_unknowns() {
    // Empty → all four in stable order.
    assert_eq!(
        PushRequest::default().resolved_kinds(),
        vec!["logs", "metrics", "events", "hw"]
    );
    // A subset, deduped and ordered, with an unknown kind dropped.
    let req = PushRequest {
        kinds: vec![
            "events".to_string(),
            "logs".to_string(),
            "events".to_string(),
            "bogus".to_string(),
        ],
        ..Default::default()
    };
    assert_eq!(req.resolved_kinds(), vec!["logs", "events"]);
}

#[test]
fn push_result_serializes_without_error_when_clean() {
    let ok = PushResult {
        pushed: 2,
        deduped: 1,
        bytes: 4096,
        error: None,
    };
    let json = serde_json::to_string(&ok).unwrap();
    // The `error` key is omitted when absent.
    assert!(!json.contains("error"));
    let back: PushResult = serde_json::from_str(&json).unwrap();
    assert_eq!(ok, back);

    let bad = PushResult::errored("not_cloud_paired");
    let json = serde_json::to_string(&bad).unwrap();
    assert!(json.contains("not_cloud_paired"));
    let back: PushResult = serde_json::from_str(&json).unwrap();
    assert_eq!(bad, back);
}

// ── sha256 / idempotency decision ───────────────────────────────────

#[test]
fn window_hash_is_stable_lowercase_hex_sha256() {
    // The empty SHA-256 digest, lowercase hex — the canonical anchor.
    assert_eq!(
        window_hash(b""),
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    );
    // Deterministic and content-sensitive.
    let a = window_hash(b"the same window");
    let b = window_hash(b"the same window");
    let c = window_hash(b"a different window");
    assert_eq!(a, b);
    assert_ne!(a, c);
    assert_eq!(a.len(), 64);
    assert!(a
        .chars()
        .all(|ch| ch.is_ascii_hexdigit() && !ch.is_ascii_uppercase()));
}

#[test]
fn ingest_decision_inserted_stores_and_marks() {
    let d = IngestDecision::from_status("inserted");
    assert!(!d.deduped);
    assert!(d.mark_synced);
}

#[test]
fn ingest_decision_duplicate_marks_without_storing() {
    // A duplicate means the cloud already holds the window, so the local
    // rows are still safe to flip to synced — but no new store happened.
    let d = IngestDecision::from_status("duplicate");
    assert!(d.deduped);
    assert!(d.mark_synced);
}

#[test]
fn ingest_decision_unknown_status_does_not_mark() {
    let d = IngestDecision::from_status("rejected");
    assert!(!d.deduped);
    assert!(!d.mark_synced);
}

// ── the gate ────────────────────────────────────────────────────────

fn passing_gate<'a>() -> PushGate<'a> {
    PushGate {
        mode: "cloud",
        cloud_logs_enabled: true,
        api_key: Some("k-123"),
        convex_url: "https://relay.example/convex",
    }
}

#[test]
fn gate_passes_only_when_paired_enabled_and_keyed() {
    let (key, url) = passing_gate().check().unwrap();
    assert_eq!(key, "k-123");
    assert_eq!(url, "https://relay.example/convex");
}

#[test]
fn gate_rejects_local_mode() {
    let g = PushGate {
        mode: "local",
        ..passing_gate()
    };
    assert_eq!(g.check().unwrap_err(), GateError::NotCloudPaired);
}

#[test]
fn gate_rejects_empty_convex_url() {
    let g = PushGate {
        convex_url: "",
        ..passing_gate()
    };
    assert_eq!(g.check().unwrap_err(), GateError::NotCloudPaired);
}

#[test]
fn gate_rejects_when_opt_in_off() {
    let g = PushGate {
        cloud_logs_enabled: false,
        ..passing_gate()
    };
    assert_eq!(g.check().unwrap_err(), GateError::CloudLogsDisabled);
}

#[test]
fn gate_rejects_when_unpaired_no_key() {
    let g = PushGate {
        api_key: None,
        ..passing_gate()
    };
    assert_eq!(g.check().unwrap_err(), GateError::NotCloudPaired);

    let g = PushGate {
        api_key: Some(""),
        ..passing_gate()
    };
    assert_eq!(g.check().unwrap_err(), GateError::NotCloudPaired);
}

#[test]
fn gate_error_codes_are_stable_behaviour_strings() {
    assert_eq!(GateError::NotCloudPaired.code(), "not_cloud_paired");
    assert_eq!(GateError::CloudLogsDisabled.code(), "cloud_logs_disabled");
    assert_eq!(GateError::NoBackend.code(), "no_backend");
}

// ── run_push gate short-circuit (no transport built when refused) ────

/// An http client built on the crate's preconfigured TLS, the same one the
/// daemon uses. A bare `reqwest::Client::new()` panics under this crate's
/// no-default-provider TLS feature set, so the preconfigured config is used.
/// The gate short-circuits before any request is sent, so no network is hit.
fn test_client() -> reqwest::Client {
    reqwest::Client::builder()
        .use_preconfigured_tls(crate::tls::client_config())
        .build()
        .expect("reqwest builds with the rustls config")
}

#[tokio::test]
async fn run_push_refuses_unpaired_before_any_transport() {
    let mut config = CloudConfig::default();
    config.server.mode = "cloud".to_string();
    config.server.cloud_logs_enabled = true;
    // Unpaired pairing state → no api key.
    let pairing = PairingState::default();
    let http = test_client();
    let req = PushRequest::default();
    // An unreachable url proves the gate short-circuits before any POST.
    let res = run_push(&config, &pairing, &http, "https://127.0.0.1:1/convex", &req).await;
    assert_eq!(res.error.as_deref(), Some("not_cloud_paired"));
    assert_eq!(res.pushed, 0);
    assert_eq!(res.deduped, 0);
}

#[tokio::test]
async fn run_push_refuses_when_opt_in_off() {
    let mut config = CloudConfig::default();
    config.server.mode = "cloud".to_string();
    config.server.cloud_logs_enabled = false; // opt-in off
    let pairing = paired_state();
    let http = test_client();
    let res = run_push(
        &config,
        &pairing,
        &http,
        "https://127.0.0.1:1/convex",
        &PushRequest::default(),
    )
    .await;
    assert_eq!(res.error.as_deref(), Some("cloud_logs_disabled"));
}

#[tokio::test]
async fn run_push_refuses_in_local_mode() {
    let mut config = CloudConfig::default();
    config.server.mode = "local".to_string();
    config.server.cloud_logs_enabled = true;
    let pairing = paired_state();
    let http = test_client();
    let res = run_push(
        &config,
        &pairing,
        &http,
        "https://127.0.0.1:1/convex",
        &PushRequest::default(),
    )
    .await;
    assert_eq!(res.error.as_deref(), Some("not_cloud_paired"));
}

/// A paired state with a live api key (built via the on-disk reader so the
/// `api_key()` gate, which keys on `paired`, returns Some).
fn paired_state() -> PairingState {
    use std::io::Write;
    let mut p = std::env::temp_dir();
    p.push(format!("ados-logpush-pair-{}.json", std::process::id()));
    let mut f = std::fs::File::create(&p).unwrap();
    f.write_all(br#"{"paired":true,"api_key":"k-123","owner_id":"u1"}"#)
        .unwrap();
    let s = PairingState::load_from(&p);
    let _ = std::fs::remove_file(&p);
    s
}

// ── result file write ───────────────────────────────────────────────

#[test]
fn write_result_is_readable_back() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("logd-push-result.json");
    let r = PushResult {
        pushed: 1,
        deduped: 0,
        bytes: 123,
        error: None,
    };
    write_result(&path, &r);
    let text = std::fs::read_to_string(&path).unwrap();
    let back: PushResult = serde_json::from_str(&text).unwrap();
    assert_eq!(r, back);
    // No leftover temp file.
    assert!(!path.with_extension("json.tmp").exists());
}

// ── http response parsing (chunked + content-length) ────────────────

#[test]
fn parse_http_response_reads_content_length_body() {
    let raw = b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello";
    let (status, body) = parse_http_response(raw.to_vec()).unwrap();
    assert_eq!(status, 200);
    assert_eq!(body, b"hello");
}

#[test]
fn parse_http_response_de_chunks_a_chunked_body() {
    // Two chunks ("ab" then "cde") then the terminator, binary-safe.
    let raw =
        b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n2\r\nab\r\n3\r\ncde\r\n0\r\n\r\n";
    let (status, body) = parse_http_response(raw.to_vec()).unwrap();
    assert_eq!(status, 200);
    assert_eq!(body, b"abcde");
}

#[test]
fn parse_http_response_surfaces_non_2xx_status() {
    let raw = b"HTTP/1.1 403 Forbidden\r\nContent-Length: 2\r\n\r\nno";
    let (status, body) = parse_http_response(raw.to_vec()).unwrap();
    assert_eq!(status, 403);
    assert_eq!(body, b"no");
}

#[test]
fn log_push_body_no_realloc() {
    // The parsed body reuses the response buffer: same allocation, same
    // capacity, for both a plain and a chunked body.
    let mut raw = b"HTTP/1.1 200 OK\r\nContent-Length: 4096\r\n\r\n".to_vec();
    raw.extend_from_slice(&[7u8; 4096]);
    let (ptr, cap) = (raw.as_ptr(), raw.capacity());
    let (_, body) = parse_http_response(raw).unwrap();
    assert_eq!(body.len(), 4096);
    assert_eq!((body.as_ptr(), body.capacity()), (ptr, cap));

    let mut raw = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n".to_vec();
    for _ in 0..4 {
        raw.extend_from_slice(b"400\r\n");
        raw.extend_from_slice(&[9u8; 0x400]);
        raw.extend_from_slice(b"\r\n");
    }
    raw.extend_from_slice(b"0\r\n\r\n");
    let (ptr, cap) = (raw.as_ptr(), raw.capacity());
    let (_, body) = parse_http_response(raw).unwrap();
    assert_eq!(body, vec![9u8; 4 * 0x400]);
    assert_eq!((body.as_ptr(), body.capacity()), (ptr, cap));
}

#[test]
fn a_truncated_chunk_keeps_what_arrived() {
    let raw = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nab";
    let (_, body) = parse_http_response(raw.to_vec()).unwrap();
    assert_eq!(body, b"ab");
}

// ── jsonl.zst row count (the real X-ADOS-Row-Count source) ──────────────

#[test]
fn count_jsonl_rows_counts_records_in_a_compressed_window() {
    // Three records, one JSON object per newline-terminated line, exactly the
    // export's line format.
    let jsonl = b"{\"a\":1}\n{\"b\":2}\n{\"c\":3}\n";
    let zst = zstd::encode_all(&jsonl[..], 3).unwrap();
    assert_eq!(count_jsonl_rows(&zst), 3);
}

#[test]
fn count_jsonl_rows_is_zero_for_an_empty_window() {
    let zst = zstd::encode_all(&b""[..], 3).unwrap();
    assert_eq!(count_jsonl_rows(&zst), 0);
}

#[test]
fn count_jsonl_rows_is_zero_not_a_panic_on_garbage() {
    // A non-zstd buffer must not panic; the count is metadata, never a gate.
    assert_eq!(count_jsonl_rows(b"not a zstd frame at all"), 0);
    assert_eq!(count_jsonl_rows(&[]), 0);
}

#[test]
fn count_jsonl_rows_streams_a_large_window_in_constant_memory() {
    // A window larger than the internal read buffer exercises the streaming
    // loop, not a single read.
    let mut jsonl = Vec::new();
    for i in 0..50_000u32 {
        jsonl.extend_from_slice(format!("{{\"i\":{i}}}\n").as_bytes());
    }
    let zst = zstd::encode_all(&jsonl[..], 3).unwrap();
    assert_eq!(count_jsonl_rows(&zst), 50_000);
}

// ── oversized windows split into consecutive sub-windows ────────────────

/// A store of rows at given timestamps, each `row_bytes` long once
/// exported. An export over `cap` bytes is reported too large, the way the
/// live sink reports a window over [`MAX_WINDOW_BYTES`].
struct FakeSink {
    rows: Vec<i64>,
    row_bytes: usize,
    cap: usize,
    uploads: Vec<(Option<i64>, i64, usize)>,
}

impl FakeSink {
    fn rows_in(&self, from: Option<i64>, to: i64) -> usize {
        self.rows
            .iter()
            .filter(|&&ts| from.is_none_or(|f| ts >= f) && ts < to)
            .count()
    }
}

#[async_trait::async_trait]
impl WindowSink for FakeSink {
    async fn export(&mut self, from: Option<i64>, to: i64) -> Result<WindowExport, String> {
        let bytes = self.rows_in(from, to) * self.row_bytes;
        if bytes > self.cap {
            Ok(WindowExport::TooLarge)
        } else {
            Ok(WindowExport::Bytes(vec![0u8; bytes]))
        }
    }
    async fn oldest_us(&mut self) -> Option<i64> {
        self.rows.iter().copied().min()
    }
    async fn upload(
        &mut self,
        from: Option<i64>,
        to: i64,
        bytes: Vec<u8>,
    ) -> Result<IngestDecision, String> {
        self.uploads.push((from, to, bytes.len()));
        Ok(IngestDecision::from_status("inserted"))
    }
}

fn sink(rows: Vec<i64>) -> FakeSink {
    FakeSink {
        rows,
        row_bytes: 1024 * 1024,
        cap: MAX_WINDOW_BYTES,
        uploads: Vec::new(),
    }
}

/// A 100 MB backlog with no lower bound goes out as consecutive windows,
/// oldest first, each within the cap, covering every row exactly once.
#[tokio::test]
async fn a_window_over_the_cap_is_pushed_as_consecutive_sub_windows() {
    let mut s = sink((1_000..1_100).collect());
    let out = push_kind_windows(&mut s, None, 2_000).await.unwrap();
    assert!(s.uploads.len() > 1, "the backlog was split");
    assert!(s.uploads.iter().all(|&(_, _, len)| len <= MAX_WINDOW_BYTES));
    let total: usize = s.uploads.iter().map(|&(_, _, len)| len).sum();
    assert_eq!(total, 100 * 1024 * 1024, "every row uploaded exactly once");
    // Ordered and non-overlapping: each window starts at or after the
    // previous one's end (empty stretches between them are skipped), and
    // the first keeps the request's open lower bound.
    assert_eq!(s.uploads[0].0, None);
    for pair in s.uploads.windows(2) {
        assert!(pair[1].0.unwrap() >= pair[0].1, "{pair:?}");
    }
    assert_eq!(out.pushed as usize, s.uploads.len());
    assert_eq!(out.bytes, 100 * 1024 * 1024);
}

/// A window that fits is uploaded once, unsplit.
#[tokio::test]
async fn a_window_at_the_cap_is_not_split() {
    let mut s = sink((0..8).collect());
    push_kind_windows(&mut s, Some(0), 100).await.unwrap();
    assert_eq!(s.uploads, vec![(Some(0), 100, MAX_WINDOW_BYTES)]);
}

/// Rows packed into one microsecond cannot be split below the cap.
#[tokio::test]
async fn an_unsplittable_window_reports_too_large() {
    let mut s = sink(vec![5; 40]);
    assert_eq!(
        push_kind_windows(&mut s, Some(5), 6).await,
        Err("window_too_large".to_string())
    );
    assert!(s.uploads.is_empty());
}

#[tokio::test]
async fn nothing_unsynced_reports_an_empty_window() {
    let mut s = sink(Vec::new());
    assert_eq!(
        push_kind_windows(&mut s, None, 100).await,
        Err("empty_window".to_string())
    );
}

// ── bounded socket read (the OOM guard) ─────────────────────────────────

#[test]
fn read_cap_leaves_slack_above_the_window_cap() {
    // The read ceiling sits just above the upload window cap so a legitimate
    // at-cap window plus its HTTP framing is not rejected, while a runaway
    // body is still bounded well before it can exhaust memory.
    const { assert!(MAX_READ_BYTES > MAX_WINDOW_BYTES) };
    const { assert!(MAX_READ_BYTES - MAX_WINDOW_BYTES == READ_SLACK_BYTES) };
}
