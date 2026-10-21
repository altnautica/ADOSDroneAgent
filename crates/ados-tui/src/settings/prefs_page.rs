//! THIS TERMINAL › Appearance and Behaviour: local preferences shown as
//! settings rows. A change applies live and is saved to `tui.json`; no agent
//! call is made.

use serde_json::{json, Value};

use super::registry::PrefsPage;
use super::row::EditRow;
use super::schema::Kind;
use crate::prefs::{Prefs, DEFAULT_REFRESH_MS, REFRESH_CHOICES};

fn strs(values: &[&str]) -> Kind {
    Kind::Enum(values.iter().map(|v| json!(v)).collect())
}

/// One preference row's key and text.
struct Spec {
    key: &'static str,
    label: &'static str,
    help: &'static str,
}

const APPEARANCE: &[Spec] = &[
    Spec { key: "theme", label: "Theme", help: "Theme used for the whole terminal UI" },
    Spec { key: "color_blind", label: "Color-blind mode", help: "Draw success states in blue instead of green" },
    Spec { key: "symbols", label: "Symbols", help: "Glyph set: unicode, Nerd Font icons (needs a Nerd Font), or plain ASCII" },
    Spec { key: "motion", label: "Motion", help: "Animation level; auto is full locally and reduced over SSH" },
];

const BEHAVIOUR: &[Spec] = &[
    Spec { key: "refresh_ms", label: "Refresh interval (ms)", help: "How often the agent status is polled" },
    Spec { key: "mouse", label: "Mouse", help: "Capture the mouse for clicks and scrolling; capture blocks terminal text selection" },
    Spec { key: "start_tab", label: "Start tab", help: "The tab shown at launch: Overview, or the tab in use when the terminal UI last closed" },
];

fn specs(page: PrefsPage) -> &'static [Spec] {
    match page {
        PrefsPage::Appearance => APPEARANCE,
        PrefsPage::Behaviour => BEHAVIOUR,
    }
}

/// The kind, current value and default of a preference.
fn describe(prefs: &Prefs, key: &str) -> Option<(Kind, Value, Value)> {
    let d = Prefs::default();
    Some(match key {
        "theme" => (strs(&["amber", "brand-dark", "brand-light", "nvg"]), json!(prefs.theme), json!(d.theme)),
        "color_blind" => (Kind::Bool, json!(prefs.color_blind), json!(d.color_blind)),
        "symbols" => (strs(&["unicode", "nerd", "ascii"]), json!(prefs.symbols), json!(d.symbols)),
        "motion" => (strs(&["auto", "full", "reduced", "off"]), json!(prefs.motion), json!(d.motion)),
        "refresh_ms" => (
            Kind::Enum(REFRESH_CHOICES.iter().map(|v| json!(v)).collect()),
            json!(prefs.refresh_ms),
            json!(DEFAULT_REFRESH_MS),
        ),
        "mouse" => (Kind::Bool, json!(prefs.mouse), json!(d.mouse)),
        "start_tab" => (strs(&["overview", "last"]), json!(prefs.start_tab), json!(d.start_tab)),
        _ => return None,
    })
}

/// The rows of a preferences page.
pub fn rows(page: PrefsPage, prefs: &Prefs) -> Vec<EditRow> {
    specs(page)
        .iter()
        .filter_map(|s| {
            let (kind, value, default) = describe(prefs, s.key)?;
            Some(EditRow {
                key: s.key.to_string(),
                label: s.label.to_string(),
                help: s.help.to_string(),
                kind,
                value: Some(value),
                default: Some(default),
                secret: false,
                nullable: false,
                read_only: false,
            })
        })
        .collect()
}

/// Set preference `key` to `value`. False when the key is unknown or the
/// value is not one the preference accepts.
pub fn set(prefs: &mut Prefs, key: &str, value: &Value) -> bool {
    let Some((kind, _, _)) = describe(prefs, key) else { return false };
    if let Kind::Enum(values) = &kind {
        if !values.iter().any(|v| super::row::same(v, value)) {
            return false;
        }
    }
    match (key, value) {
        ("theme", Value::String(s)) => prefs.theme = s.clone(),
        ("symbols", Value::String(s)) => prefs.symbols = s.clone(),
        ("motion", Value::String(s)) => prefs.motion = s.clone(),
        ("start_tab", Value::String(s)) => prefs.start_tab = s.clone(),
        ("color_blind", Value::Bool(b)) => prefs.color_blind = *b,
        ("mouse", Value::Bool(b)) => prefs.mouse = *b,
        ("refresh_ms", v) => match v.as_u64() {
            Some(ms) => prefs.refresh_ms = ms,
            None => return false,
        },
        _ => return false,
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::prefs::PrefsStore;

    #[test]
    fn rows_show_current_values_and_defaults() {
        let p = Prefs {
            theme: "nvg".into(),
            ..Prefs::default()
        };
        let rows = rows(PrefsPage::Appearance, &p);
        let theme = rows.iter().find(|r| r.key == "theme").unwrap();
        assert_eq!(theme.value, Some(json!("nvg")));
        assert!(theme.non_default());
        let cb = rows.iter().find(|r| r.key == "color_blind").unwrap();
        assert_eq!(cb.kind, Kind::Bool);
        assert!(!cb.non_default());
        let behaviour = super::rows(PrefsPage::Behaviour, &p);
        let keys: Vec<&str> = behaviour.iter().map(|r| r.key.as_str()).collect();
        assert_eq!(keys, ["refresh_ms", "mouse", "start_tab"]);
    }

    #[test]
    fn set_rejects_values_outside_the_choices() {
        let mut p = Prefs::default();
        assert!(!set(&mut p, "theme", &json!("neon")));
        assert!(!set(&mut p, "refresh_ms", &json!(1500)));
        assert!(!set(&mut p, "nope", &json!(true)));
        assert!(set(&mut p, "refresh_ms", &json!(5000)));
        assert_eq!(p.refresh_ms, 5000);
        assert!(set(&mut p, "mouse", &json!(true)));
        assert!(p.mouse);
    }

    #[test]
    fn a_set_preference_survives_a_save_and_reload() {
        let dir = std::env::temp_dir().join(format!("ados-tui-prefs-page-{}", std::process::id()));
        let path = dir.join("ados").join("tui.json");
        let _ = std::fs::remove_dir_all(&dir);
        let mut store = PrefsStore::load(Some(path.clone()));
        assert!(set(&mut store.prefs, "theme", &json!("brand-light")));
        assert!(set(&mut store.prefs, "symbols", &json!("ascii")));
        store.save().unwrap();
        let back = PrefsStore::load(Some(path));
        assert_eq!(back.prefs.theme, "brand-light");
        assert_eq!(back.prefs.symbols, "ascii");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
