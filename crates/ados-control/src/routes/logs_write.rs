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

use std::path::{Path, PathBuf};

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Map, Value};

// ---------------------------------------------------------------------------
// Path seam: the runtime tmpfs request/result files.
// ---------------------------------------------------------------------------

/// The runtime dir (`ADOS_RUN_DIR`, default `/run/ados`), the same override the
/// sibling sockets + sentinels resolve under. The request file
/// (`logd-push-request.json`) and the result file (`logd-push-result.json`) live
/// directly under it, mirroring the Python `LOGD_PUSH_REQUEST_PATH` /
/// `LOGD_PUSH_RESULT_PATH`; the write + poll helpers join the filenames onto the
/// resolved dir so a test can redirect both with one `ADOS_RUN_DIR` override.
fn run_dir() -> PathBuf {
    PathBuf::from(std::env::var("ADOS_RUN_DIR").unwrap_or_else(|_| "/run/ados".to_string()))
}

// ---------------------------------------------------------------------------
// Constants matching the Python module.
// ---------------------------------------------------------------------------

/// The window kinds the store exports, one table each. An empty selection means
/// all four; anything outside this set is rejected. Mirrors the Python
/// `VALID_KINDS`.
const VALID_KINDS: [&str; 4] = ["logs", "metrics", "events", "hw"];

/// How long the waited call polls for the cloud service's result before returning
/// the pending placeholder. Mirrors the Python `_DEFAULT_POLL_SECONDS`.
const DEFAULT_POLL_SECONDS: f64 = 8.0;

/// The interval between result-file polls. Mirrors the Python
/// `_POLL_INTERVAL_SECONDS`.
const POLL_INTERVAL_SECONDS: f64 = 0.2;

// ---------------------------------------------------------------------------
// Selector validation (parse_since / validate_kinds), with the typed error.
// ---------------------------------------------------------------------------

/// A validation failure that maps to a `400 {"error": {"code, message}}` body,
/// carrying the stable code the residual `LogPushTriggerError` carries.
#[derive(Debug)]
struct TriggerError {
    code: &'static str,
    message: String,
}

impl TriggerError {
    fn response(&self) -> Response {
        (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": {"code": self.code, "message": self.message}})),
        )
            .into_response()
    }
}

/// The validated window selector ready to be written as a trigger file. Mirrors
/// the Python `PushRequest`.
struct PushRequest {
    session: Option<i64>,
    since_us: Option<i64>,
    kinds: Vec<String>,
}

/// Resolve a `since` string to an epoch-microsecond lower bound, accepting the
/// same three forms the store's query API accepts (mirrors `parse_since`):
/// a relative `-Ns`/`-Nm`/`-Nh`/`-Nd`/`-Nms`, an absolute epoch-microsecond
/// integer, or a bare `YYYY-MM-DDTHH:MM:SS` (space/`T` separator) read as UTC.
/// `None` for an empty/absent value; `Err(bad_since)` on a malformed input.
fn parse_since(value: Option<&str>, now_us: i64) -> Result<Option<i64>, TriggerError> {
    let s = match value {
        None => return Ok(None),
        Some(v) => v.trim(),
    };
    if s.is_empty() {
        return Ok(None);
    }

    if let Some(rest) = s.strip_prefix('-') {
        match parse_relative(rest) {
            Some(micros) => return Ok(Some(now_us - micros)),
            None => {
                return Err(TriggerError {
                    code: "bad_since",
                    message: format!("{s:?} is not a relative duration like -5m, -2h, -1d"),
                });
            }
        }
    }

    if let Ok(n) = s.parse::<i64>() {
        return Ok(Some(n));
    }

    if let Some(iso) = parse_iso_utc(s) {
        return Ok(Some(iso));
    }

    Err(TriggerError {
        code: "bad_since",
        message: format!(
            "{s:?} is not an epoch-microsecond integer, an ISO timestamp, \
or a relative duration like -5m"
        ),
    })
}

/// Parse a `90s` / `5m` / `2h` / `1d` / `500ms` magnitude+unit into microseconds.
/// `None` on a malformed input. Mirrors the Python `_parse_relative`.
fn parse_relative(rest: &str) -> Option<i64> {
    let rest = rest.trim();
    // Split at the first ASCII alphabetic char (the unit), mirroring the Python
    // `next((i for i,c in enumerate(rest) if c.isascii() and c.isalpha()), None)`.
    let split = rest
        .char_indices()
        .find(|(_, c)| c.is_ascii_alphabetic())
        .map(|(i, _)| i)?;
    let (num, unit) = rest.split_at(split);
    let magnitude: i64 = num.trim().parse().ok()?;
    if magnitude < 0 {
        return None;
    }
    let per_unit_us: i64 = match unit {
        "ms" => 1_000,
        "s" => 1_000_000,
        "m" => 60 * 1_000_000,
        "h" => 3_600 * 1_000_000,
        "d" => 86_400 * 1_000_000,
        _ => return None,
    };
    Some(magnitude * per_unit_us)
}

