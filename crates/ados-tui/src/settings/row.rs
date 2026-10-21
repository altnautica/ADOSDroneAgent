//! Editable value rows shared by every settings page, the All settings
//! browser, and extension config panes: how a value displays, how ←/→ cycles
//! it, how typed text parses back into JSON, and which overlay edits it.

use serde_json::{json, Value};

use super::schema::Kind;
use crate::overlay::{Overlay, Ret};

/// Enums longer than this open a picker instead of cycling in place.
pub const CYCLE_MAX: usize = 8;

/// The masked form of a secret.
pub const MASK: &str = "••••••";

/// One editable row as a page describes it.
#[derive(Debug, Clone, PartialEq)]
pub struct EditRow {
    /// Stable key (config key, plugin parameter key, or a page-local id).
    pub key: String,
    pub label: String,
    pub help: String,
    pub kind: Kind,
    pub value: Option<Value>,
    pub default: Option<Value>,
    pub secret: bool,
    pub nullable: bool,
    /// Shown dim with the read-only glyph; never edited.
    pub read_only: bool,
}

impl EditRow {
    /// Whether the value differs from its default (drawn with `◆`).
    pub fn non_default(&self) -> bool {
        match (&self.value, &self.default) {
            (Some(v), Some(d)) => !same(v, d),
            _ => false,
        }
    }
}

/// JSON equality that treats `1` and `1.0` as equal.
pub fn same(a: &Value, b: &Value) -> bool {
    match (a.as_f64(), b.as_f64()) {
        (Some(x), Some(y)) => x == y,
        _ => a == b,
    }
}

/// The display text of a value. Secrets mask unless `revealed`; the agent
/// redacts secrets to `***`, which displays as "set".
pub fn display(kind: &Kind, value: Option<&Value>, secret: bool, revealed: bool) -> String {
    let Some(v) = value else { return "—".into() };
    if secret {
        return match v {
            Value::Null => "not set".into(),
            Value::String(s) if s.is_empty() => "not set".into(),
            Value::String(s) if s == "***" => "set".into(),
            Value::String(s) if revealed => s.clone(),
            _ => MASK.into(),
        };
    }
    match (kind, v) {
        (_, Value::Null) => "none".into(),
        (Kind::Bool, Value::Bool(b)) => if *b { "on" } else { "off" }.into(),
        (_, Value::String(s)) if s.is_empty() => "\"\"".into(),
        (_, Value::String(s)) => s.clone(),
        (Kind::Opaque, other) => compact(other),
        (_, other) => other.to_string(),
    }
}

/// Compact one-line JSON for an opaque value.
pub fn compact(v: &Value) -> String {
    serde_json::to_string(v).unwrap_or_default()
}

/// The next value when ←/→ is pressed, for kinds that cycle in place (bool,
/// short enums). `None` when the kind does not cycle.
pub fn cycle(kind: &Kind, current: Option<&Value>, forward: bool) -> Option<Value> {
    match kind {
        Kind::Bool => Some(json!(!current.and_then(Value::as_bool).unwrap_or(false))),
        Kind::Enum(values) if !values.is_empty() && values.len() <= CYCLE_MAX => {
            let pos = current.and_then(|c| values.iter().position(|v| same(v, c)));
            let n = values.len();
            let next = match (pos, forward) {
                (Some(i), true) => (i + 1) % n,
                (Some(i), false) => (i + n - 1) % n,
                (None, _) => 0,
            };
            Some(values[next].clone())
        }
        _ => None,
    }
}

/// Parse typed text into a JSON value of `kind`, enforcing bounds.
pub fn parse(kind: &Kind, text: &str) -> Result<Value, String> {
    let t = text.trim();
    match kind {
        Kind::Bool => match t.to_ascii_lowercase().as_str() {
            "on" | "true" | "yes" | "1" => Ok(json!(true)),
            "off" | "false" | "no" | "0" => Ok(json!(false)),
            _ => Err("Enter on or off".into()),
        },
        Kind::Int { min, max } => {
            let n: i64 = t.parse().map_err(|_| "Enter a whole number".to_string())?;
            if let Some(lo) = min {
                if n < *lo {
                    return Err(format!("Minimum is {lo}"));
                }
            }
            if let Some(hi) = max {
                if n > *hi {
                    return Err(format!("Maximum is {hi}"));
                }
            }
            Ok(json!(n))
        }
        Kind::Num { min, max } => {
            let n: f64 = t.parse().map_err(|_| "Enter a number".to_string())?;
            if !n.is_finite() {
                return Err("Enter a finite number".into());
            }
            if let Some(lo) = min {
                if n < *lo {
                    return Err(format!("Minimum is {lo}"));
                }
            }
            if let Some(hi) = max {
                if n > *hi {
                    return Err(format!("Maximum is {hi}"));
                }
            }
            Ok(json!(n))
        }
        Kind::Enum(values) => values
            .iter()
            .find(|v| match v {
                Value::String(s) => s == t,
                other => serde_json::from_str::<Value>(t).is_ok_and(|p| same(&p, other)),
            })
            .cloned()
            .ok_or_else(|| "Pick one of the listed values".into()),
        Kind::Str { .. } => Ok(json!(text)),
        Kind::Opaque => Err("List value: edit config.yaml (the config API sets one scalar key per call)".into()),
    }
}

