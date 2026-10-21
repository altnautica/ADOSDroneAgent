//! A plugin's declared config parameters (`gcs.contributes.parameters[]` and
//! `gcs.contributes.settings[].parameters[]` of the detail route's manifest)
//! turned into settings rows: kind from `schema`, label/help/group/order and
//! the `visible_if` reveal from `ui`, values from the live plugin config.

use serde_json::{Map, Value};

use crate::settings::row::{same, EditRow};
use crate::settings::rows_view::Item;
use crate::settings::schema::{title_case, Kind};

/// One declared parameter.
#[derive(Debug, Clone, PartialEq)]
pub struct ParamDef {
    pub key: String,
    pub kind: Kind,
    pub default: Option<Value>,
    pub label: Option<String>,
    pub help: Option<String>,
    pub group: Option<String>,
    pub order: Option<f64>,
    /// Shown only while the other key's value equals this one.
    pub visible_if: Option<(String, Value)>,
    /// Written through the plugin config route. Parameters bound elsewhere
    /// (the vision detector, an agent key) or drawn as model pickers are
    /// shown read-only.
    pub writable: bool,
    /// The `settings[]` section title the parameter sits in.
    pub section: Option<String>,
}

/// The edit kind a parameter schema describes.
pub fn schema_kind(schema: &Value) -> Kind {
    if let Some(values) = schema.get("enum").and_then(Value::as_array) {
        if !values.is_empty() {
            return Kind::Enum(values.clone());
        }
    }
    let num = |k: &str| schema.get(k).and_then(Value::as_f64);
    match schema.get("type").and_then(Value::as_str) {
        Some("boolean") => Kind::Bool,
        Some("integer") => Kind::Int {
            min: num("minimum").map(|v| v.ceil() as i64),
            max: num("maximum").map(|v| v.floor() as i64),
        },
        Some("number") => Kind::Num {
            min: num("minimum"),
            max: num("maximum"),
        },
        Some("string") => Kind::Str {
            pattern: schema.get("pattern").and_then(Value::as_str).map(str::to_string),
        },
        _ => Kind::Opaque,
    }
}

fn parse_one(raw: &Value, section: Option<&str>) -> Option<ParamDef> {
    let key = raw.get("key").and_then(Value::as_str)?.to_string();
    let schema = raw.get("schema").filter(|s| s.is_object())?;
    let ui = raw.get("ui");
    let ui_str = |k: &str| ui.and_then(|u| u.get(k)).and_then(Value::as_str).map(str::to_string);
    let visible_if = ui.and_then(|u| u.get("visible_if")).and_then(|v| {
        let k = v.get("key").and_then(Value::as_str)?;
        let eq = v.get("equals").filter(|e| e.is_string() || e.is_number() || e.is_boolean())?;
        Some((k.to_string(), eq.clone()))
    });
    let binding = raw.get("binding").and_then(Value::as_str).unwrap_or("plugin.config");
    let widget = ui_str("widget");
    let kind = schema_kind(schema);
    let writable = binding == "plugin.config"
        && !matches!(widget.as_deref(), Some("model" | "model_upload"))
        && kind != Kind::Opaque;
    Some(ParamDef {
        key,
        kind,
        default: schema.get("default").cloned(),
        label: ui_str("label"),
        help: ui_str("help"),
        group: ui_str("group"),
        order: ui.and_then(|u| u.get("order")).and_then(Value::as_f64),
        visible_if,
        writable,
        section: section.map(str::to_string),
    })
}