/// Parse a bare `YYYY-MM-DDTHH:MM:SS` (space or `T` separator), read as UTC, into
/// an epoch-microsecond integer. `None` when it is not that shape. Mirrors the
/// Python `_parse_iso_utc` (`datetime.fromisoformat` after normalizing the
/// separator + stripping a trailing `Z`, then read as UTC when naive).
fn parse_iso_utc(s: &str) -> Option<i64> {
    let normalized = s.replace(' ', "T");
    let normalized = normalized.strip_suffix('Z').unwrap_or(&normalized);
    let secs = civil_to_unix_secs(normalized)?;
    Some(secs * 1_000_000)
}

/// Parse a naive `YYYY-MM-DDTHH:MM:SS` (no offset; an offset makes this `None`
/// since the residual `_parse_iso_utc` only ever feeds a naive timestamp through —
/// `fromisoformat` of an aware string keeps its own tz, but the bare form the
/// store query uses is naive) into a Unix-epoch second count read as UTC. The
/// civil-to-days conversion is the inverse of the sibling read module's
/// `iso8601_from_unix_secs`.
fn civil_to_unix_secs(s: &str) -> Option<i64> {
    let (date, time) = s.split_once('T')?;
    let mut dparts = date.split('-');
    let year: i64 = dparts.next()?.parse().ok()?;
    let month: i64 = dparts.next()?.parse().ok()?;
    let day: i64 = dparts.next()?.parse().ok()?;
    if dparts.next().is_some() {
        return None;
    }
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }

    let mut tparts = time.split(':');
    let hour: i64 = tparts.next()?.parse().ok()?;
    let minute: i64 = tparts.next()?.parse().ok()?;
    let sec_str = tparts.next()?;
    if tparts.next().is_some() {
        return None;
    }
    // A fractional second is accepted (truncated to whole seconds; the store query
    // form is whole-second, and `fromisoformat` would carry the fraction, but the
    // epoch-second floor is what the lower bound needs).
    let second: i64 = match sec_str.split_once('.') {
        Some((whole, _frac)) => whole.parse().ok()?,
        None => sec_str.parse().ok()?,
    };
    if !(0..=23).contains(&hour) || !(0..=59).contains(&minute) || !(0..=60).contains(&second) {
        return None;
    }

    // Howard Hinnant's days_from_civil: (y, m, d) → days since the Unix epoch.
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400; // [0, 399]
    let doy = (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + day - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    let days = era * 146_097 + doe - 719_468;

    Some(days * 86_400 + hour * 3_600 + minute * 60 + second)
}

/// Normalize and validate the requested window kinds. An empty/absent list selects
/// all four; an unknown kind is `Err(bad_kind)`; order is preserved and duplicates
/// dropped. Mirrors the Python `validate_kinds`.
fn validate_kinds(kinds: Option<Vec<String>>) -> Result<Vec<String>, TriggerError> {
    let kinds = match kinds {
        Some(k) if !k.is_empty() => k,
        _ => return Ok(VALID_KINDS.iter().map(|s| s.to_string()).collect()),
    };
    let mut seen: Vec<String> = Vec::new();
    for raw in &kinds {
        let k = raw.trim().to_ascii_lowercase();
        if !VALID_KINDS.contains(&k.as_str()) {
            return Err(TriggerError {
                code: "bad_kind",
                message: format!(
                    "unknown kind {raw:?}; choose from {}",
                    VALID_KINDS.join(", ")
                ),
            });
        }
        if !seen.contains(&k) {
            seen.push(k);
        }
    }
    Ok(seen)
}

/// Validate the operator's selector into a `PushRequest`, mirroring the Python
/// `build_request` (`parse_since` then `validate_kinds`).
fn build_request(
    session: Option<i64>,
    since: Option<&str>,
    kinds: Option<Vec<String>>,
    now_us: i64,
) -> Result<PushRequest, TriggerError> {
    let since_us = parse_since(since, now_us)?;
    let kinds = validate_kinds(kinds)?;
    Ok(PushRequest {
        session,
        since_us,
        kinds,
    })
}

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
    if getrandom::getrandom(&mut bytes).is_err() {
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
/// defaults for any field an older service omits. Mirrors the Python
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
    push_logs_at(&body, &run_dir(), DEFAULT_POLL_SECONDS).await
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
/// match or the pending placeholder on the timeout. Mirrors the Python
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
mod tests {
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
}