/// The overlay that edits `row` on Enter: a picker for an enum, a text prompt
/// otherwise (masked for secrets, clearable when nullable). `None` for a
/// read-only or opaque row.
pub fn edit_overlay(row: &EditRow, ret: Ret) -> Option<Overlay> {
    if row.read_only || row.kind == Kind::Opaque {
        return None;
    }
    if let Kind::Enum(values) = &row.kind {
        let items: Vec<String> = values.iter().map(|v| display(&row.kind, Some(v), false, true)).collect();
        let sel = row
            .value
            .as_ref()
            .and_then(|c| values.iter().position(|v| same(v, c)))
            .unwrap_or(0);
        return Some(Overlay::picker(ret, row.label.clone(), items, sel));
    }
    let initial = match (&row.value, row.secret) {
        (_, true) | (None, _) | (Some(Value::Null), _) => String::new(),
        (Some(Value::String(s)), _) => s.clone(),
        (Some(Value::Bool(b)), _) => if *b { "on" } else { "off" }.into(),
        (Some(v), _) => v.to_string(),
    };
    let mut s = Overlay::input(ret, row.label.clone(), row.help.clone(), &initial);
    s.masked = row.secret;
    s.nullable = row.nullable;
    s.hint = match &row.kind {
        Kind::Int { min, max } => bounds_hint(min.map(|v| v as f64), max.map(|v| v as f64)),
        Kind::Num { min, max } => bounds_hint(*min, *max),
        Kind::Str { pattern: Some(p) } => Some(format!("Pattern {p}")),
        _ => None,
    };
    Some(Overlay::Input(s))
}

fn bounds_hint(min: Option<f64>, max: Option<f64>) -> Option<String> {
    match (min, max) {
        (Some(a), Some(b)) => Some(format!("Range {a} – {b}")),
        (Some(a), None) => Some(format!("Minimum {a}")),
        (None, Some(b)) => Some(format!("Maximum {b}")),
        (None, None) => None,
    }
}

/// The value an overlay result stands for, for a row of `kind`: a picker
/// index maps back into the enum, text parses, `Null` clears.
pub fn value_from_result(kind: &Kind, result: &crate::overlay::OverlayResult) -> Option<Result<Value, String>> {
    use crate::overlay::OverlayResult as R;
    match (kind, result) {
        (Kind::Enum(values), R::Picked(i)) => values.get(*i).cloned().map(Ok),
        (_, R::Text(t)) => Some(parse(kind, t)),
        (_, R::Null) => Some(Ok(Value::Null)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_enums_cycle_and_wrap() {
        let k = Kind::Enum(vec![json!(0), json!(90), json!(180), json!(270)]);
        assert_eq!(cycle(&k, Some(&json!(270)), true), Some(json!(0)));
        assert_eq!(cycle(&k, Some(&json!(0)), false), Some(json!(270)));
        assert_eq!(cycle(&Kind::Bool, Some(&json!(true)), true), Some(json!(false)));
        let long = Kind::Enum((0..9).map(|i| json!(i)).collect());
        assert_eq!(cycle(&long, Some(&json!(1)), true), None);
    }

    #[test]
    fn parse_enforces_bounds_and_finiteness() {
        let k = Kind::Int { min: Some(2500), max: Some(4200) };
        assert_eq!(parse(&k, "3300"), Ok(json!(3300)));
        assert!(parse(&k, "5000").is_err());
        assert!(parse(&k, "3.5").is_err());
        let n = Kind::Num { min: None, max: None };
        assert!(parse(&n, "NaN").is_err());
        assert_eq!(parse(&n, "0.25"), Ok(json!(0.25)));
    }

    #[test]
    fn secrets_never_display_their_value_unless_revealed() {
        assert_eq!(display(&Kind::Str { pattern: None }, Some(&json!("***")), true, false), "set");
        assert_eq!(display(&Kind::Str { pattern: None }, Some(&json!("")), true, false), "not set");
        assert_eq!(display(&Kind::Str { pattern: None }, Some(&json!("hunter2")), true, false), MASK);
        assert_eq!(display(&Kind::Str { pattern: None }, Some(&json!("hunter2")), true, true), "hunter2");
    }
}