/// Every parameter a plugin detail body declares: the top-level list first,
/// then each settings section (by `order`, then declaration order).
pub fn defs(detail: &Value) -> Vec<ParamDef> {
    let Some(contrib) = crate::app::at(detail, "manifest.gcs.contributes") else {
        return Vec::new();
    };
    let mut out: Vec<ParamDef> = contrib
        .get("parameters")
        .and_then(Value::as_array)
        .map(|l| l.iter().filter_map(|p| parse_one(p, None)).collect())
        .unwrap_or_default();
    let mut sections: Vec<&Value> = contrib
        .get("settings")
        .and_then(Value::as_array)
        .map(|l| l.iter().collect())
        .unwrap_or_default();
    let ord = |s: &Value| s.get("order").and_then(Value::as_f64).unwrap_or(f64::MAX);
    sections.sort_by(|a, b| ord(a).total_cmp(&ord(b)));
    for s in sections {
        let title = s
            .get("title")
            .and_then(Value::as_str)
            .or_else(|| s.get("id").and_then(Value::as_str));
        if let Some(list) = s.get("parameters").and_then(Value::as_array) {
            out.extend(list.iter().filter_map(|p| parse_one(p, title)));
        }
    }
    // A key declared twice keeps its first declaration.
    let mut seen: Vec<String> = Vec::new();
    out.retain(|d| {
        if seen.contains(&d.key) {
            false
        } else {
            seen.push(d.key.clone());
            true
        }
    });
    out
}

/// The plugin's string for `text` (a locale key), else `text` itself.
pub fn localize(locale: Option<&Map<String, Value>>, text: &str) -> String {
    locale
        .and_then(|m| m.get(text))
        .and_then(Value::as_str)
        .unwrap_or(text)
        .to_string()
}

/// Whether `def` shows, given the other parameters' current values. The
/// comparison is type-strict: the number `1` never equals the string `"1"`.
pub fn visible(def: &ParamDef, all: &[ParamDef], values: &Map<String, Value>) -> bool {
    let Some((key, equals)) = &def.visible_if else { return true };
    let current = values
        .get(key)
        .or_else(|| all.iter().find(|d| &d.key == key).and_then(|d| d.default.as_ref()));
    match current {
        Some(v) if v.is_number() && equals.is_number() => same(v, equals),
        Some(v) => v == equals,
        None => false,
    }
}

/// The settings rows: visible parameters grouped under headers (the section
/// title and the `ui.group`, in first-appearance order), ordered by
/// `ui.order` inside a group.
pub fn items(defs: &[ParamDef], values: &Map<String, Value>, locale: Option<&Map<String, Value>>) -> Vec<Item> {
    let heading = |d: &ParamDef| -> Option<String> {
        let sec = d.section.as_deref().map(|s| localize(locale, s));
        let grp = d.group.as_deref().map(|g| localize(locale, g));
        match (sec, grp) {
            (Some(s), Some(g)) => Some(format!("{s} · {g}")),
            (s, g) => s.or(g),
        }
    };
    let mut groups: Vec<(Option<String>, Vec<&ParamDef>)> = Vec::new();
    for d in defs.iter().filter(|d| visible(d, defs, values)) {
        let h = heading(d);
        match groups.iter_mut().find(|(k, _)| *k == h) {
            Some((_, list)) => list.push(d),
            None => groups.push((h, vec![d])),
        }
    }
    let mut out = Vec::new();
    for (h, mut list) in groups {
        list.sort_by(|a, b| a.order.unwrap_or(f64::MAX).total_cmp(&b.order.unwrap_or(f64::MAX)));
        if let Some(h) = h {
            out.push(Item::Header(h));
        }
        for d in list {
            out.push(Item::Row(row(d, values, locale)));
        }
    }
    out
}

