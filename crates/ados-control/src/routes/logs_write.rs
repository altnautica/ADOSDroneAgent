//! Operator cloud-export trigger write route.
//!
//! One operator action the GCS logs view fires:
//!
//! - **`POST /api/logs/push`** — request an explicit cloud export of a chosen log
//!   window. The body is an all-optional `{session?, since?, kinds?, wait?}`.
//!
//! ## What this route does, faithfully to the residual handler
//!
//! This is a thin TRIGGER, not an exporter. It validates the operator's window
//! selector, writes a request file on the runtime tmpfs (`logd-push-request.json`)
//! for the long-running cloud service to consume, and reports the outcome the
//! cloud service writes back (`logd-push-result.json`). It never reads the store,
//! uploads bytes, or marks rows itself — the cloud service owns those (and refuses
//! the push when the agent is in local mode / not cloud-paired / has cloud push
//! disabled). So the whole effect that ports here is the selector validation + the
//! request-file write + the brief result poll, none of which needs a Python
//! manager or a daemon socket.
//!
//! ## The request-file contract (matched to `log_push_trigger.write_request`)
//!
//! `write_request` first clears any stale `logd-push-result.json` (so the poll
//! below cannot latch a prior run's result), then atomically writes (tmp sibling +
//! rename) the request file with the shape the cloud service consumes:
//!
//! ```json
//! {"session": <id|null>, "since_us": <us|null>, "kinds": [...],
//!  "request_id": "<32-hex uuid>", "requested_at_us": <us>}
//! ```
//!
//! ## The poll + the response (matched to `trigger_push` / `read_result`)
//!
//! With `wait` false the call returns as soon as the trigger is on disk: the
//! `pending: true` placeholder. With `wait` true (the default) it polls
//! `logd-push-result.json` every 0.2 s for up to 8 s for a result whose
//! `request_id` matches; on a match it returns the normalized outcome, on the
//! timeout the same `pending: true` placeholder. The status follows the residual
//! `status = 202 if result["pending"] else 200`: a landed result is `200`, and
//! BOTH the `wait` false path AND the `wait` true poll-timeout carry
//! `pending: true` → `202` (the cloud service may still complete on its own loop).
//! On the bench, where no cloud service answers, the waited call returns the
//! `pending` placeholder + `202` after the poll window, exactly as the residual
//! does.
//!
//! ## Validation envelopes (matched to the residual route)
//!
//! - a `kinds` that is not a list, a string, or absent → `400 {"error": {"code":
//!   "bad_kinds", "message": "kinds must be a list or comma string"}}`;
//! - a `session` that is not an integer (a JSON bool counts as an integer, like
//!   Python's `isinstance(int)`) → `400 {"error": {"code": "bad_session",
//!   "message": "session must be an integer"}}`;
//! - a malformed `since` → `400 {"error": {"code": "bad_since", "message": ...}}`;
//! - an unknown `kind` → `400 {"error": {"code": "bad_kind", "message": ...}}`;
//! - a request-file write fault → `400 {"error": {"code": "trigger_unavailable",
//!   "message": "could not write the push request: ..."}}`.

use std::path::Path;

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Map, Value};

mod selector;

use selector::*;

// ---------------------------------------------------------------------------
// Path seam: the runtime tmpfs request/result files.
// ---------------------------------------------------------------------------

// The request file (`logd-push-request.json`) and the result file
// (`logd-push-result.json`) live directly under the run dir
// (`ados_protocol::paths::run_dir`), mirroring the Python `LOGD_PUSH_REQUEST_PATH`
// / `LOGD_PUSH_RESULT_PATH`; the write + poll helpers join the filenames onto an
// explicit dir so a test drives both against a tempdir.

// ---------------------------------------------------------------------------
// Constants matching the Python module.
// ---------------------------------------------------------------------------

/// The window kinds the store exports, one table each. An empty selection means
/// all four; anything outside this set is rejected. Matches the Python
/// `log_push_trigger.VALID_KINDS`.
const VALID_KINDS: [&str; 4] = ["logs", "metrics", "events", "hw"];

