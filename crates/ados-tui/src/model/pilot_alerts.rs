//! The status pill's alert list, as the shared `status-pill.ts` `pillAlerts`
//! builds it: the shared base alerts (link, battery, packs, GPS), the agent
//! not answering, a pilot state too old to trust or the FC's own failsafe, a
//! mission stall, why the setup wizard aborted, failing readiness items, a
//! lost controller, failing pre-flight items, a degraded navigation source,
//! the FC's PreArm/Arm text, its status texts at WARNING or worse, and the
//! on-board storage. The same rules, ids, texts, order, sections (`group`),
//! snooze keys and readiness detail (`params`, `fix`), held to the golden
//! `pill-cases.json`.

use ados_protocol::pilot::{ParamDiff, ReadinessFix};

use super::autonomy::Pilot;
use super::base_alerts::{storage_crit_pct, storage_warn_pct, BaseAlert, Level};
use super::flight_state::stale_after_ms;
use super::mission_stall::stall_view;
use super::nav_source::{NavPill, NavTone};
use super::spec_text::{has_spec_text, spec_text};

mod groups;
pub use groups::{alert_group, group_alerts, AlertGroup};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Critical,
    Warning,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PillAlert {
    /// The shared pill id (`failsafe`, `pilot:<item>`, `fc:<text>`, ...).
    pub id: String,
    pub severity: Severity,
    pub text: String,
    /// The section it lists under (`alert-groups.json`).
    pub group: AlertGroup,
    /// The shared snooze key, `id|text`.
    pub key: String,
    /// The parameter rows behind a readiness item; empty for any other.
    pub params: Vec<ParamDiff>,
    /// The fix a readiness item offers.
    pub fix: Option<ReadinessFix>,
}

/// A PreArm/Arm line without its prefix, or `None` for any other text.
fn prearm_body(text: &str) -> Option<&str> {
    ["PreArm:", "Arm:"]
        .iter()
        .find_map(|p| text.strip_prefix(p))
        .map(str::trim_start)
}

/// Whether the pill treats the pilot state as not answering: marked stale,
/// of unknown age, or at least `flight-state.json` `stale_after_ms` old.
fn pilot_stale(p: &Pilot, age_ms: Option<u64>) -> bool {
    p.stale || age_ms.is_none_or(|a| a >= stale_after_ms())
}

/// `Failsafe: <cause>`, the cause worded by `pilot.banner.cause.*`; no cause
/// reads as the flight controller, an unknown one as itself.
pub fn failsafe_text(cause: Option<&str>) -> String {
    let c = cause.filter(|c| !c.is_empty()).unwrap_or("fc");
    let key = format!("pilot.banner.cause.{c}");
    let worded = if has_spec_text(&key) {
        spec_text(&key, &[])
    } else {
        c.to_string()
    };
    spec_text("alert.failsafe", &[("cause", &worded)])
}

/// The low-battery item's text for `action` (`land`, `rth`); the pilot
/// announces the same line as a STATUSTEXT.
pub fn low_battery_text(action: Option<&str>) -> Option<String> {
    match action {
        Some("land") => Some(spec_text("alert.low_battery_land", &[])),
        Some("rth") => Some(spec_text("alert.low_battery_rth", &[])),
        _ => None,
    }
}

/// The low-battery item as listed: a return the estimate of the way home
/// asked for names the charge it needs.
fn low_battery_item(p: &Pilot) -> Option<String> {
    match (p.low_battery_action.as_deref(), p.energy_need_pct) {
        (Some("rth"), Some(need)) => Some(spec_text(
            "alert.low_battery_rth_energy",
            &[("need_pct", &need.to_string())],
        )),
        (action, _) => low_battery_text(action),
    }
}

/// The pilot holds for a lost controller while armed and airborne. An
/// unknown arm or landed state is possibly airborne.
pub fn controller_lost(p: &Pilot) -> bool {
    p.hold_reason.as_deref() == Some("controller-lost")
        && p.armed != Some(false)
        && p.landed != Some(true)
}

