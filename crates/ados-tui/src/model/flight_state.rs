//! The flight-state chip: who flies the aircraft now, as one line of text and
//! a colour, plus the secondary line (hold reason, avoidance state, control
//! mode). The rules are the cockpit spec's `flight-state.json`, compiled into
//! the binary and evaluated in order on facts drawn from the pilot kind; the
//! first match wins. This is the shared `flight-state.ts` (`chipFacts` and
//! the resolver) and `chip-inputs.ts` evaluated in Rust, checked against the
//! same golden cases (`web-shared/src/hud/fixtures/chip-cases.json`).
//!
//! Beside the chip, in [`readouts`]: the link-loss action and countdown and
//! the height above ground with its source mark.

use std::collections::BTreeMap;
use std::sync::LazyLock;

use serde::Deserialize;
use serde_json::{Map, Value};

use super::autonomy::Pilot;

pub mod readouts;
mod words;

use words::fill;

/// The spec file, byte for byte.
pub const FLIGHT_STATE_JSON: &str =
    include_str!("../../../../web-shared/src/cockpit-spec/flight-state.json");

#[derive(Debug, Deserialize)]
struct Rule {
    id: String,
    #[serde(rename = "match")]
    when: Map<String, Value>,
    text: String,
    color: String,
    #[serde(default)]
    dim: bool,
    button: Option<String>,
    /// A different button while a further condition holds.
    #[serde(default)]
    button_if: Option<ButtonIf>,
    #[serde(default)]
    suffix_if_landing_seq: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ButtonIf {
    #[serde(rename = "match")]
    when: Map<String, Value>,
    button: String,
}

#[derive(Debug, Deserialize)]
struct Families {
    copter: Vec<f64>,
    plane: Vec<f64>,
}

#[derive(Debug, Deserialize)]
struct FcModeClasses {
    rtl: Families,
    land: Families,
}

#[derive(Debug, Deserialize)]
struct Styled {
    text: String,
    color: String,
}

/// A secondary part shown only while its own condition holds.
#[derive(Debug, Deserialize)]
struct Conditional {
    #[serde(rename = "match")]
    when: Map<String, Value>,
    text: String,
    color: String,
}

#[derive(Debug, Deserialize)]
struct Secondary {
    order: Vec<String>,
    fc_failsafe: Styled,
    mission_paused_line: Conditional,
    avoid_state: BTreeMap<String, Option<Styled>>,
    hold_reason: Styled,
    control_mode: BTreeMap<String, String>,
}

#[derive(Debug, Deserialize)]
struct Labelled {
    values: Vec<String>,
    labels: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct ModeSheet {
    profile: Labelled,
}

#[derive(Debug, Deserialize)]
struct Spec {
    rules: Vec<Rule>,
    fc_mode_classes: FcModeClasses,
    secondary: Secondary,
    hold_reason_labels: BTreeMap<String, String>,
    stale_after_ms: u64,
    mode_sheet: ModeSheet,
}

static SPEC: LazyLock<Spec> = LazyLock::new(|| {
    serde_json::from_str(FLIGHT_STATE_JSON).expect("flight-state.json is the chip spec")
});

/// The age past which a pilot state (and every nav kind beside it) is stale:
/// the spec's `stale_after_ms`, where the chip dims.
pub fn stale_after_ms() -> u64 {
    SPEC.stale_after_ms
}

/// The pilot owners the chip knows; a pilot state naming another is no
/// pilot state (as the shared parser drops it).
pub const PILOT_OWNERS: [&str; 8] = [
    "idle",
    "pilot",
    "skill",
    "goto",
    "swarm",
    "mission_fc",
    "fc_mode",
    "tuning",
];

/// An `ar` frame older than this carries no ETA (the shared
/// `chip-inputs.ts` `AR_ETA_MAX_AGE_MS`).
pub const AR_ETA_MAX_AGE_MS: u64 = 2_000;

/// The values a rule can match on and fill its placeholders from.
#[derive(Debug, Clone, PartialEq)]
pub struct ChipFacts {
    pub owner: String,
    pub ready: bool,
    pub first_block_msg: Option<String>,
    pub setup_needed: bool,
    pub profile: Option<String>,
    pub skill_label: Option<String>,
    pub mode_detail: Option<String>,
    pub dist_m: Option<f64>,
    pub home_dist_m: Option<f64>,
    pub setup_step: Option<String>,
    pub mission_state: Option<String>,
    pub seq: Option<f64>,
    pub total: Option<f64>,
    pub eta_s: Option<f64>,
    pub landing_seq: bool,
    /// Ms since the mission state turned `complete`, or `None`.
    pub complete_age_ms: Option<f64>,
    pub vtol_transitioning: bool,
    pub fc_mode: Option<f64>,
    pub fc_mode_name: Option<String>,
    /// Q modes count as plane.
    pub plane: bool,
    pub control_mode: Option<String>,
    pub hold_reason: Option<String>,
    pub avoid_state: Option<String>,
    /// The step of a running launch sequence (not `done`, no error).
    pub launch_step: Option<String>,
    /// The FC flies a failsafe RTL or land of its own.
    pub fc_failsafe: bool,
    /// Age of the pilot state in ms.
    pub age_ms: u64,
}

/// One part of the secondary line: its text and the spec colour name.
#[derive(Debug, Clone, PartialEq)]
pub struct ChipLine {
    pub text: String,
    pub color: String,
}

/// The chip for one set of facts. `color` is a spec colour name
/// (`tokens.json` `state.*`); the surface maps it to its own palette.
#[derive(Debug, Clone, PartialEq)]
pub struct ChipView {
    pub rule_id: String,
    pub text: String,
    pub color: String,
    pub dim: bool,
    /// `take_control`, `resume`, `abort` (a running launch) or `setup` (the
    /// "Set up aircraft" link).
    pub button: Option<&'static str>,
    pub secondary: Vec<ChipLine>,
}

/// What a surface knows beside the pilot kind.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ChipContext {
    pub home_dist_m: Option<f64>,
    /// The FC's local position (north, east), metres about the NED origin.
    pub local_pos: Option<(f64, f64)>,
    /// The next waypoint's ETA from a fresh `ar` frame.
    pub eta_s: Option<f64>,
    /// The running skill's detail line; used only while a skill owns.
    pub mode_detail: Option<String>,
    pub complete_age_ms: Option<f64>,
}

/// JavaScript's `Math.round`: halves round up.
fn js_round(v: f64) -> f64 {
    (v + 0.5).floor()
}

fn fc_mode_class(f: &ChipFacts) -> Option<&'static str> {
    let mode = f.fc_mode?;
    let classes = &SPEC.fc_mode_classes;
    [("rtl", &classes.rtl), ("land", &classes.land)]
        .into_iter()
        .find(|(_, fam)| {
            let modes = if f.plane { &fam.plane } else { &fam.copter };
            modes.contains(&mode)
        })
        .map(|(name, _)| name)
}

