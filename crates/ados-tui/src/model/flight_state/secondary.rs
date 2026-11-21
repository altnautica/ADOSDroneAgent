//! The chip's secondary line, in the spec's order, as `flight-state.ts`
//! builds it: FC failsafe, the obstacle clearance, a late stick input, the
//! position's uncertainty, the pilot's speed cap, the limit that cut the
//! commanded speed, a mission the
//! pilot holds paused, hold reason, avoidance state, control mode, then the
//! horizontal and vertical speed while the sticks fly.

use serde::Deserialize;

use super::words::fill;
use super::{
    avoid_state_text, control_mode_text, hold_reason_label, js_round, matches, ChipFacts, ChipLine,
    SPEC,
};
use crate::model::route_check::speed_cap_line;
use crate::model::spec_text::spec_text;

/// The limit that cut the commanded speed: the first `command.limits` code
/// not in `skip`, while one of `owners` flies.
#[derive(Debug, Deserialize)]
pub(super) struct CommandLimit {
    owners: Vec<String>,
    skip: Vec<String>,
    suppress_while_blocked: Vec<String>,
    color: String,
    #[serde(default)]
    colors: std::collections::BTreeMap<String, String>,
}

/// The `command.limits` code the chip shows, as `commandLimitCode` in
/// `flight-state.ts` picks it: none with no label, and none of the
/// `suppress_while_blocked` codes while avoidance reports blocked.
fn command_limit_code(f: &ChipFacts) -> Option<&str> {
    let c = &SPEC.secondary.command_limit;
    if !c.owners.contains(&f.owner) {
        return None;
    }
    let code = f.command_limits.iter().find(|l| !c.skip.contains(l))?;
    if !SPEC.command_limit_labels.contains_key(code) {
        return None;
    }
    if f.avoid_state.as_deref() == Some("blocked") && c.suppress_while_blocked.contains(code) {
        return None;
    }
    Some(code)
}

/// A part worded by a spec string (`setup-strings.json`).
#[derive(Debug, Deserialize)]
pub(super) struct Keyed {
    pub(super) text_key: String,
}

/// The obstacle clearance part: shown under `near_factor` × the margin.
#[derive(Debug, Deserialize)]
pub(super) struct Clearance {
    text_key: String,
    near_factor: f64,
    near_color: String,
    inside_margin_color: String,
}

/// The late stick input part.
#[derive(Debug, Deserialize)]
pub(super) struct InputAge {
    text_key: String,
    over_ms: f64,
    color: String,
}

/// A part worded by a spec string, in one colour.
#[derive(Debug, Deserialize)]
pub(super) struct KeyedPart {
    text_key: String,
    color: String,
}

/// A speed part, shown only while `owner` flies.
#[derive(Debug, Deserialize)]
pub(super) struct SpeedPart {
    text_key: String,
    owner: String,
    color: String,
}

/// The secondary line for a set of facts.
pub fn chip_secondary(f: &ChipFacts) -> Vec<ChipLine> {
    let sec = &SPEC.secondary;
    let mut out = Vec::new();
    for part in &sec.order {
        match part.as_str() {
            "fc_failsafe" => {
                if f.fc_failsafe {
                    out.push(ChipLine {
                        text: sec.fc_failsafe.text.clone(),
                        color: sec.fc_failsafe.color.clone(),
                    });
                }
            }
            "clearance" => out.extend(clearance(&sec.clearance, f)),
            "input_age" => {
                let a = &sec.input_age;
                if let Some(ms) = f.stick_input_age_ms.filter(|ms| *ms > a.over_ms) {
                    out.push(ChipLine {
                        text: spec_text(&a.text_key, &[("ms", &(js_round(ms) as i64).to_string())]),
                        color: a.color.clone(),
                    });
                }
            }
            "h_speed" | "v_speed" => {
                let (s, v) = if part == "h_speed" {
                    (&sec.h_speed, f.h_speed_mps)
                } else {
                    (&sec.v_speed, f.v_speed_mps)
                };
                if let (true, Some(v)) = (f.owner == s.owner, v) {
                    out.push(ChipLine {
                        text: spec_text(&s.text_key, &[("v", &signed1(v))]),
                        color: s.color.clone(),
                    });
                }
            }
            "pose_uncertainty" => {
                let u = &sec.pose_uncertainty;
                if let Some(m) = f.pose_uncertainty_m {
                    out.push(ChipLine {
                        text: spec_text(&u.text_key, &[("m", &format!("{m:.1}"))]),
                        color: u.color.clone(),
                    });
                }
            }
            "speed_cap" => {
                if let Some(v) = f.speed_cap_mps {
                    // The reason's own spec line; the spec's default line
                    // for a reason it does not name.
                    let own = f
                        .speed_cap_reason
                        .as_deref()
                        .and_then(|r| speed_cap_line(r, v));
                    out.push(ChipLine {
                        text: own
                            .unwrap_or_else(|| sec.speed_cap.text.replace("{v}", &cap_text(v))),
                        color: sec.speed_cap.color.clone(),
                    });
                }
            }
            "command_limit" => {
                if let Some(code) = command_limit_code(f) {
                    let c = &sec.command_limit;
                    out.push(ChipLine {
                        text: SPEC.command_limit_labels[code].clone(),
                        color: c.colors.get(code).unwrap_or(&c.color).clone(),
                    });
                }
            }
            "mission_paused_line" => {
                let line = &sec.mission_paused_line;
                if matches(&line.when, f) {
                    out.push(ChipLine {
                        text: fill(&line.text, f),
                        color: line.color.clone(),
                    });
                }
            }
            "hold_reason" => {
                if let Some(reason) = &f.hold_reason {
                    out.push(ChipLine {
                        text: hold_reason_label(reason),
                        color: sec.hold_reason.color.clone(),
                    });
                }
            }
            "avoid_state" => {
                if let Some(line) = f.avoid_state.as_deref().and_then(avoid_state_text) {
                    out.push(line);
                }
            }
            "control_mode" => {
                if let Some(mode) = &f.control_mode {
                    out.push(ChipLine {
                        text: control_mode_text(mode),
                        color: "fc".into(),
                    });
                }
            }
            _ => {}
        }
    }
    out
}

/// The nearest obstacle's distance, while under `near_factor` × the margin:
/// in the near colour, then in the inside colour within the margin.
fn clearance(c: &Clearance, f: &ChipFacts) -> Option<ChipLine> {
    let m = f.min_clearance_m?;
    let margin = f.margin_m.filter(|m| *m > 0.0)?;
    if m >= c.near_factor * margin {
        return None;
    }
    let color = if m <= margin {
        &c.inside_margin_color
    } else {
        &c.near_color
    };
    Some(ChipLine {
        text: spec_text(
            &c.text_key,
            &[("m", &format!("{:.1}", js_round(m * 10.0) / 10.0))],
        ),
        color: color.clone(),
    })
}

/// The cap in m/s with at most one decimal (1.5 → "1.5", 2 → "2"), as every
/// surface prints it.
fn cap_text(v: f64) -> String {
    format!("{}", (v * 10.0).round() / 10.0)
}

/// One signed decimal, never "-0.0" (the shared `fmtSigned1`).
fn signed1(v: f64) -> String {
    if v.abs() < 0.05 {
        "0.0".into()
    } else {
        format!("{v:.1}")
    }
}
