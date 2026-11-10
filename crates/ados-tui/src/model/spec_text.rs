//! Text from the cockpit spec's string table
//! (`web-shared/src/cockpit-spec/setup-strings.json`), compiled into the
//! binary so the terminal reads the same words as every other cockpit surface:
//! the link-loss line, the AGL source marks, the alert texts, the control
//! mode, profile and avoidance labels. `{arg}` placeholders fill as the shared
//! `spec-text.ts` fills them.

use std::collections::BTreeMap;
use std::sync::LazyLock;

use serde::Deserialize;

/// The spec file, byte for byte.
pub const SETUP_STRINGS_JSON: &str =
    include_str!("../../../../web-shared/src/cockpit-spec/setup-strings.json");

#[derive(Deserialize)]
struct Table {
    strings: BTreeMap<String, String>,
}

static STRINGS: LazyLock<BTreeMap<String, String>> = LazyLock::new(|| {
    serde_json::from_str::<Table>(SETUP_STRINGS_JSON)
        .expect("setup-strings.json is the spec string table")
        .strings
});

/// Arg names whose values are keys of their own.
const ARG_KEYS: [(&str, &str); 2] = [
    ("source", "pilot.banner.source."),
    ("action", "pilot.link_loss_action."),
];

/// Whether the spec carries `key`.
pub fn has_spec_text(key: &str) -> bool {
    STRINGS.contains_key(key)
}

/// A string by key with `{arg}` filled; an unknown key reads as itself. A
/// missing arg fills as empty; `profile` and `mode` read upper case; a
/// `source` or `action` value that names a key of its own reads as that key.
pub fn spec_text(key: &str, args: &[(&str, &str)]) -> String {
    let Some(template) = STRINGS.get(key) else {
        return key.to_string();
    };
    fill_placeholders(template, |name| {
        let v = args.iter().find(|(k, _)| *k == name).map(|(_, v)| *v)?;
        let own = ARG_KEYS
            .iter()
            .find(|(arg, _)| *arg == name)
            .and_then(|(_, prefix)| STRINGS.get(&format!("{prefix}{v}")));
        Some(match own {
            Some(text) => text.clone(),
            None if name == "profile" || name == "mode" => v.to_uppercase(),
            None => v.to_string(),
        })
    })
}

/// Replace every `{name}` (`name` of `a-z` and `_`) in `template` with
/// `value(name)`, or with nothing when it has none; any other brace stays.
pub fn fill_placeholders(template: &str, value: impl Fn(&str) -> Option<String>) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        let name_len = after
            .find(|c: char| !(c.is_ascii_lowercase() || c == '_'))
            .unwrap_or(after.len());
        if name_len > 0 && after[name_len..].starts_with('}') {
            out.push_str(&value(&after[..name_len]).unwrap_or_default());
            rest = &after[name_len + 1..];
        } else {
            out.push('{');
            rest = after;
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_key_reads_its_text_and_an_unknown_key_reads_as_itself() {
        assert_eq!(spec_text("agl.source.home", &[]), "HOME");
        assert_eq!(spec_text("no.such.key", &[]), "no.such.key");
        assert!(has_spec_text("pilot.link_loss_action.rth"));
        assert!(!has_spec_text("pilot.link_loss_action."));
    }

    #[test]
    fn args_fill_and_an_action_reads_through_its_own_key() {
        assert_eq!(
            spec_text(
                "flow.link_loss.countdown",
                &[("action", "rth"), ("in_s", "4")]
            ),
            "Link lost: Return home in 4 s"
        );
        assert_eq!(
            spec_text("flow.link_loss.countdown", &[("action", "Hold")]),
            "Link lost: Hold in  s"
        );
    }

    #[test]
    fn placeholders_leave_other_braces_alone() {
        let v = |n: &str| (n == "a").then(|| "X".to_string());
        assert_eq!(fill_placeholders("{a} {b} {A} {", v), "X  {A} {");
    }
}
