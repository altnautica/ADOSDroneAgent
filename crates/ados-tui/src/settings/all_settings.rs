//! ALL SETTINGS: every scalar leaf of the config schema as an editable row,
//! grouped under its top-level section. Arrays and objects without scalar
//! leaves show read-only as compact JSON.

use super::config_rows::{help_of, label_of, leaf_row};
use super::row::EditRow;
use super::rows_view::{Item, RowsState};
use super::schema::{Kind, Leaf};
use crate::app::Core;

/// The context line of a list or object value.
pub const OPAQUE_HELP: &str = "List value: edit config.yaml (the config API sets one scalar key per call)";

/// The top-level sections, in schema order.
pub fn sections(leaves: &[Leaf]) -> Vec<&str> {
    let mut out: Vec<&str> = Vec::new();
    for l in leaves {
        let s = l.section();
        if !out.contains(&s) {
            out.push(s);
        }
    }
    out
}

/// The row of one leaf. The label is the dotted key below its section; the
/// help names the full key, then the curated or schema text.
pub fn row(core: &Core, leaf: &Leaf) -> EditRow {
    let label = leaf
        .key
        .split_once('.')
        .map(|(_, rest)| rest.to_string())
        .unwrap_or_else(|| leaf.key.clone());
    let help = if leaf.kind == Kind::Opaque {
        OPAQUE_HELP.to_string()
    } else {
        let text = help_of(&leaf.key, Some(leaf));
        let text = if text.is_empty() {
            label_of(&leaf.key, Some(leaf))
        } else {
            text
        };
        format!("{}: {text}", leaf.key)
    };
    leaf_row(core, leaf, label, help)
}

/// Every leaf under a header per section.
pub fn items(core: &Core, leaves: &[Leaf]) -> Vec<Item> {
    let mut out = Vec::with_capacity(leaves.len() + 32);
    let mut current = "";
    for leaf in leaves {
        if leaf.section() != current {
            current = leaf.section();
            out.push(Item::Header(current.to_string()));
        }
        out.push(Item::Row(row(core, leaf)));
    }
    out
}

/// The section of the focused row.
pub fn focused_section(items: &[Item], state: &RowsState) -> Option<String> {
    state
        .focused_row(items)
        .map(|r| r.key.split('.').next().unwrap_or(&r.key).to_string())
}

/// The focusable index of the first row of every section, in order.
fn section_starts(items: &[Item]) -> Vec<(String, usize)> {
    let mut out = Vec::new();
    let mut n = 0;
    let mut last = String::new();
    for item in items {
        match item {
            Item::Row(r) => {
                let s = r.key.split('.').next().unwrap_or(&r.key);
                if s != last {
                    last = s.to_string();
                    out.push((last.clone(), n));
                }
                n += 1;
            }
            Item::Entry { .. } => n += 1,
            _ => {}
        }
    }
    out
}

/// Focus the first row of `section`.
pub fn focus_section(items: &[Item], state: &mut RowsState, section: &str) {
    if let Some((_, i)) = section_starts(items).into_iter().find(|(s, _)| s == section) {
        state.select(items, i);
    }
}

/// Move the focus to the first row of the next (or previous) section.
pub fn jump_section(items: &[Item], state: &mut RowsState, forward: bool) {
    let starts = section_starts(items);
    let cur = state.selected;
    let target = if forward {
        starts.iter().find(|(_, i)| *i > cur).map(|(_, i)| *i)
    } else {
        // The start of the current section when inside it, else the one before.
        starts.iter().rev().find(|(_, i)| *i < cur).map(|(_, i)| *i)
    };
    if let Some(i) = target {
        state.select(items, i);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::poll::ResourceId;
    use crate::settings::schema::flatten;
    use serde_json::json;

    fn schema() -> serde_json::Value {
        json!({
            "properties": {
                "agent": {"properties": {"name": {"type": "string", "title": "Name", "default": "my-drone"}}},
                "cameras": {"type": "array", "default": []},
                "mavlink": {"properties": {
                    "baud_rate": {"type": "integer", "minimum": 0, "default": 57600},
                    "system_id": {"type": "integer", "default": 1}
                }}
            }
        })
    }

    fn app_with_schema() -> crate::app::App {
        let mut app = crate::ui::tests::test_app();
        let now = std::time::Instant::now();
        for (id, v) in [
            (ResourceId::ConfigSchema, schema()),
            (ResourceId::Config, json!({"agent": {"name": "bench"}, "cameras": [{"id": "a"}], "mavlink": {"baud_rate": 115200, "system_id": 1}})),
        ] {
            app.core.res.insert(
                id,
                crate::app::Slot { value: Some(v), error: None, code: None, error_body: None, at: now },
            );
        }
        app
    }

    #[test]
    fn leaves_group_under_section_headers() {
        let app = app_with_schema();
        let leaves = flatten(&schema());
        assert_eq!(sections(&leaves), ["agent", "cameras", "mavlink"]);
        let items = items(&app.core, &leaves);
        let headers: Vec<String> = items
            .iter()
            .filter_map(|i| match i {
                Item::Header(h) => Some(h.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(headers, ["agent", "cameras", "mavlink"]);
        let baud = items
            .iter()
            .find_map(|i| match i {
                Item::Row(r) if r.key == "mavlink.baud_rate" => Some(r.clone()),
                _ => None,
            })
            .unwrap();
        assert_eq!(baud.label, "baud_rate");
        assert_eq!(baud.value, Some(json!(115200)));
        assert!(baud.non_default());
        assert!(baud.help.starts_with("mavlink.baud_rate: Serial line speed"), "{}", baud.help);
        let cams = items
            .iter()
            .find_map(|i| match i {
                Item::Row(r) if r.key == "cameras" => Some(r.clone()),
                _ => None,
            })
            .unwrap();
        assert!(cams.read_only);
        assert_eq!(cams.help, OPAQUE_HELP);
    }

    #[test]
    fn section_jumps_land_on_each_first_row() {
        let app = app_with_schema();
        let leaves = flatten(&schema());
        let items = items(&app.core, &leaves);
        let mut st = RowsState::default();
        jump_section(&items, &mut st, true);
        assert_eq!(focused_section(&items, &st).as_deref(), Some("cameras"));
        jump_section(&items, &mut st, true);
        assert_eq!(st.focused_row(&items).unwrap().key, "mavlink.baud_rate");
        st.step(&items, true);
        jump_section(&items, &mut st, false);
        assert_eq!(st.focused_row(&items).unwrap().key, "mavlink.baud_rate");
        focus_section(&items, &mut st, "agent");
        assert_eq!(st.focused_row(&items).unwrap().key, "agent.name");
    }
}