/// How long the waited call polls for the cloud service's result before returning
/// the pending placeholder. Matches the Python `_DEFAULT_POLL_SECONDS`.
const DEFAULT_POLL_SECONDS: f64 = 8.0;

/// The interval between result-file polls. Matches the Python
/// `_POLL_INTERVAL_SECONDS`.
const POLL_INTERVAL_SECONDS: f64 = 0.2;

// ---------------------------------------------------------------------------
// Trigger write + result normalization.
// ---------------------------------------------------------------------------

/// Atomically write the trigger file and return its correlation id, clearing any
/// stale result first. The on-disk shape is the shared contract the cloud service
/// consumes: `{session, since_us, kinds, request_id, requested_at_us}`. Mirrors
/// the Python `write_request`. `Err(trigger_unavailable)` on a write fault.
fn write_request(req: &PushRequest, run_dir: &Path, now_us: i64) -> Result<String, TriggerError> {
    let request_id = new_request_id();

    let mut payload = Map::new();
    payload.insert(
        "session".to_string(),
        req.session.map(|s| json!(s)).unwrap_or(Value::Null),
    );
    payload.insert(
        "since_us".to_string(),
        req.since_us.map(|s| json!(s)).unwrap_or(Value::Null),
    );
    payload.insert("kinds".to_string(), json!(req.kinds));
    payload.insert("request_id".to_string(), json!(request_id));
    payload.insert("requested_at_us".to_string(), json!(now_us));
    let body = serde_json::to_vec(&Value::Object(payload)).map_err(|e| TriggerError {
        code: "trigger_unavailable",
        message: format!("could not write the push request: {e}"),
    })?;

    let request_path = run_dir.join("logd-push-request.json");
    let result_path = run_dir.join("logd-push-result.json");

    let write = || -> std::io::Result<()> {
        std::fs::create_dir_all(run_dir)?;
        // Clear any result left by a previous push so the poll cannot latch onto it.
        // A missing file is fine.
        match std::fs::remove_file(&result_path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        let tmp = run_dir.join(format!("logd-push-request.json.{request_id}.tmp"));
        std::fs::write(&tmp, &body)?;
        std::fs::rename(&tmp, &request_path)?;
        Ok(())
    };
    write().map_err(|e| TriggerError {
        code: "trigger_unavailable",
        message: format!("could not write the push request: {e}"),
    })?;

    Ok(request_id)
}

/// A 32-hex-char correlation id, matching the Python `uuid.uuid4().hex` shape (a
/// random 128-bit value rendered as lowercase hex). The bytes are random either
/// way; only the format is load-bearing for the cloud-service match.
fn new_request_id() -> String {
    let mut bytes = [0u8; 16];
    // getrandom is already a crate dep (the pairing key/code minting use it).
    if getrandom::fill(&mut bytes).is_err() {
        // A fall-back so the id is still 32 hex chars even if the OS RNG balks; the
        // cloud-service match only needs uniqueness within this process's run.
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        bytes[..16].copy_from_slice(&nanos.to_le_bytes());
    }
    hex::encode(bytes)
}

/// The `pending: true` placeholder the trigger returns when `wait` is false or the
/// poll window closes with no matching result. Mirrors the dict the Python
/// `trigger_push` (wait=False) and `read_result` (timeout) return.
fn pending_placeholder(request_id: &str) -> Value {
    json!({
        "accepted": true,
        "request_id": request_id,
        "pushed": false,
        "deduped": false,
        "bytes": 0,
        "rows": 0,
        "synced": false,
        "error": null,
        "pending": true,
    })
}

/// Read the result file if present and well-formed, else `None`. Mirrors the
/// Python `_read_result_file`.
fn read_result_file(result_path: &Path) -> Option<Value> {
    let text = std::fs::read_to_string(result_path).ok()?;
    match serde_json::from_str::<Value>(&text) {
        Ok(v) if v.is_object() => Some(v),
        _ => None,
    }
}

/// Coerce the cloud service's result onto the stable outcome shape, filling in
/// defaults for any field an older service omits. Matches the Python
/// `_normalize_result(raw, accepted=True)`.
fn normalize_result(raw: &Value) -> Value {
    let error = raw.get("error");
    let error_value = match error {
        Some(e) if !e.is_null() => {
            // Python `str(error) if error else None`: a falsey value (false / 0 /
            // "" / [] / {}) becomes null; otherwise its string form.
            if json_truthy(e) {
                json!(value_to_python_str(e))
            } else {
                Value::Null
            }
        }
        _ => Value::Null,
    };
    json!({
        "accepted": true,
        "request_id": raw.get("request_id").cloned().unwrap_or(Value::Null),
        "pushed": python_bool(raw.get("pushed")),
        "deduped": python_bool(raw.get("deduped")),
        "bytes": python_int(raw.get("bytes")),
        "rows": python_int(raw.get("rows")),
        "synced": python_bool(raw.get("synced")),
        "window_id": raw.get("window_id").cloned().unwrap_or(Value::Null),
        "sha256": raw.get("sha256").cloned().unwrap_or(Value::Null),
        "error": error_value,
        "pending": false,
    })
}

/// Python `bool(raw.get(key, False))` over an optional JSON value.
fn python_bool(v: Option<&Value>) -> bool {
    v.map(json_truthy).unwrap_or(false)
}

/// Python `int(raw.get(key, 0) or 0)` over an optional JSON value: a falsey value
/// (null / false / 0 / "" / [] / {}) is 0; a number is truncated to an integer; a
/// numeric string is parsed.
fn python_int(v: Option<&Value>) -> i64 {
    match v {
        None | Some(Value::Null) => 0,
        Some(Value::Bool(false)) => 0,
        Some(Value::Bool(true)) => 1,
        Some(Value::Number(n)) => n
            .as_i64()
            .or_else(|| n.as_f64().map(|f| f as i64))
            .unwrap_or(0),
        Some(Value::String(s)) => {
            if s.is_empty() {
                0
            } else {
                s.parse::<i64>()
                    .or_else(|_| s.parse::<f64>().map(|f| f as i64))
                    .unwrap_or(0)
            }
        }
        Some(_) => 0,
    }
}

/// Python `bool(x)` truthiness over a JSON value.
fn json_truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map(|f| f != 0.0).unwrap_or(true),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// Render a JSON value the way Python `str()` would for the `error` field: a
/// string is itself; everything else is its JSON-ish textual form. The cloud
/// service writes the error as a string in practice, so this only needs the
/// string-passthrough plus a best-effort for the rare non-string case.
fn value_to_python_str(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

// ---------------------------------------------------------------------------
// POST /api/logs/push handler.
// ---------------------------------------------------------------------------

/// `POST /api/logs/push` → request a cloud export of a chosen log window.
///
/// Parses the all-optional `{session?, since?, kinds?, wait?}` body, validates the
/// selector (the `400` error-object bodies on a bad kinds/session/since/kind),
/// writes the request file for the cloud service, and returns the outcome: `202`
/// with the pending placeholder when `wait` is false, else `200` with either the
/// landed result or (on the poll timeout) the pending placeholder.
pub async fn push_logs(body: Option<Json<Value>>) -> Response {
    // FastAPI's `Body(default_factory=dict)` defaults an absent/empty body to an
    // empty object; a non-object JSON body is read field-by-field as absent.
    let body = match body {
        Some(Json(v)) if v.is_object() => v,
        _ => json!({}),
    };
    push_logs_at(
        &body,
        &ados_protocol::paths::run_dir(),
        DEFAULT_POLL_SECONDS,
    )
    .await
}

/// The push logic against an explicit runtime dir + poll budget. The public
/// handler resolves the run dir from the env; this takes it directly so a test can
/// point it at a temp dir and bound the poll.
async fn push_logs_at(body: &Value, run_dir: &Path, poll_seconds: f64) -> Response {
    let now_us = now_unix_micros();

    // session: absent → None; a JSON integer (or bool, matching Python's
    // `isinstance(int)` where bool is an int subclass) → that integer; anything
    // else → 400 bad_session.
    let session = match body.get("session") {
        None | Some(Value::Null) => None,
        Some(Value::Bool(b)) => Some(if *b { 1 } else { 0 }),
        Some(Value::Number(n)) if n.is_i64() || n.is_u64() => Some(n.as_i64().unwrap_or(0)),
        Some(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": {"code": "bad_session", "message": "session must be an integer"}})),
            )
                .into_response();
        }
    };

    // kinds: None → None; a string → split on commas + trim non-empty; a list →
    // each element's string form; anything else → 400 bad_kinds.
    let kinds: Option<Vec<String>> = match body.get("kinds") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => Some(
            s.split(',')
                .map(str::trim)
                .filter(|k| !k.is_empty())
                .map(str::to_string)
                .collect(),
        ),
        Some(Value::Array(a)) => Some(a.iter().map(value_to_python_str).collect()),
        Some(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": {"code": "bad_kinds", "message": "kinds must be a list or comma string"}})),
            )
                .into_response();
        }
    };

    // since: stringified when present (matching `str(since) if since is not None`).
    let since_owned: Option<String> = match body.get("since") {
        None | Some(Value::Null) => None,
        Some(v) => Some(value_to_python_str(v)),
    };
    let since = since_owned.as_deref();

    // wait: default true (`body.get("wait", True)`), Python-truthy otherwise.
    let wait = match body.get("wait") {
        None => true,
        Some(v) => json_truthy(v),
    };

    // Validate the selector (parse_since + validate_kinds).
    let request = match build_request(session, since, kinds, now_us) {
        Ok(r) => r,
        Err(e) => return e.response(),
    };

    // Write the trigger file (clears a stale result first), then resolve the
    // outcome the same way the residual `trigger_push` does: with `wait` false the
    // immediate pending placeholder; with `wait` true a brief poll of the result
    // file (a matching result, else the pending placeholder on the timeout).
    let request_id = match write_request(&request, run_dir, now_us) {
        Ok(id) => id,
        Err(e) => return e.response(),
    };
    let result = if wait {
        poll_result(&request_id, run_dir, poll_seconds).await
    } else {
        pending_placeholder(&request_id)
    };

    // The residual `status = 202 if result["pending"] else 200`: a landed result is
    // 200, and BOTH the wait-false path AND the wait-true poll-timeout carry
    // pending:true → 202 (the cloud service may still complete on its own loop).
    let status = if result
        .get("pending")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        StatusCode::ACCEPTED
    } else {
        StatusCode::OK
    };
    (status, Json(result)).into_response()
}

/// Poll the result file every [`POLL_INTERVAL_SECONDS`] for up to `poll_seconds`
/// for a result whose `request_id` matches, returning the normalized result on a
/// match or the pending placeholder on the timeout. Matches the Python
/// `read_result`.
async fn poll_result(request_id: &str, run_dir: &Path, poll_seconds: f64) -> Value {
    let result_path = run_dir.join("logd-push-result.json");
    let deadline =
        std::time::Instant::now() + std::time::Duration::from_secs_f64(poll_seconds.max(0.0));
    let interval = std::time::Duration::from_secs_f64(POLL_INTERVAL_SECONDS);
    loop {
        if let Some(raw) = read_result_file(&result_path) {
            if raw.get("request_id").and_then(Value::as_str) == Some(request_id) {
                return normalize_result(&raw);
            }
        }
        if std::time::Instant::now() >= deadline {
            return pending_placeholder(request_id);
        }
        tokio::time::sleep(interval).await;
    }
}

/// The current wall-clock time in microseconds since the Unix epoch (mirrors the
/// Python `int(time.time() * 1_000_000)`).
fn now_unix_micros() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_micros() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests;