/// A condition value against a fact, with the strict equality of the shared
/// evaluator: a string matches only that string, `null` only an absent fact.
fn is(want: &Value, v: Option<&str>) -> bool {
    match v {
        Some(s) => want.as_str() == Some(s),
        None => want.is_null(),
    }
}

fn is_bool(want: &Value, v: bool) -> bool {
    want.as_bool() == Some(v)
}

fn matches(when: &Map<String, Value>, f: &ChipFacts) -> bool {
    when.iter().all(|(key, want)| match key.as_str() {
        "pilot_stale" => is_bool(want, f.age_ms >= SPEC.stale_after_ms),
        "owner" => is(want, Some(&f.owner)),
        "ready" => is_bool(want, f.ready),
        "mission_state" => is(want, f.mission_state.as_deref()),
        "complete_age_lt_ms" => match (f.complete_age_ms, want.as_f64()) {
            (Some(age), Some(limit)) => age < limit,
            (Some(_), None) => true,
            (None, _) => false,
        },
        "vtol_transitioning" => is_bool(want, f.vtol_transitioning),
        "fc_mode_class" => is(want, fc_mode_class(f)),
        "control_mode" => is(want, f.control_mode.as_deref()),
        "sequence_running" => is_bool(want, f.launch_step.is_some()),
        "fc_failsafe" => is_bool(want, f.fc_failsafe),
        // A condition this cockpit does not know never matches.
        _ => false,
    })
}

/// A hold reason as the spec labels it; an unlabelled one reads with its
/// separators as spaces.
pub fn hold_reason_label(reason: &str) -> String {
    SPEC.hold_reason_labels
        .get(reason)
        .cloned()
        .unwrap_or_else(|| reason.replace(['-', '_'], " "))
}

