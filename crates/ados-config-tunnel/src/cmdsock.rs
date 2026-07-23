//! The ground-side command socket the injector serves.
//!
//! `ados-control`'s relayed-config route forwards one newline-JSON request per
//! connection here (the same framing every GS data-plane command socket uses);
//! this module plans the request against the default-off gates and, when
//! allowed, runs it through the [`Injector`].
//!
//! ```text
//!   {"op":"config_request","request":{"op":"get"},"timeout_ms":8000}
//!       -> {"ok":true,"is_error":false,"response":<config JSON>}
//!       -> {"ok":true,"is_error":true,"response":<error envelope>}  // drone-side reject
//!       -> {"ok":false,"error":"E_TIMEOUT"}                         // no reply
//!   {"op":"status"} -> {"ok":true, …the current sidecar body…}
//! ```
//!
//! Gating (the GS half of the default-off safety gate): with the channel
//! disabled every request is refused; with the channel enabled but
//! `command_enabled` off, a `put` (config WRITE) is refused BEFORE anything is
//! sent, so a write can never radiate until the safety gate is opened.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use ados_protocol::ipc::{bind_command_socket, serve_rpc};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::Mutex;

use crate::injector::{Injector, InjectorError};

/// Cap on a single request line.
const MAX_REQUEST_BYTES: usize = 64 * 1024;
/// Default per-request deadline when the caller does not pin one.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(8);
/// Ceiling on a caller-supplied deadline.
const MAX_TIMEOUT: Duration = Duration::from_secs(30);

/// The shared state the command handlers touch.
#[derive(Clone)]
pub struct CmdState {
    pub injector: Arc<Injector>,
    pub enabled: bool,
    pub command_enabled: bool,
    /// The latest sidecar body, kept fresh by the service loop, for `status`.
    pub latest_status: Arc<Mutex<Value>>,
}

#[derive(Debug, Deserialize)]
struct Request {
    op: String,
    #[serde(default)]
    request: Option<Value>,
    #[serde(default)]
    timeout_ms: Option<u64>,
}

/// The planned outcome of a request, decided purely (no injector, no I/O) so
/// the gate logic is testable.
#[derive(Debug, PartialEq, Eq)]
enum Plan {
    /// Serve the status body.
    Status,
    /// Forward `op_body` to the drone with `timeout`.
    Forward { op_body: Vec<u8>, timeout: Duration },
    /// Refuse with a ready reply (`{"ok":false,"error":…}`).
    Reject(Value),
}

fn reject(code: &str) -> Value {
    json!({"ok": false, "error": code})
}

/// Plan a request against the gates. Pure.
fn plan_request(line: &[u8], enabled: bool, command_enabled: bool) -> Plan {
    let req: Request = match serde_json::from_slice(line) {
        Ok(r) => r,
        Err(e) => return Plan::Reject(reject(&format!("E_BAD_REQUEST: {e}"))),
    };
    match req.op.as_str() {
        "status" => Plan::Status,
        "config_request" => {
            if !enabled {
                return Plan::Reject(reject("E_TUNNEL_DISABLED"));
            }
            let Some(inner) = req.request.filter(Value::is_object) else {
                return Plan::Reject(reject("E_MISSING_REQUEST"));
            };
            // A config WRITE is refused before it can radiate unless the write
            // gate is open.
            let is_write = inner.get("op").and_then(Value::as_str) == Some("put");
            if is_write && !command_enabled {
                return Plan::Reject(reject("E_WRITE_DISABLED"));
            }
            let op_body = match serde_json::to_vec(&inner) {
                Ok(b) => b,
                Err(_) => return Plan::Reject(reject("E_ENCODE")),
            };
            let timeout = req
                .timeout_ms
                .map(|ms| Duration::from_millis(ms).min(MAX_TIMEOUT))
                .unwrap_or(DEFAULT_TIMEOUT);
            Plan::Forward { op_body, timeout }
        }
        other => Plan::Reject(reject(&format!("E_UNKNOWN_OP: {other}"))),
    }
}

/// Parse a drone response body as JSON, else surface it as a raw string so a
/// non-JSON error is never silently dropped.
fn response_value(body: &[u8]) -> Value {
    serde_json::from_slice::<Value>(body)
        .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(body).into_owned()))
}