/// What the pill reads.
#[derive(Default)]
pub struct PillInputs<'a> {
    /// The shared base alerts; advisories stay out of the pill.
    pub alerts: &'a [BaseAlert],
    /// The pilot kind; `None` when nothing publishes it (the pilot rules
    /// are off).
    pub pilot: Option<&'a Pilot>,
    /// The pilot state's age now (the read's age plus the time since the
    /// read, so a hung poll ages it); `None` is unknown, so stale.
    pub pilot_age_ms: Option<u64>,
    /// The FC's PreArm/Arm texts; `None` while unknown.
    pub prearm: Option<&'a [String]>,
    pub storage_pct: Option<f64>,
    /// The agent itself does not answer.
    pub agent_unreachable: bool,
    /// The arm state is unknown (the drone may be airborne): pre-flight
    /// items hide.
    pub arm_unknown: bool,
    /// The failing pre-flight checklist items (id, label).
    pub preflight_failing: &'a [(String, String)],
    pub nav_source: Option<NavPill>,
}

/// The pill source of a base alert id (`alertSource`).
fn alert_source(id: &str) -> &'static str {
    match id {
        "batt_crit" | "batt_low" => "battery",
        _ if id.starts_with("battery_pack_") => "battery",
        "gps_no_3d" => "nav_source",
        "video_frozen" => "video",
        _ => "link",
    }
}

/// Add an alert from pill source `source` (`cockpit.status.source.*`), which
/// with its id picks the section; returns it for readiness detail.
fn push<'o>(
    out: &'o mut Vec<PillAlert>,
    id: impl Into<String>,
    source: &str,
    severity: Severity,
    text: String,
) -> &'o mut PillAlert {
    let id = id.into();
    let group = alert_group(&id, source);
    let key = format!("{id}|{text}");
    out.push(PillAlert {
        id,
        severity,
        text,
        group,
        key,
        params: Vec::new(),
        fix: None,
    });
    out.last_mut().expect("just pushed")
}

/// Every active alert, critical first, then in source order. A stale pilot
/// state contributes only the not-answering item, never its readiness,
/// controller or status-text items.
pub fn pill_alerts(i: &PillInputs) -> Vec<PillAlert> {
    let mut out = Vec::new();
    for a in i.alerts {
        let severity = match a.level {
            Level::Critical => Severity::Critical,
            Level::Warning => Severity::Warning,
            Level::Advisory => continue,
        };
        let id = format!("alert:{}", a.id);
        push(&mut out, id, alert_source(&a.id), severity, a.title.clone());
    }
    if i.agent_unreachable {
        let text = spec_text("alert.agent_unreachable", &[]);
        push(
            &mut out,
            "agent_unreachable",
            "link",
            Severity::Critical,
            text,
        );
    }
    let fresh = i.pilot.filter(|p| !pilot_stale(p, i.pilot_age_ms));
    match (i.pilot, fresh) {
        (Some(_), None) => {
            let text = spec_text("alert.pilot_not_answering", &[]);
            push(
                &mut out,
                "pilot_not_answering",
                "pilot",
                Severity::Warning,
                text,
            );
        }
        (_, Some(p)) if p.fc_failsafe => {
            let text = failsafe_text(p.failsafe_cause.as_deref());
            push(&mut out, "failsafe", "pilot", Severity::Critical, text);
        }
        _ => {}
    }
    // Why the setup wizard aborted the air run; an unknown key reads as itself.
    if let Some(key) = fresh
        .and_then(|p| p.setup_abort.as_deref())
        .filter(|k| !k.is_empty())
    {
        let text = spec_text(key, &[]);
        push(&mut out, "setup_abort", "pilot", Severity::Warning, text);
    }
    // The pilot's STATUSTEXT line folds into the item (`low_battery`).
    let low_battery = fresh.and_then(|p| low_battery_text(p.low_battery_action.as_deref()));
    if let Some(text) = fresh.and_then(low_battery_item) {
        push(&mut out, "low_battery", "battery", Severity::Critical, text);
    }
    if let Some(s) = fresh.and_then(|p| p.mission_stall.as_ref()) {
        let text = stall_view(s).title;
        push(&mut out, "mission_stall", "pilot", Severity::Critical, text);
    }
    for r in fresh
        .into_iter()
        .flat_map(|p| &p.readiness)
        .filter(|r| !r.ok)
    {
        // A blocking item refuses take-off: critical, as the pre-flight sheet fails it.
        let severity = if r.block {
            Severity::Critical
        } else {
            Severity::Warning
        };
        let text = if r.msg.is_empty() {
            r.id.clone()
        } else {
            r.msg.clone()
        };
        let id = format!("pilot:{}", r.id);
        let a = push(&mut out, id, "pilot_readiness", severity, text);
        a.params.clone_from(&r.params);
        a.fix.clone_from(&r.fix);
    }
    if fresh.is_some_and(controller_lost) {
        let text = spec_text("alert.controller_lost", &[]);
        push(
            &mut out,
            "controller_lost",
            "controller",
            Severity::Warning,
            text,
        );
    }
    let preflight = if i.arm_unknown {
        &[][..]
    } else {
        i.preflight_failing
    };
    for (id, label) in preflight {
        let text = spec_text("alert.preflight_failing", &[("item", label)]);
        let id = format!("preflight:{id}");
        push(&mut out, id, "preflight", Severity::Warning, text);
    }
    if let Some(nav) = i.nav_source {
        if let Some(key) = nav.kind.key() {
            let severity = match nav.tone {
                NavTone::Crit => Some(Severity::Critical),
                NavTone::Warn => Some(Severity::Warning),
                _ => None,
            };
            if let Some(severity) = severity {
                let text = spec_text(
                    "alert.nav_source_degraded",
                    &[("source", &spec_text(key, &[]))],
                );
                push(
                    &mut out,
                    "nav_source_degraded",
                    "nav_source",
                    severity,
                    text,
                );
            }
        }
    }
    // Every distinct PreArm/Arm line the FC repeats: each names its own cause.
    let mut prearm_seen: Vec<&str> = Vec::new();
    for line in i.prearm.unwrap_or_default() {
        let text = prearm_body(line).unwrap_or(line);
        if text.is_empty() || prearm_seen.contains(&text) {
            continue;
        }
        prearm_seen.push(text);
        let text = text.to_string();
        push(
            &mut out,
            format!("fc:{text}"),
            "fc_prearm",
            Severity::Warning,
            text,
        );
    }
    let fc_start = out.len();
    for s in fresh.into_iter().flat_map(|p| &p.statustext) {
        // PreArm/Arm lines already arrive through `prearm`; the pilot's own
        // low-battery line folds into its `low_battery` item.
        if prearm_body(&s.text).is_some() || low_battery.as_deref() == Some(s.text.as_str()) {
            continue;
        }
        let severity = if s.severity <= 2.0 {
            Severity::Critical
        } else {
            Severity::Warning
        };
        match out[fc_start..].iter_mut().find(|a| a.text == s.text) {
            Some(seen) if severity == Severity::Critical => seen.severity = severity,
            Some(_) => {}
            None => {
                let id = format!("fcst:{}", s.text);
                push(&mut out, id, "fc_status_text", severity, s.text.clone());
            }
        }
    }
    if let Some(pct) = i.storage_pct.filter(|p| *p >= storage_warn_pct()) {
        let severity = if pct >= storage_crit_pct() {
            Severity::Critical
        } else {
            Severity::Warning
        };
        let n = (pct.round() as i64).to_string();
        let text = spec_text("cockpit.status.storage_used", &[("pct", &n)]);
        push(&mut out, "storage", "storage", severity, text);
    }
    // A stable sort: equal severities keep their source order.
    out.sort_by_key(|a| a.severity != Severity::Critical);
    out
}

