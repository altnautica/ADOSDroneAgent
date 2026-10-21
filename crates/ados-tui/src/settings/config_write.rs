//! Writing one config key (`PUT /api/config {key, value}`) and classifying the
//! reply into what the operator is told.

use ados_protocol::rest::RestError;
use serde_json::{json, Value};

use crate::app::{Core, ARMED_REFUSAL};
use crate::poll::ResourceId;
use crate::toast::ToastKind;
use crate::worker::{Purpose, Request, Route};

/// What a config write came to.
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    /// Saved and persisted.
    Saved,
    /// Applied in memory but the file write failed.
    NotPersisted(String),
    /// Refused; the text says why. The shown value reverts on the next read.
    Failed(String),
}

impl Outcome {
    pub fn ok(&self) -> bool {
        matches!(self, Outcome::Saved | Outcome::NotPersisted(_))
    }
}

/// Queue a write of `key = value`. `row` names the rendered row to flash.
pub fn submit(core: &mut Core, route: Route, key: &str, value: Value, row: &str) {
    core.submit(
        Request::new(
            "PUT",
            "/api/config",
            route,
            Purpose::ConfigSet {
                key: key.to_string(),
                row: row.to_string(),
            },
        )
        .body(json!({ "key": key, "value": value })),
    );
}

fn str_at<'a>(v: &'a Value, path: &[&str]) -> Option<&'a str> {
    path.iter().try_fold(v, |acc, k| acc.get(*k))?.as_str()
}

/// Classify a `PUT /api/config` reply.
pub fn classify(result: &Result<Value, RestError>) -> Outcome {
    match result {
        Ok(body) => {
            if let Some(err) = body.get("error").and_then(Value::as_str) {
                return Outcome::Failed(err.to_string());
            }
            if body.get("persisted").and_then(Value::as_bool) == Some(false) {
                let why = str_at(body, &["persist_error"]).unwrap_or("unknown error");
                return Outcome::NotPersisted(why.to_string());
            }
            Outcome::Saved
        }
        Err(e) => {
            let body = e.body().cloned().unwrap_or(Value::Null);
            match e.code() {
                Some(409) => {
                    let armed = str_at(&body, &["error"]) == Some("E_ARMED")
                        || str_at(&body, &["detail", "error"]) == Some("E_ARMED")
                        || str_at(&body, &["detail", "code"]) == Some("E_ARMED");
                    if armed {
                        Outcome::Failed(ARMED_REFUSAL.to_string())
                    } else {
                        Outcome::Failed(crate::app::error_text(e))
                    }
                }
                Some(500) if body.get("persisted").and_then(Value::as_bool) == Some(false) => {
                    let why = str_at(&body, &["persist_error"]).unwrap_or("unknown error");
                    Outcome::NotPersisted(why.to_string())
                }
                Some(400)
                    if str_at(&body, &["detail", "error", "code"]) == Some("E_REDACTED_SENTINEL")
                        || str_at(&body, &["code"]) == Some("E_REDACTED_SENTINEL") =>
                {
                    Outcome::Failed("Enter a new secret value".into())
                }
                Some(422) => {
                    let msg = str_at(&body, &["message"])
                        .or_else(|| str_at(&body, &["detail", "message"]))
                        .or_else(|| str_at(&body, &["detail"]))
                        .map(str::to_string)
                        .unwrap_or_else(|| crate::app::error_text(e));
                    match str_at(&body, &["field"]) {
                        Some(field) => Outcome::Failed(format!("{msg} ({field})")),
                        None => Outcome::Failed(msg),
                    }
                }
                _ => Outcome::Failed(crate::app::error_text(e)),
            }
        }
    }
}

/// Handle a finished config write: toast, flash the row, re-read the config.
/// Returns the outcome for pages that react further.
pub fn on_reply(core: &mut Core, label: &str, row: &str, result: &Result<Value, RestError>) -> Outcome {
    let outcome = classify(result);
    match &outcome {
        Outcome::Saved => core.toast(ToastKind::Success, format!("{label} saved")),
        Outcome::NotPersisted(why) => core.toast(
            ToastKind::Warn,
            format!("Applied but not persisted: {why}"),
        ),
        Outcome::Failed(why) => core.toast(ToastKind::Error, format!("{label}: {why}")),
    }
    core.flash(row, outcome.ok());
    core.fetch_now(ResourceId::Config);
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(code: u16, body: Value) -> Result<Value, RestError> {
        Err(RestError::Status {
            code,
            body: Some(body),
        })
    }

    #[test]
    fn replies_classify_into_what_the_operator_is_told() {
        assert_eq!(
            classify(&Ok(json!({"status": "ok", "key": "a", "value": 1, "persisted": true}))),
            Outcome::Saved
        );
        assert_eq!(
            classify(&Ok(json!({"error": "Key not found: a.b"}))),
            Outcome::Failed("Key not found: a.b".into())
        );
        assert_eq!(
            classify(&Ok(json!({"status": "ok", "persisted": false, "persist_error": "EROFS"}))),
            Outcome::NotPersisted("EROFS".into())
        );
        assert_eq!(
            classify(&status(500, json!({"status": "error", "persisted": false, "persist_error": "disk full"}))),
            Outcome::NotPersisted("disk full".into())
        );
        assert_eq!(
            classify(&status(409, json!({"error": "E_ARMED", "message": "armed"}))),
            Outcome::Failed(ARMED_REFUSAL.into())
        );
        assert_eq!(
            classify(&status(422, json!({"error": "E_INVALID", "field": "mavlink.baud_rate", "message": "bad baud"}))),
            Outcome::Failed("bad baud (mavlink.baud_rate)".into())
        );
        assert_eq!(
            classify(&status(400, json!({"detail": {"error": {"code": "E_REDACTED_SENTINEL"}}}))),
            Outcome::Failed("Enter a new secret value".into())
        );
    }
}
