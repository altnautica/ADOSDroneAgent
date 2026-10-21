//! Small helpers the bespoke settings pages share: list navigation, row
//! builders, config-key rows edited in place, the operator text of the nested
//! error bodies the network and radio routes answer with, the setup-status
//! subscription, and UTC timestamps.

use ados_protocol::rest::RestError;
use serde_json::Value;

use crate::app::{Core, Hit, ARMED_REFUSAL};
use crate::keymap::Command;
use crate::overlay::{OverlayResult, Ret};
use crate::poll::{ResourceId, Sub};
use crate::settings::config_write;
use crate::settings::row::{self, EditRow};
use crate::settings::rows_view::{self, Item, RowsState};
use crate::settings::schema::Kind;
use crate::toast::ToastKind;
use crate::worker::{Purpose, Reply, Route};

/// How often a page re-reads the setup status while focused.
pub const SETUP_MS: u64 = 5000;

/// The setup-status subscription.
pub fn setup_sub() -> Sub {
    Sub::every_ms(ResourceId::SetupStatus, "/api/v1/setup/status", SETUP_MS)
}

/// The last setup-status body.
pub fn setup(core: &Core) -> Option<&Value> {
    core.value(&ResourceId::SetupStatus)
}

/// A string at a dotted path, empty strings treated as absent.
pub fn text<'a>(v: Option<&'a Value>, path: &str) -> Option<&'a str> {
    v.and_then(|v| crate::app::at(v, path))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
}

/// Move the focus for a navigation command. True when `cmd` was navigation.
pub fn nav(state: &mut RowsState, items: &[Item], cmd: Command) -> bool {
    match cmd {
        Command::Up => state.step(items, false),
        Command::Down => state.step(items, true),
        Command::PageUp => state.jump(items, -10),
        Command::PageDown => state.jump(items, 10),
        Command::Top => state.jump(items, -(items.len() as isize)),
        Command::Bottom => state.jump(items, items.len() as isize),
        _ => return false,
    }
    true
}

/// Focus the clicked row.
pub fn click(state: &mut RowsState, items: &[Item], hit: &Hit) {
    if let Hit::Row(i) = hit {
        state.select(items, *i);
    }
}

/// An editable row with no default.
pub fn row(key: &str, label: &str, help: &str, kind: Kind, value: Option<Value>) -> EditRow {
    EditRow {
        key: key.to_string(),
        label: label.to_string(),
        help: help.to_string(),
        kind,
        value,
        default: None,
        secret: false,
        nullable: false,
        read_only: false,
    }
}

/// A string enum kind.
pub fn enum_of(values: &[&str]) -> Kind {
    Kind::Enum(values.iter().map(|v| Value::String((*v).to_string())).collect())
}

/// A non-value focusable line (an action such as "Apply").
pub fn action(core: &Core, key: &str, label: &str, detail: &str) -> Item {
    use ratatui::text::{Line, Span};
    let t = &core.theme;
    let mut spans = vec![Span::styled(format!("[ {label} ]"), t.accented())];
    if !detail.is_empty() {
        spans.push(Span::styled(format!("  {detail}"), t.dim()));
    }
    Item::Entry {
        key: key.to_string(),
        line: Line::from(spans),
    }
}

/// A loading or failure note for a resource the page depends on, or `None`
/// once it has a value.
pub fn resource_note(core: &Core, id: &ResourceId, what: &str) -> Option<Item> {
    match core.slot(id) {
        Some(s) if s.value.is_some() => None,
        Some(s) if s.error.is_some() => Some(rows_view::note(
            core,
            format!("Could not read {what}: {}", s.error.as_deref().unwrap_or("")),
            true,
        )),
        _ => Some(rows_view::note(core, format!("Reading {what}…"), false)),
    }
}

/// `YYYY-MM-DD HH:MM UTC` from Unix seconds.
pub fn utc(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (y, m, d) = civil(days);
    format!("{y:04}-{m:02}-{d:02} {:02}:{:02} UTC", rem / 3600, (rem % 3600) / 60)
}

/// `utc` from Unix milliseconds.
pub fn utc_ms(ms: i64) -> String {
    utc(ms.div_euclid(1000))
}

/// Days since 1970-01-01 to a civil (proleptic Gregorian) date.
fn civil(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

/// Overlay tag for editing a row through [`open_editor`].
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

/// Open the editor overlay of a non-toggle row. The result comes back tagged
/// [`CONFIG_EDIT`] with the key in `data`. False when the row has no editor:
/// a read-only or opaque row, or a toggle (which Enter flips in place).
pub fn open_editor(core: &mut Core, route: Route, row: &EditRow) -> bool {
    if row.kind == Kind::Bool {
        return false;
    }
    let ret = Ret::new(route, CONFIG_EDIT).with(Value::String(row.key.clone()));
    match row::edit_overlay(row, ret) {
        Some(overlay) => {
            core.open(overlay);
            true
        }
        None => false,
    }
}

/// Enter on a config-key row: a toggle flips and writes like ←/→; any other
/// row opens its editor.
pub fn config_edit(core: &mut Core, route: Route, page: &str, row: &EditRow) {
    if row.kind == Kind::Bool {
        config_cycle(core, route, page, row, true);
    } else {
        open_editor(core, route, row);
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

/// The row whose key is `key`, among `items`.
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

/// Store `value` as the latest read of `id` (page tests).
#[cfg(test)]
pub fn put(core: &mut Core, id: ResourceId, value: Value) {
    core.res.insert(
        id,
        crate::app::Slot { value: Some(value), error: None, code: None, error_body: None, at: core.now },
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn unix_times_format_as_utc_dates() {
        assert_eq!(utc(0), "1970-01-01 00:00 UTC");
        assert_eq!(utc(951_782_400), "2000-02-29 00:00 UTC");
        assert_eq!(utc_ms(1_700_000_000_000), "2023-11-14 22:13 UTC");
    }

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