/// An alert's one-line title (`alertTitle`).
pub fn alert_title(a: &PillAlert) -> &str {
    short_title(&a.text, !a.params.is_empty())
}

/// A readiness or alert message's one-line title (`shortTitle`): with
/// parameter rows behind it the rows carry the detail, so the title is the
/// words before the first `": "` ("FC parameters differ").
pub fn short_title(text: &str, has_params: bool) -> &str {
    if !has_params {
        return text;
    }
    match text.find(": ") {
        Some(at) if at > 0 => &text[..at],
        _ => text,
    }
}

/// The count of active alerts as the pill words it, blockers first so a
/// warning count never hides one (`3 warnings`, `2 blocking`,
/// `2 blocking · 5 warnings`); `None` with none.
pub fn alerts_count_text(alerts: &[PillAlert]) -> Option<String> {
    let count = alerts.len();
    let blocking = alerts
        .iter()
        .filter(|a| a.severity == Severity::Critical)
        .count();
    let warnings = count - blocking;
    let n = |v: usize| v.to_string();
    Some(match (count, blocking, warnings) {
        (0, _, _) => return None,
        (1, 0, _) => spec_text("cockpit.status.one", &[]),
        (_, 0, _) => spec_text("cockpit.status.many", &[("n", &n(count))]),
        (_, b, 0) => spec_text("cockpit.status.blocking", &[("n", &n(b))]),
        (_, b, 1) => spec_text("cockpit.status.blocking_one_warning", &[("n", &n(b))]),
        (_, b, w) => spec_text(
            "cockpit.status.blocking_warnings",
            &[("n", &n(b)), ("m", &n(w))],
        ),
    })
}

#[cfg(test)]
mod tests;