/// The avoidance state as the chip names it (`Avoiding`, `Blocked`, ...),
/// with its spec colour; `None` for `clear` and unknown states.
pub fn avoid_state_text(state: &str) -> Option<ChipLine> {
    SPEC.secondary
        .avoid_state
        .get(state)?
        .as_ref()
        .map(|s| ChipLine {
            text: s.text.clone(),
            color: s.color.clone(),
        })
}

/// A control mode as the chip names it (`ADOS Pilot`, `FC Manual`, `ELRS`).
pub fn control_mode_text(mode: &str) -> String {
    SPEC.secondary
        .control_mode
        .get(mode)
        .cloned()
        .unwrap_or_else(|| mode.to_uppercase())
}

/// The secondary line, in the spec's order: FC failsafe, a mission the pilot
/// holds paused, hold reason, avoidance state, control mode.
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

fn button_of(rule: &Rule, f: &ChipFacts) -> Option<&'static str> {
    let name = match &rule.button_if {
        Some(alt) if matches(&alt.when, f) => Some(alt.button.as_str()),
        _ => rule.button.as_deref(),
    };
    match name {
        Some("take_control") => Some("take_control"),
        Some("resume") => Some("resume"),
        Some("abort") => Some("abort"),
        Some("setup_link_when_setup_needed") if f.setup_needed => Some("setup"),
        _ => None,
    }
}

/// The chip for a set of facts.
pub fn flight_state_chip(f: &ChipFacts) -> ChipView {
    let Some(rule) = SPEC.rules.iter().find(|r| matches(&r.when, f)) else {
        return ChipView {
            rule_id: "none".into(),
            text: f.owner.to_uppercase(),
            color: "fc".into(),
            dim: false,
            button: None,
            secondary: chip_secondary(f),
        };
    };
    let mut text = fill(&rule.text, f);
    if let (Some(suffix), true) = (&rule.suffix_if_landing_seq, f.landing_seq) {
        text.push_str(suffix);
    }
    ChipView {
        rule_id: rule.id.clone(),
        text,
        color: rule.color.clone(),
        dim: rule.dim,
        button: button_of(rule, f),
        secondary: if rule.id == "link_stale" {
            Vec::new()
        } else {
            chip_secondary(f)
        },
    }
}

/// Horizontal distance (m) from the FC's local position to the pilot goal,
/// both NED about the same origin.
pub fn goal_distance_m(local: Option<(f64, f64)>, goal: Option<[f64; 3]>) -> Option<f64> {
    let ((x, y), g) = local.zip(goal)?;
    Some((g[0] - x).hypot(g[1] - y))
}

/// Facts for the chip rules from one pilot state `age_ms` old; `None` when
/// the state names no known owner.
pub fn chip_inputs(p: &Pilot, age_ms: u64, ctx: &ChipContext) -> Option<ChipFacts> {
    let owner = p.owner.clone()?;
    let first_block = p
        .readiness
        .iter()
        .find(|r| !r.ok && r.block)
        .map(|r| r.msg.clone());
    let mission = p.mission.as_ref();
    Some(ChipFacts {
        ready: first_block.is_none(),
        first_block_msg: first_block,
        setup_needed: p.readiness.iter().any(|r| r.id == "setup_needed" && !r.ok),
        profile: p.profile.clone(),
        skill_label: p.skill_label.clone(),
        mode_detail: if owner == "skill" {
            ctx.mode_detail.clone()
        } else {
            None
        },
        dist_m: goal_distance_m(ctx.local_pos, p.goal_ned),
        home_dist_m: ctx.home_dist_m,
        setup_step: p.setup_step.clone(),
        mission_state: mission.and_then(|m| m.state.clone()),
        seq: mission.and_then(|m| m.seq),
        total: mission.and_then(|m| m.total),
        eta_s: ctx.eta_s,
        landing_seq: mission.is_some_and(|m| m.landing_seq),
        complete_age_ms: ctx.complete_age_ms,
        vtol_transitioning: p.vtol_transitioning,
        fc_mode: p.fc_mode,
        fc_mode_name: p.fc_mode_name.clone(),
        plane: matches!(p.vehicle_class.as_deref(), Some("fixedwing" | "vtol")),
        control_mode: p.control_mode.clone(),
        hold_reason: p.hold_reason.clone(),
        avoid_state: p.avoid_state.clone(),
        launch_step: p.launch_step.clone(),
        fc_failsafe: p.fc_failsafe,
        age_ms,
        owner,
    })
}

#[cfg(test)]
mod tests;