async fn dispatch(line: &[u8], state: &CmdState) -> Value {
    match plan_request(line, state.enabled, state.command_enabled) {
        Plan::Reject(v) => v,
        Plan::Status => {
            let body = state.latest_status.lock().await.clone();
            match body {
                Value::Object(map) => {
                    let mut out = serde_json::Map::with_capacity(map.len() + 1);
                    out.insert("ok".to_string(), Value::Bool(true));
                    out.extend(map);
                    Value::Object(out)
                }
                _ => json!({"ok": true, "status": Value::Null}),
            }
        }
        Plan::Forward { op_body, timeout } => {
            match state.injector.submit(&op_body, timeout).await {
                Ok(resp) => json!({
                    "ok": true,
                    "is_error": resp.is_error,
                    "response": response_value(&resp.body),
                }),
                Err(InjectorError::Timeout(_)) => reject("E_TIMEOUT"),
                Err(InjectorError::SendFailed) => reject("E_BEARER_SEND_FAILED"),
                Err(InjectorError::Chunk(_)) => reject("E_REQUEST_TOO_LARGE"),
            }
        }
    }
}

/// Bind + serve the command socket until the listener errors.
pub async fn serve(state: CmdState, sock_path: &Path) -> std::io::Result<()> {
    let listener = bind_command_socket(sock_path, 0o660)?;
    tracing::info!(path = %sock_path.display(), "tunnel-config command socket listening");
    serve_rpc(listener, MAX_REQUEST_BYTES, move |req: Vec<u8>| {
        let state = state.clone();
        async move {
            let resp = dispatch(&req, &state).await;
            serde_json::to_vec(&resp)
                .unwrap_or_else(|_| br#"{"ok":false,"error":"E_ENCODE"}"#.to_vec())
        }
    })
    .await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_is_always_planned() {
        assert_eq!(
            plan_request(br#"{"op":"status"}"#, false, false),
            Plan::Status
        );
    }

    #[test]
    fn config_request_refused_when_channel_disabled() {
        match plan_request(
            br#"{"op":"config_request","request":{"op":"get"}}"#,
            false,
            false,
        ) {
            Plan::Reject(v) => assert_eq!(v["error"], "E_TUNNEL_DISABLED"),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn read_forwards_but_write_is_gated() {
        // A read is forwarded once the channel is enabled, write gate irrelevant.
        match plan_request(
            br#"{"op":"config_request","request":{"op":"get"},"timeout_ms":1000}"#,
            true,
            false,
        ) {
            Plan::Forward { op_body, timeout } => {
                assert_eq!(op_body, br#"{"op":"get"}"#);
                assert_eq!(timeout, Duration::from_millis(1000));
            }
            other => panic!("unexpected {other:?}"),
        }
        // A write is refused while the write gate is closed…
        match plan_request(
            br#"{"op":"config_request","request":{"op":"put","key":"k","value":"v"}}"#,
            true,
            false,
        ) {
            Plan::Reject(v) => assert_eq!(v["error"], "E_WRITE_DISABLED"),
            other => panic!("unexpected {other:?}"),
        }
        // …and forwarded once it is open.
        assert!(matches!(
            plan_request(
                br#"{"op":"config_request","request":{"op":"put","key":"k","value":"v"}}"#,
                true,
                true,
            ),
            Plan::Forward { .. }
        ));
    }

    #[test]
    fn a_caller_timeout_is_clamped() {
        match plan_request(
            br#"{"op":"config_request","request":{"op":"get"},"timeout_ms":999999}"#,
            true,
            false,
        ) {
            Plan::Forward { timeout, .. } => assert_eq!(timeout, MAX_TIMEOUT),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn missing_or_unknown_is_rejected() {
        match plan_request(br#"{"op":"config_request"}"#, true, true) {
            Plan::Reject(v) => assert_eq!(v["error"], "E_MISSING_REQUEST"),
            other => panic!("unexpected {other:?}"),
        }
        match plan_request(br#"{"op":"nope"}"#, true, true) {
            Plan::Reject(v) => assert!(v["error"].as_str().unwrap().starts_with("E_UNKNOWN_OP")),
            other => panic!("unexpected {other:?}"),
        }
    }
}
