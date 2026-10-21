//! The result dialog of `POST /api/v1/setup/apply`: one line per section.
//!
//! The reply is `{overall, sections: {<name>: {ok, message, data}},
//! rolled_back: [<name>…]}`. `overall` is true only when every section sent
//! succeeded; `rolled_back` names sections that succeeded and were then
//! reverted because a later one failed.

use ados_protocol::rest::RestError;
use serde_json::Value;

use crate::app::Core;
use crate::overlay::Overlay;
use crate::toast::ToastKind;

/// The dialog lines for an apply reply: the overall outcome, then each
/// section's `ok`/`message`, then each rolled-back section.
pub fn lines(body: &Value) -> Vec<(ToastKind, String)> {
    let overall = body.get("overall").and_then(Value::as_bool).unwrap_or(false);
    let mut out = vec![if overall {
        (ToastKind::Success, "Applied".to_string())
    } else {
        (ToastKind::Error, "Not applied".to_string())
    }];
    if let Some(sections) = body.get("sections").and_then(Value::as_object) {
        for (name, section) in sections {
            let ok = section.get("ok").and_then(Value::as_bool).unwrap_or(false);
            let message = section
                .get("message")
                .and_then(Value::as_str)
                .filter(|m| !m.is_empty())
                .unwrap_or(if ok { "ok" } else { "failed" });
            let kind = if ok { ToastKind::Success } else { ToastKind::Error };
            out.push((kind, format!("{name}: {message}")));
        }
    }
    if let Some(rolled) = body.get("rolled_back").and_then(Value::as_array) {
        for name in rolled.iter().filter_map(Value::as_str) {
            out.push((ToastKind::Warn, format!("{name}: rolled back")));
        }
    }
    out
}

/// One section of an apply reply.
pub fn section<'a>(body: &'a Value, name: &str) -> Option<&'a Value> {
    body.get("sections").and_then(|s| s.get(name))
}

/// Whether the whole apply succeeded and `name` reported ok.
pub fn succeeded(body: &Value, name: &str) -> bool {
    body.get("overall").and_then(Value::as_bool) == Some(true)
        && section(body, name)
            .and_then(|s| s.get("ok"))
            .and_then(Value::as_bool)
            == Some(true)
}

/// Show a finished apply: the result dialog for an answer, an error toast
/// for a failed request. Returns the body when the agent answered.
pub fn show<'a>(core: &mut Core, title: &str, result: &'a Result<Value, RestError>) -> Option<&'a Value> {
    match result {
        Ok(body) => {
            core.open(Overlay::result(title, lines(body)));
            Some(body)
        }
        Err(e) => {
            core.toast_error(title, e);
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn an_apply_reply_renders_one_line_per_section() {
        let body = json!({
            "overall": false,
            "sections": {
                "profile": {"ok": true, "message": "Profile set to drone", "data": {}},
                "regulatory": {"ok": false, "message": "Unknown region XX", "data": {}},
                "cloud": {"ok": true, "message": ""}
            },
            "rolled_back": ["profile"]
        });
        let got = lines(&body);
        assert_eq!(got[0], (ToastKind::Error, "Not applied".to_string()));
        assert!(got.contains(&(ToastKind::Success, "profile: Profile set to drone".to_string())));
        assert!(got.contains(&(ToastKind::Error, "regulatory: Unknown region XX".to_string())));
        assert!(got.contains(&(ToastKind::Success, "cloud: ok".to_string())));
        assert_eq!(got.last(), Some(&(ToastKind::Warn, "profile: rolled back".to_string())));
        assert_eq!(got.len(), 5);
        assert!(!succeeded(&body, "profile"));
    }

    #[test]
    fn a_clean_apply_is_a_success() {
        let body = json!({
            "overall": true,
            "sections": {"regulatory": {"ok": true, "message": "Region set", "data": {"restart_required": true}}},
            "rolled_back": []
        });
        assert_eq!(
            lines(&body),
            vec![
                (ToastKind::Success, "Applied".to_string()),
                (ToastKind::Success, "regulatory: Region set".to_string()),
            ]
        );
        assert!(succeeded(&body, "regulatory"));
        assert_eq!(
            section(&body, "regulatory").and_then(|s| crate::app::at(s, "data.restart_required")),
            Some(&json!(true))
        );
    }
}
