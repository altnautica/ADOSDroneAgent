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
use super::pilot_alerts::short_title;

pub mod readouts;
mod secondary;
mod words;

pub use secondary::chip_secondary;
use secondary::{Clearance, CommandLimit, InputAge, Keyed, KeyedPart, RouteRound, SpeedPart};
use words::{fill, word_label};

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
    speed_cap: Styled,
    pose_uncertainty: KeyedPart,
    route_round: RouteRound,
    mission_paused_line: Conditional,
    avoid_state: BTreeMap<String, Option<Styled>>,
    hold_reason: Styled,
    control_mode: BTreeMap<String, String>,
    profile_limit: Keyed,
    clearance: Clearance,
    input_age: InputAge,
    h_speed: SpeedPart,
    v_speed: SpeedPart,
    command_limit: CommandLimit,
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
    position_unverified_labels: BTreeMap<String, String>,
    command_limit_labels: BTreeMap<String, String>,
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
    /// The pilot's horizontal speed cap in m/s, while one applies.
    pub speed_cap_mps: Option<f64>,
    /// Why the cap (`gnss-denied`, `confined-near`, `position-uncertain`),
    /// while one applies.
    pub speed_cap_reason: Option<String>,
    pub avoid_state: Option<String>,
    /// The step of a running launch sequence (not `done`, no error).
    pub launch_step: Option<String>,
    /// The FC flies a failsafe RTL or land of its own.
    pub fc_failsafe: bool,
    /// The pilot's position-unverified label key (its `detail` when the spec
    /// labels it, else its `reason`); `None` while the position is trusted.
    pub position_unverified: Option<String>,
    /// The position's one-sigma uncertainty (m) while the pilot flies on it
    /// slowed (`pose_uncertainty_m`).
    pub pose_uncertainty_m: Option<f64>,
    /// The mission item a detour round a blocked leg resumes at.
    pub detour_seq: Option<u32>,
    /// A routed return: the way left and the straight line home, metres.
    pub return_route: Option<(f64, f64)>,
    /// The horizontal limit the sticks fly at (m/s), from `profile_limits`.
    pub profile_limit_mps: Option<f64>,
    /// The vehicle's horizontal and vertical (+ up) speed (m/s), when the
    /// surface passes them.
    pub h_speed_mps: Option<f64>,
    pub v_speed_mps: Option<f64>,
    /// Age of the stick frame the pilot flew last (ms), while the sticks fly.
    pub stick_input_age_ms: Option<f64>,
    /// The nearest obstacle (m) and the margin the pilot keeps (m).
    pub min_clearance_m: Option<f64>,
    pub margin_m: Option<f64>,
    /// The pilot's `command.limits` codes, first bound first.
    pub command_limits: Vec<String>,
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
    /// The vehicle's horizontal and vertical (+ up) speed (m/s) for the
    /// chip's speed parts; `None` on a surface that shows them elsewhere.
    pub h_speed_mps: Option<f64>,
    pub v_speed_mps: Option<f64>,
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
        "position_unverified" => is_bool(want, f.position_unverified.is_some()),
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
        .unwrap_or_else(|| word_label(mode))
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
            text: word_label(&f.owner),
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
        .map(|r| short_title(&r.msg, !r.params.is_empty()).to_string());
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
        speed_cap_mps: p.speed_cap_mps,
        speed_cap_reason: p.speed_cap_reason.clone(),
        avoid_state: p.avoid_state.clone(),
        pose_uncertainty_m: p.pose_uncertainty_m,
        detour_seq: p.mission_detour.as_ref().map(|d| d.seq),
        return_route: p
            .return_route
            .as_ref()
            .map(|r| (f64::from(r.remaining_m), f64::from(r.straight_m))),
        launch_step: p.launch_step.clone(),
        fc_failsafe: p.fc_failsafe,
        position_unverified: p.position_unverified.as_ref().map(|u| match &u.detail {
            Some(d) if SPEC.position_unverified_labels.contains_key(d) => d.clone(),
            _ => u.reason.clone(),
        }),
        profile_limit_mps: p.profile_limits.map(|l| f64::from(l.v_h_mps)),
        h_speed_mps: ctx.h_speed_mps,
        v_speed_mps: ctx.v_speed_mps,
        stick_input_age_ms: p.stick.map(|s| f64::from(s.input_age_ms)),
        min_clearance_m: p.min_clearance_m,
        margin_m: p
            .profile_limits
            .map(|l| f64::from(l.margin_m))
            .filter(|m| *m > 0.0)
            .or_else(|| p.route_check.as_ref().and_then(|r| r.margin_m)),
        command_limits: p.command_limits.clone(),
        age_ms,
        owner,
    })
}

#[cfg(test)]
mod tests;
