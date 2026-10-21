//! Shared plumbing for the bespoke link, network and capture pages:
//! config-key rows edited in place, and the operator text of the nested
//! error bodies the network and radio routes answer with. List navigation is
//! [`super::kit::nav`].

use ados_protocol::rest::RestError;
use serde_json::Value;

use crate::app::{Core, ARMED_REFUSAL};
use crate::overlay::{OverlayResult, Ret};
use crate::settings::config_write;
use crate::settings::row::{self, EditRow};
use crate::settings::rows_view::Item;
use crate::toast::ToastKind;
use crate::worker::{Purpose, Reply, Route};

/// Overlay tag for editing a config-key row.
pub const CONFIG_EDIT: &str = "config_edit";

/// The flash/row key prefix of a page: `"<page id>:"`.
pub fn prefix(page: &str) -> String {
    format!("{page}:")
}

/// ←/→ on a config-key row: cycle a bool or short enum and write it.
/// Returns true when the row cycles.
pub fn config_cycle(core: &mut Core, route: Route, page: &str, row: &EditRow, forward: bool) -> bool {
    if row.read_only {
        return false;
    }
    match row::cycle(&row.kind, row.value.as_ref(), forward) {
        Some(next) => {
            let flash = format!("{}{}", prefix(page), row.key);
            config_write::submit(core, route, &row.key, next, &flash);
            true
        }
        None => false,
    }
}

/// Enter on a config-key row: open its editor. The overlay result comes back
/// tagged [`CONFIG_EDIT`] with the key in `data`.
pub fn config_edit(core: &mut Core, route: Route, row: &EditRow) {
    let ret = Ret::new(route, CONFIG_EDIT).with(Value::String(row.key.clone()));
    if let Some(overlay) = row::edit_overlay(row, ret) {
        core.open(overlay);
    }
}

/// A [`CONFIG_EDIT`] overlay finished: parse the answer for `row` and write it.
pub fn config_result(core: &mut Core, route: Route, page: &str, row: &EditRow, result: &OverlayResult) {
    match row::value_from_result(&row.kind, result) {
        Some(Ok(value)) => {
            let flash = format!("{}{}", prefix(page), row.key);
            config_write::submit(core, route, &row.key, value, &flash);
        }
        Some(Err(why)) => core.toast(ToastKind::Error, format!("{}: {why}", row.label)),
        None => {}
    }
}

/// The focused config row whose key is `key`, among `items`.
pub fn find_row<'a>(items: &'a [Item], key: &str) -> Option<&'a EditRow> {
    items.iter().find_map(|i| match i {
        Item::Row(r) if r.key == key => Some(r),
        _ => None,
    })
}

/// A config write reply: toast, flash and re-read through `config_write`.
/// True when `reply` was a config write.
pub fn config_reply(core: &mut Core, items: &[Item], reply: &Reply) -> bool {
    let Purpose::ConfigSet { key, row } = &reply.purpose else {
        return false;
    };
    let label = find_row(items, key)
        .map(|r| r.label.clone())
        .unwrap_or_else(|| key.clone());
    config_write::on_reply(core, &label, row, &reply.result);
    true
}

/// The operator text of a failed request, reading the nested error bodies the
/// network, radio and sensor routes use:
/// `{"detail":{"error":{"code","message"}}}`, `{"detail":{"error","message"}}`,
/// `{"detail":"…"}`, `{"error","detail"}` and the armed refusal.
pub fn detail_text(err: &RestError) -> String {
    let Some(body) = err.body() else {
        return crate::app::error_text(err);
    };
    if body.get("error").and_then(Value::as_str) == Some("E_ARMED") {
        return ARMED_REFUSAL.to_string();
    }
    if let Some(text) = body_text(body) {
        return text;
    }
    crate::app::error_text(err)
}

fn body_text(body: &Value) -> Option<String> {
    let str_of = |v: Option<&Value>| v.and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_string);
    match body.get("detail") {
        Some(Value::String(s)) if !s.is_empty() => return Some(s.clone()),
        Some(detail @ Value::Object(_)) => {
            if let Some(inner) = detail.get("error").filter(|e| e.is_object()) {
                if let Some(m) = str_of(inner.get("message")) {
                    return Some(m);
                }
                if let Some(c) = str_of(inner.get("code")) {
                    return Some(c);
                }
            }
            if let Some(m) = str_of(detail.get("message")) {
                return Some(m);
            }
            if let Some(code) = str_of(detail.get("error")) {
                let bound = ["min", "max"]
                    .iter()
                    .find_map(|k| detail.get(*k).map(|v| format!(" ({k} {v})")))
                    .unwrap_or_default();
                return Some(format!("{}{bound}", code.replace('_', " ")));
            }
        }
        _ => {}
    }
    str_of(body.get("message")).or_else(|| str_of(body.get("error")))
}

/// The machine code of a failed request: `detail.error.code`, a string
/// `detail.error`, or a top-level `error`.
pub fn error_code(err: &RestError) -> Option<String> {
    let body = err.body()?;
    let detail = body.get("detail");
    detail
        .and_then(|d| d.get("error"))
        .and_then(|e| e.get("code"))
        .and_then(Value::as_str)
        .or_else(|| detail.and_then(|d| d.get("error")).and_then(Value::as_str))
        .or_else(|| body.get("error").and_then(Value::as_str))
        .map(str::to_string)
}

/// A string or number field as display text; `—` when absent or null.
pub fn text_of(v: Option<&Value>) -> String {
    match v {
        Some(Value::String(s)) if !s.is_empty() => s.clone(),
        Some(Value::Number(n)) => n.to_string(),
        Some(Value::Bool(b)) => if *b { "yes" } else { "no" }.to_string(),
        _ => "—".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn status(code: u16, body: Value) -> RestError {
        RestError::Status { code, body: Some(body) }
    }

    #[test]
    fn nested_error_bodies_read_as_operator_text() {
        let e = status(503, json!({"detail":{"error":{"code":"E_SCAN_UNAVAILABLE","message":"Wi-Fi command socket unavailable"}}}));
        assert_eq!(detail_text(&e), "Wi-Fi command socket unavailable");
        assert_eq!(error_code(&e).as_deref(), Some("E_SCAN_UNAVAILABLE"));
        let e = status(400, json!({"detail":"Invalid channel 7. Valid channels: [36, 40]"}));
        assert_eq!(detail_text(&e), "Invalid channel 7. Valid channels: [36, 40]");
        let e = status(400, json!({"detail":{"error":"above_ceiling","max":15}}));
        assert_eq!(detail_text(&e), "above ceiling (max 15)");
        let e = status(409, json!({"error":"E_ARMED","message":"Vehicle is armed"}));
        assert_eq!(detail_text(&e), ARMED_REFUSAL);
        assert_eq!(error_code(&e).as_deref(), Some("E_ARMED"));
        let e = status(400, json!({"error":"E_RIG_INVALID","detail":"missing sensors"}));
        assert_eq!(detail_text(&e), "missing sensors");
        assert_eq!(error_code(&e).as_deref(), Some("E_RIG_INVALID"));
    }
}
