//! The chip's words: the placeholders a rule's text fills from the facts
//! (profile, skill label, distances, ETA, launch step), as `flight-state.ts`
//! fills them.

use super::{js_round, ChipFacts, SPEC};
use crate::model::spec_text::{fill_placeholders, spec_text};

pub(super) fn fmt_eta(s: Option<f64>) -> String {
    let Some(s) = s.filter(|s| s.is_finite()) else {
        return String::new();
    };
    // Round the total first, so 119.6 s reads 2:00, never 1:60.
    let total = js_round(s) as i64;
    let m = total.div_euclid(60);
    if m > 0 {
        format!("ETA {m}:{:02}", total % 60)
    } else {
        format!("ETA {total} s")
    }
}

/// A launch step's name as the spec words it (`flow.launch.step.*`, `after`
/// read as its label). The take-off step reads as its action
/// (`chip.launch_step.takeoff`), so the chip never repeats "take off".
fn launch_step_name(step: Option<&str>) -> String {
    let key = match step {
        None => return String::new(),
        Some("after") => "flow.launch.step.after_label".to_string(),
        Some("takeoff") => "chip.launch_step.takeoff".to_string(),
        Some(s) => format!("flow.launch.step.{s}"),
    };
    spec_text(&key, &[])
}

/// A word that starts a chip part, with a capital first letter.
fn capital_first(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) => c.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// A wire identifier (`autotune_roll`) as a sentence-case word ("Autotune roll").
pub(super) fn word_label(s: &str) -> String {
    // Each run of `-`/`_` becomes one space, as the shared `[-_]+` replace does.
    let mut spaced = String::with_capacity(s.len());
    let mut in_sep = false;
    for c in s.chars() {
        if c == '-' || c == '_' {
            if !in_sep {
                spaced.push(' ');
            }
            in_sep = true;
        } else {
            spaced.push(c);
            in_sep = false;
        }
    }
    capital_first(&spaced.to_lowercase())
}

/// A profile as the mode sheet labels it (`mode_sheet.profile`), "Normal"
/// when none is reported; an unknown value reads with a capital first letter.
fn profile_label(profile: Option<&str>) -> String {
    let value = profile.unwrap_or("normal");
    let sheet = &SPEC.mode_sheet.profile;
    match sheet.values.iter().position(|v| v == value) {
        Some(i) => sheet.labels[i].clone(),
        None => capital_first(value),
    }
}

fn num(v: Option<f64>) -> String {
    match v.filter(|v| v.is_finite()) {
        Some(v) => (js_round(v) as i64).to_string(),
        None => "—".to_string(),
    }
}

pub(super) fn fill(template: &str, f: &ChipFacts) -> String {
    let filled = fill_placeholders(template, |key| {
        Some(match key {
            "first_block_msg" => f.first_block_msg.clone().unwrap_or_default(),
            "profile" => profile_label(f.profile.as_deref()),
            "skill_label" => capital_first(f.skill_label.as_deref().unwrap_or("Skill")),
            "mode_detail" => f.mode_detail.clone().unwrap_or_else(|| "Active".into()),
            "dist_m" => num(f.dist_m),
            "home_dist_m" => num(f.home_dist_m),
            "setup_step" => word_label(f.setup_step.as_deref().unwrap_or("")),
            "seq" => num(f.seq),
            "total" => num(f.total),
            "eta" => fmt_eta(f.eta_s),
            "fc_mode_name_upper" => f.fc_mode_name.as_deref().unwrap_or("—").to_uppercase(),
            "step" => launch_step_name(f.launch_step.as_deref()),
            _ => return None,
        })
    });
    match filled.strip_suffix(" · ") {
        Some(trimmed) => trimmed.to_string(),
        None => filled,
    }
}