/// The editable row of one parameter.
pub fn row(def: &ParamDef, values: &Map<String, Value>, locale: Option<&Map<String, Value>>) -> EditRow {
    let label = def
        .label
        .as_deref()
        .map(|l| localize(locale, l))
        .unwrap_or_else(|| title_case(&def.key));
    let mut help = def.help.as_deref().map(|h| localize(locale, h)).unwrap_or_default();
    let live = values.get(&def.key).cloned();
    if live.is_none() && def.default.is_some() {
        if !help.is_empty() {
            help.push_str(" · ");
        }
        help.push_str("Default shown: not read from the drone");
    }
    if !def.writable {
        if !help.is_empty() {
            help.push_str(" · ");
        }
        help.push_str("Set from Mission Control: this control does not write the plugin config");
    }
    EditRow {
        key: def.key.clone(),
        label,
        help,
        kind: def.kind.clone(),
        value: live.or_else(|| def.default.clone()),
        default: def.default.clone(),
        secret: false,
        nullable: false,
        read_only: !def.writable,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn detail() -> Value {
        json!({"manifest": {"gcs": {"contributes": {
            "parameters": [
                {"key": "mode", "schema": {"type": "string", "enum": ["auto", "manual"], "default": "auto"},
                 "ui": {"label": "p.mode", "group": "Tracking", "order": 2}},
                {"key": "gain", "schema": {"type": "number", "minimum": 0.5, "maximum": 4, "default": 1},
                 "ui": {"label": "Gain", "group": "Tracking", "order": 1, "help": "Loop gain",
                        "visible_if": {"key": "mode", "equals": "manual"}}},
                {"key": "detector", "schema": {"type": "string"}, "binding": "engine.detector",
                 "ui": {"widget": "model"}}
            ],
            "settings": [
                {"id": "follow", "title": "Follow", "parameters": [
                    {"key": "distance_m", "schema": {"type": "integer", "minimum": 3, "maximum": 30, "default": 8}},
                    {"key": "active", "schema": {"type": "boolean", "default": false}}
                ]}
            ]
        }}}})
    }

    fn rows(items: &[Item]) -> Vec<&EditRow> {
        items
            .iter()
            .filter_map(|i| match i {
                Item::Row(r) => Some(r),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn manifest_parameters_parse_with_kinds_and_defaults() {
        let d = defs(&detail());
        assert_eq!(d.len(), 5);
        assert_eq!(d[0].kind, Kind::Enum(vec![json!("auto"), json!("manual")]));
        assert_eq!(d[1].kind, Kind::Num { min: Some(0.5), max: Some(4.0) });
        assert!(!d[2].writable, "a detector-bound model picker is read-only");
        assert_eq!(d[3].kind, Kind::Int { min: Some(3), max: Some(30) });
        assert_eq!(d[3].default, Some(json!(8)));
        assert_eq!(d[3].section.as_deref(), Some("Follow"));
        assert_eq!(d[4].kind, Kind::Bool);
    }

    #[test]
    fn visible_if_hides_and_shows() {
        let d = defs(&detail());
        let auto = Map::new();
        let keys: Vec<String> = rows(&items(&d, &auto, None)).iter().map(|r| r.key.clone()).collect();
        assert!(!keys.contains(&"gain".to_string()), "default mode auto hides gain");
        let mut manual = Map::new();
        manual.insert("mode".into(), json!("manual"));
        let shown = items(&d, &manual, None);
        let r = rows(&shown);
        // Ordered by ui.order inside the Tracking group: gain (1) before mode (2).
        assert_eq!(r[0].key, "gain");
        assert_eq!(r[1].key, "mode");
        // Type-strict: a numeric `equals` never matches the string.
        let mut strict = d.clone();
        strict[1].visible_if = Some(("mode".into(), json!(1)));
        let mut one = Map::new();
        one.insert("mode".into(), json!("1"));
        assert!(!visible(&strict[1], &strict, &one));
    }

    #[test]
    fn rows_carry_values_defaults_labels_and_headers() {
        let d = defs(&detail());
        let mut values = Map::new();
        values.insert("distance_m".into(), json!(12));
        let mut locale = Map::new();
        locale.insert("p.mode".into(), json!("Track mode"));
        let it = items(&d, &values, Some(&locale));
        assert!(matches!(&it[0], Item::Header(h) if h == "Tracking"));
        assert!(it.iter().any(|i| matches!(i, Item::Header(h) if h == "Follow")));
        let r = rows(&it);
        let mode = r.iter().find(|r| r.key == "mode").unwrap();
        assert_eq!(mode.label, "Track mode");
        assert_eq!(mode.value, Some(json!("auto")));
        assert!(mode.help.contains("not read from the drone"));
        let dist = r.iter().find(|r| r.key == "distance_m").unwrap();
        assert_eq!(dist.label, "Distance m");
        assert_eq!(dist.value, Some(json!(12)));
        assert!(dist.non_default());
        let det = r.iter().find(|r| r.key == "detector").unwrap();
        assert!(det.read_only);
    }
}
