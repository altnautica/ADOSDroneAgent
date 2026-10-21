//! Small helpers the bespoke settings pages share: list navigation, row
//! builders, the setup-status subscription, and UTC timestamps.

use serde_json::Value;

use crate::app::{Core, Hit};
use crate::keymap::Command;
use crate::poll::{ResourceId, Sub};
use crate::settings::row::EditRow;
use crate::settings::rows_view::{self, Item, RowsState};
use crate::settings::schema::Kind;

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unix_times_format_as_utc_dates() {
        assert_eq!(utc(0), "1970-01-01 00:00 UTC");
        assert_eq!(utc(951_782_400), "2000-02-29 00:00 UTC");
        assert_eq!(utc_ms(1_700_000_000_000), "2023-11-14 22:13 UTC");
    }
}
