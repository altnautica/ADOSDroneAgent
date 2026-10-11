use super::*;

/// Read a response body as JSON.
async fn body_json(resp: Response) -> Value {
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

// ── selector validation ───────────────────────────────────────────────────

#[test]
fn parse_since_handles_the_three_forms_and_rejects_garbage() {
    let now = 10_000_000_000; // a fixed "now" in microseconds.
                              // Absent / empty → None.
    assert_eq!(parse_since(None, now).unwrap(), None);
    assert_eq!(parse_since(Some("  "), now).unwrap(), None);
    // Relative: -5m → now - 5*60*1e6.
    assert_eq!(
        parse_since(Some("-5m"), now).unwrap(),
        Some(now - 300_000_000)
    );
    assert_eq!(
        parse_since(Some("-500ms"), now).unwrap(),
        Some(now - 500_000)
    );
    // Absolute epoch-us integer.
    assert_eq!(parse_since(Some("123456"), now).unwrap(), Some(123_456));
    // ISO UTC: 2021-01-01T00:00:00 = 1609459200 s = 1609459200000000 us.
    assert_eq!(
        parse_since(Some("2021-01-01T00:00:00"), now).unwrap(),
        Some(1_609_459_200_000_000)
    );
    // A space separator is accepted.
    assert_eq!(
        parse_since(Some("2021-01-01 00:00:00"), now).unwrap(),
        Some(1_609_459_200_000_000)
    );
    // Garbage relative + garbage non-iso → bad_since.
    assert_eq!(parse_since(Some("-5x"), now).unwrap_err().code, "bad_since");
    assert_eq!(
        parse_since(Some("not-a-time"), now).unwrap_err().code,
        "bad_since"
    );
}

#[test]
fn validate_kinds_defaults_all_four_and_rejects_unknown() {
    // Absent / empty → all four in canonical order.
    assert_eq!(
        validate_kinds(None).unwrap(),
        vec!["logs", "metrics", "events", "hw"]
    );
    assert_eq!(
        validate_kinds(Some(vec![])).unwrap(),
        vec!["logs", "metrics", "events", "hw"]
    );
    // A subset, lower-cased + de-duped + order-preserved.
    assert_eq!(
        validate_kinds(Some(vec![
            "Events".to_string(),
            "logs".to_string(),
            "EVENTS".to_string()
        ]))
        .unwrap(),
        vec!["events", "logs"]
    );
    // An unknown kind → bad_kind.
    let err = validate_kinds(Some(vec!["bogus".to_string()])).unwrap_err();
    assert_eq!(err.code, "bad_kind");
    assert!(err.message.contains("bogus"));
}

// ── the request-file contract ─────────────────────────────────────────────

#[test]
fn write_request_writes_the_contract_shape_and_clears_a_stale_result() {
    let dir = tempfile::tempdir().unwrap();
    // Seed a stale result so write_request must clear it.
    std::fs::write(
        dir.path().join("logd-push-result.json"),
        r#"{"request_id":"old"}"#,
    )
    .unwrap();

    let req = PushRequest {
        session: Some(7),
        since_us: Some(123),
        kinds: vec!["logs".to_string(), "hw".to_string()],
    };
    let id = write_request(&req, dir.path(), 999).unwrap();
    assert_eq!(id.len(), 32);
    assert!(id.chars().all(|c| c.is_ascii_hexdigit()));

    // The stale result is gone.
    assert!(!dir.path().join("logd-push-result.json").exists());

    // The request file carries the exact contract shape.
    let written: Value = serde_json::from_str(
        &std::fs::read_to_string(dir.path().join("logd-push-request.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(written["session"], json!(7));
    assert_eq!(written["since_us"], json!(123));
    assert_eq!(written["kinds"], json!(["logs", "hw"]));
    assert_eq!(written["request_id"], json!(id));
    assert_eq!(written["requested_at_us"], json!(999));
}

#[test]
fn write_request_writes_nulls_for_absent_session_and_since() {
    let dir = tempfile::tempdir().unwrap();
    let req = PushRequest {
        session: None,
        since_us: None,
        kinds: vec!["logs".to_string()],
    };
    write_request(&req, dir.path(), 1).unwrap();
    let written: Value = serde_json::from_str(
        &std::fs::read_to_string(dir.path().join("logd-push-request.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(written["session"], Value::Null);
    assert_eq!(written["since_us"], Value::Null);
}

// ── result normalization ──────────────────────────────────────────────────

#[test]
fn normalize_fills_defaults_and_coerces_types() {
    let raw = json!({
        "request_id": "abc",
        "pushed": true,
        "deduped": 0,
        "bytes": "2048",
        "rows": 5,
        "synced": 1,
        "window_id": 42,
        "sha256": "deadbeef",
        "error": "boom",
    });
    let out = normalize_result(&raw);
    assert_eq!(out["accepted"], json!(true));
    assert_eq!(out["request_id"], json!("abc"));
    assert_eq!(out["pushed"], json!(true));
    assert_eq!(out["deduped"], json!(false));
    assert_eq!(out["bytes"], json!(2048));
    assert_eq!(out["rows"], json!(5));
    assert_eq!(out["synced"], json!(true));
    assert_eq!(out["window_id"], json!(42));
    assert_eq!(out["sha256"], json!("deadbeef"));
    assert_eq!(out["error"], json!("boom"));
    assert_eq!(out["pending"], json!(false));
}

#[test]
fn normalize_nulls_a_falsey_error_and_missing_fields() {
    let raw = json!({"error": ""});
    let out = normalize_result(&raw);
    assert_eq!(out["error"], Value::Null);
    // Missing numerics default to 0, missing bools to false, ids to null.
    assert_eq!(out["bytes"], json!(0));
    assert_eq!(out["rows"], json!(0));
    assert_eq!(out["pushed"], json!(false));
    assert_eq!(out["request_id"], Value::Null);
    assert_eq!(out["window_id"], Value::Null);
}

// ── the handler: wait=false → 202 pending ─────────────────────────────────

#[tokio::test]
async fn wait_false_returns_a_202_pending_placeholder_and_writes_the_trigger() {
    let dir = tempfile::tempdir().unwrap();
    let body = json!({"wait": false, "kinds": ["logs"], "session": 3});
    let resp = push_logs_at(&body, dir.path(), 8.0).await;
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    let out = body_json(resp).await;
    assert_eq!(out["accepted"], json!(true));
    assert_eq!(out["pending"], json!(true));
    assert_eq!(out["pushed"], json!(false));
    assert!(out["request_id"].as_str().unwrap().len() == 32);
    // The trigger file was written with the session + kinds.
    let written: Value = serde_json::from_str(
        &std::fs::read_to_string(dir.path().join("logd-push-request.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(written["session"], json!(3));
    assert_eq!(written["kinds"], json!(["logs"]));
}

// ── the handler: wait=true with no service → 202 pending after the poll ───

#[tokio::test]
async fn wait_true_with_no_result_returns_a_202_pending_after_the_poll() {
    let dir = tempfile::tempdir().unwrap();
    // A tiny poll budget so the test does not block; no result file lands. The
    // pending placeholder carries pending:true → 202 (matching the residual
    // `status = 202 if result["pending"] else 200`), the same status the
    // wait=false path returns.
    let body = json!({"wait": true});
    let resp = push_logs_at(&body, dir.path(), 0.05).await;
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    let out = body_json(resp).await;
    assert_eq!(out["pending"], json!(true));
    assert_eq!(out["accepted"], json!(true));
    // kinds defaulted to all four → the trigger carries them.
    let written: Value = serde_json::from_str(
        &std::fs::read_to_string(dir.path().join("logd-push-request.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(written["kinds"], json!(["logs", "metrics", "events", "hw"]));
}

// ── the handler: wait=true with a matching result → 200 normalized ────────

#[tokio::test]
async fn wait_true_with_a_matching_result_returns_the_200_normalized_outcome() {
    let dir = tempfile::tempdir().unwrap();
    // Pre-seed a result whose request_id we cannot know yet, so write the trigger
    // first via the handler with wait=false to learn the id, THEN drop a matching
    // result and call again. Simpler: drive write_request directly, then poll.
    let req = PushRequest {
        session: None,
        since_us: None,
        kinds: vec!["logs".to_string()],
    };
    let id = write_request(&req, dir.path(), 1).unwrap();
    std::fs::write(
        dir.path().join("logd-push-result.json"),
        serde_json::to_vec(&json!({
            "request_id": id,
            "pushed": true,
            "bytes": 4096,
            "rows": 12,
            "synced": true,
        }))
        .unwrap(),
    )
    .unwrap();
    let out = poll_result(&id, dir.path(), 1.0).await;
    assert_eq!(out["pending"], json!(false));
    assert_eq!(out["pushed"], json!(true));
    assert_eq!(out["bytes"], json!(4096));
    assert_eq!(out["rows"], json!(12));
    assert_eq!(out["synced"], json!(true));
    assert_eq!(out["request_id"], json!(id));
}

// ── the handler: bad selectors → 400 error objects ────────────────────────

#[tokio::test]
async fn a_non_int_session_is_a_400_bad_session() {
    let dir = tempfile::tempdir().unwrap();
    let body = json!({"session": "not-an-int"});
    let resp = push_logs_at(&body, dir.path(), 0.05).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let out = body_json(resp).await;
    assert_eq!(
        out,
        json!({"error": {"code": "bad_session", "message": "session must be an integer"}})
    );
    // No trigger written on a validation failure.
    assert!(!dir.path().join("logd-push-request.json").exists());
}

#[tokio::test]
async fn a_non_list_non_string_kinds_is_a_400_bad_kinds() {
    let dir = tempfile::tempdir().unwrap();
    let body = json!({"kinds": 5});
    let resp = push_logs_at(&body, dir.path(), 0.05).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let out = body_json(resp).await;
    assert_eq!(
        out,
        json!({"error": {"code": "bad_kinds", "message": "kinds must be a list or comma string"}})
    );
}

#[tokio::test]
async fn a_bad_since_is_a_400_bad_since() {
    let dir = tempfile::tempdir().unwrap();
    let body = json!({"since": "-9x"});
    let resp = push_logs_at(&body, dir.path(), 0.05).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let out = body_json(resp).await;
    assert_eq!(out["error"]["code"], json!("bad_since"));
}

#[tokio::test]
async fn an_unknown_kind_is_a_400_bad_kind() {
    let dir = tempfile::tempdir().unwrap();
    let body = json!({"kinds": ["logs", "nope"]});
    let resp = push_logs_at(&body, dir.path(), 0.05).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let out = body_json(resp).await;
    assert_eq!(out["error"]["code"], json!("bad_kind"));
}

// ── a JSON bool session is accepted (Python isinstance(int) on bool) ──────

#[tokio::test]
async fn a_bool_session_is_accepted_as_an_integer() {
    let dir = tempfile::tempdir().unwrap();
    // Python: isinstance(True, int) is True, so session=True passes the guard
    // and is written as the integer 1.
    let body = json!({"session": true, "wait": false});
    let resp = push_logs_at(&body, dir.path(), 0.05).await;
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    let written: Value = serde_json::from_str(
        &std::fs::read_to_string(dir.path().join("logd-push-request.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(written["session"], json!(1));
}

// ── since string-coercion mirrors `str(since)` ────────────────────────────

#[test]
fn civil_to_unix_secs_round_trips_known_epochs() {
    assert_eq!(civil_to_unix_secs("1970-01-01T00:00:00"), Some(0));
    assert_eq!(
        civil_to_unix_secs("2021-01-01T00:00:00"),
        Some(1_609_459_200)
    );
    // A bad shape is None.
    assert_eq!(civil_to_unix_secs("2021-01-01"), None); // no time part
    assert_eq!(civil_to_unix_secs("not-a-date"), None);
}
