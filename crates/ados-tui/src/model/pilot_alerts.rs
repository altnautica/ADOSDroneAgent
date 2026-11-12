//! The status pill's alert list, as the shared `status-pill.ts` `pillAlerts`
//! builds it: the shared base alerts (link, battery, packs, GPS), the agent
//! not answering, a pilot state too old to trust or the FC's own failsafe,
//! failing readiness items, a lost controller, failing pre-flight items, a
//! degraded navigation source, the FC's PreArm/Arm text, its status texts at
//! WARNING or worse, and the on-board storage. The same rules, ids, texts and
//! order, held to the golden `pill-cases.json`.

use super::autonomy::Pilot;
use super::base_alerts::{storage_crit_pct, storage_warn_pct, BaseAlert, Level};
use super::flight_state::stale_after_ms;
use super::nav_source::{NavPill, NavTone};
use super::spec_text::{has_spec_text, spec_text};

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

fn push(out: &mut Vec<PillAlert>, id: impl Into<String>, severity: Severity, text: String) {
    out.push(PillAlert {
        id: id.into(),
        severity,
        text,
    });
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
        push(
            &mut out,
            format!("alert:{}", a.id),
            severity,
            a.title.clone(),
        );
    }
    if i.agent_unreachable {
        let text = spec_text("alert.agent_unreachable", &[]);
        push(&mut out, "agent_unreachable", Severity::Critical, text);
    }
    let fresh = i.pilot.filter(|p| !pilot_stale(p, i.pilot_age_ms));
    match (i.pilot, fresh) {
        (Some(_), None) => {
            let text = spec_text("alert.pilot_not_answering", &[]);
            push(&mut out, "pilot_not_answering", Severity::Warning, text);
        }
        (_, Some(p)) if p.fc_failsafe => {
            let text = failsafe_text(p.failsafe_cause.as_deref());
            push(&mut out, "failsafe", Severity::Critical, text);
        }
        _ => {}
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
        push(&mut out, format!("pilot:{}", r.id), severity, text);
    }
    if fresh.is_some_and(controller_lost) {
        let text = spec_text("alert.controller_lost", &[]);
        push(&mut out, "controller_lost", Severity::Warning, text);
    }
    let preflight = if i.arm_unknown {
        &[][..]
    } else {
        i.preflight_failing
    };
    for (id, label) in preflight {
        let text = spec_text("alert.preflight_failing", &[("item", label)]);
        push(&mut out, format!("preflight:{id}"), Severity::Warning, text);
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
                push(&mut out, "nav_source_degraded", severity, text);
            }
        }
    }
    if let Some(last) = i.prearm.and_then(<[String]>::last) {
        let text = prearm_body(last).unwrap_or(last).to_string();
        push(&mut out, format!("fc:{text}"), Severity::Warning, text);
    }
    let fc_start = out.len();
    for s in fresh.into_iter().flat_map(|p| &p.statustext) {
        // PreArm/Arm lines already arrive through `prearm`.
        if prearm_body(&s.text).is_some() {
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
            None => push(
                &mut out,
                format!("fcst:{}", s.text),
                severity,
                s.text.clone(),
            ),
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
        push(&mut out, "storage", severity, text);
    }
    // A stable sort: equal severities keep their source order.
    out.sort_by_key(|a| a.severity != Severity::Critical);
    out
}

/// The count of active alerts as the pill words it (`1 warning`,
/// `3 warnings`); `None` with none.
pub fn alerts_count_text(n: usize) -> Option<String> {
    match n {
        0 => None,
        1 => Some(spec_text("cockpit.status.one", &[])),
        n => Some(spec_text("cockpit.status.many", &[("n", &n.to_string())])),
    }
}

#[cfg(test)]
mod tests {
    use super::super::autonomy::{FcStatusText, ReadinessItem};
    use super::super::nav_source::NavKind;
    use super::*;
    use serde_json::Value;

    fn item(id: &str, ok: bool, block: bool, msg: &str) -> ReadinessItem {
        ReadinessItem {
            id: id.into(),
            ok,
            block,
            msg: msg.into(),
        }
    }

    fn st(severity: f64, text: &str) -> FcStatusText {
        FcStatusText {
            severity,
            text: text.into(),
        }
    }

    fn fresh(readiness: Vec<ReadinessItem>, statustext: Vec<FcStatusText>) -> Pilot {
        Pilot {
            readiness,
            statustext,
            age_ms: Some(100),
            ..Pilot::default()
        }
    }

    /// The pill from the pilot kind and PreArm texts alone.
    fn pilot_alerts(p: &Pilot, age_ms: Option<u64>, prearm: Option<&[String]>) -> Vec<PillAlert> {
        pill_alerts(&PillInputs {
            pilot: Some(p),
            pilot_age_ms: age_ms,
            prearm,
            ..PillInputs::default()
        })
    }

    #[test]
    fn readiness_prearm_and_status_texts_in_pill_order() {
        let p = fresh(
            vec![
                item("agl", false, false, "No rangefinder"),
                item("position", true, true, "Position ok"),
                item("fc_profile", false, true, ""),
            ],
            vec![
                st(4.0, "EKF variance"),
                st(4.0, "PreArm: Compass not calibrated"),
                st(2.0, "EKF variance"),
                st(4.0, "Low disk"),
            ],
        );
        let alerts = pilot_alerts(
            &p,
            p.age_ms,
            Some(&["PreArm: Old".into(), "Arm: Throttle too high".into()]),
        );
        let got: Vec<(Severity, &str)> = alerts
            .iter()
            .map(|a| (a.severity, a.text.as_str()))
            .collect();
        assert_eq!(
            got,
            vec![
                (Severity::Critical, "fc_profile"),
                (Severity::Critical, "EKF variance"),
                (Severity::Warning, "No rangefinder"),
                (Severity::Warning, "Throttle too high"),
                (Severity::Warning, "Low disk"),
            ]
        );
    }

    #[test]
    fn an_unknown_prearm_list_and_an_empty_one_add_nothing() {
        let p = fresh(vec![], vec![]);
        assert!(pilot_alerts(&p, p.age_ms, None).is_empty());
        assert!(pilot_alerts(&p, p.age_ms, Some(&[])).is_empty());
    }

    #[test]
    fn alerts_judge_staleness_on_the_aged_age_not_the_read_age() {
        let p = Pilot {
            fc_failsafe: true,
            ..fresh(vec![item("agl", false, true, "No rangefinder")], vec![])
        };
        let fresh_ids: Vec<String> = pilot_alerts(&p, p.age_ms, None)
            .into_iter()
            .map(|a| a.id)
            .collect();
        assert_eq!(fresh_ids, ["failsafe", "pilot:agl"]);
        // The read said 100 ms, but the poll hung: the state is old now.
        let aged = pilot_alerts(&p, Some(stale_after_ms() + 1), None);
        let aged_ids: Vec<&str> = aged.iter().map(|a| a.id.as_str()).collect();
        assert_eq!(aged_ids, ["pilot_not_answering"]);
    }

    #[test]
    fn the_pill_reads_the_spec_threshold_as_stale() {
        let p = fresh(vec![], vec![]);
        let ids = |age: u64| -> Vec<String> {
            pilot_alerts(&p, Some(age), None)
                .into_iter()
                .map(|a| a.id)
                .collect()
        };
        assert!(ids(1999).is_empty(), "1999 ms is fresh");
        assert_eq!(ids(2000), ["pilot_not_answering"], "2000 ms is stale");
    }

    #[test]
    fn a_failsafe_cause_reads_its_spec_label() {
        assert_eq!(failsafe_text(None), "Failsafe: flight controller");
        assert_eq!(
            failsafe_text(Some("rc_or_failsafe")),
            "Failsafe: RC switch or failsafe"
        );
        assert_eq!(failsafe_text(Some("battery")), "Failsafe: battery");
    }

    #[test]
    fn the_count_reads_the_spec() {
        assert_eq!(alerts_count_text(0), None);
        assert_eq!(alerts_count_text(1).as_deref(), Some("1 warning"));
        assert_eq!(alerts_count_text(3).as_deref(), Some("3 warnings"));
    }

    /// A golden case's inputs as this port reads them. The case's
    /// `controller_lost` is the shared rule's answer, so the pilot is built
    /// holding for a lost controller at an unknown arm state.
    fn case_inputs(i: &Value) -> (Vec<BaseAlert>, Option<NavPill>, Vec<(String, String)>) {
        let alerts = i["alerts"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|a| BaseAlert {
                id: a["id"].as_str().unwrap_or_default().into(),
                level: Level::from_name(a["level"].as_str().unwrap_or_default())
                    .expect("a known level"),
                title: a["title"].as_str().unwrap_or_default().into(),
            })
            .collect();
        let nav = i
            .get("nav_source")
            .filter(|n| !n.is_null())
            .map(|n| NavPill {
                kind: NavKind::from_name(n["kind"].as_str().unwrap_or_default()).expect("a kind"),
                tone: NavTone::from_name(n["tone"].as_str().unwrap_or_default()).expect("a tone"),
            });
        let preflight = i["preflight_failing"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|p| {
                let s = |k: &str| p[k].as_str().unwrap_or_default().to_string();
                (s("id"), s("label"))
            })
            .collect();
        (alerts, nav, preflight)
    }

    #[test]
    fn pill_cases_match_the_shared_rules() {
        let cases: Vec<Value> = serde_json::from_str(include_str!(
            "../../../../web-shared/src/hud/fixtures/pill-cases.json"
        ))
        .expect("pill-cases.json");
        let mut ran = 0;
        for c in &cases {
            let i = &c["input"];
            let p = Pilot {
                hold_reason: (i["controller_lost"] == Value::Bool(true))
                    .then(|| "controller-lost".to_string()),
                readiness: i["readiness"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|r| ReadinessItem {
                        id: r["id"].as_str().unwrap_or_default().into(),
                        ok: r["ok"].as_bool() == Some(true),
                        block: r["severity"].as_str() == Some("block"),
                        msg: r["msg"].as_str().unwrap_or_default().into(),
                    })
                    .collect(),
                statustext: i["fc_status"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|s| {
                        st(
                            s["severity"].as_f64().unwrap_or(6.0),
                            s["text"].as_str().unwrap_or_default(),
                        )
                    })
                    .collect(),
                age_ms: i["pilot_age_ms"].as_u64(),
                fc_failsafe: i["fc_failsafe"].as_bool() == Some(true),
                failsafe_cause: i["failsafe_cause"].as_str().map(String::from),
                ..Pilot::default()
            };
            let prearm: Option<Vec<String>> = i["prearm"].as_array().map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(String::from)
                    .collect()
            });
            let (base, nav_source, preflight) = case_inputs(i);
            let alerts = pill_alerts(&PillInputs {
                alerts: &base,
                pilot: Some(&p),
                pilot_age_ms: p.age_ms,
                prearm: prearm.as_deref(),
                storage_pct: i["storage_pct"].as_f64(),
                agent_unreachable: i["agent_reachable"] == Value::Bool(false),
                arm_unknown: i.get("armed").is_some_and(Value::is_null),
                preflight_failing: &preflight,
                nav_source,
            });
            let severity = if alerts.is_empty() {
                "ok"
            } else if alerts.iter().any(|a| a.severity == Severity::Critical) {
                "critical"
            } else {
                "warning"
            };
            let text = match alerts.as_slice() {
                [] => spec_text("cockpit.status.ok", &[]),
                [one] => one.text.clone(),
                many => alerts_count_text(many.len()).unwrap_or_default(),
            };
            let ids: Vec<&str> = alerts.iter().map(|a| a.id.as_str()).collect();
            let e = &c["expected"];
            let name = c["name"].as_str().unwrap_or_default();
            assert_eq!(severity, e["severity"], "{name}: severity");
            assert_eq!(text, e["text"], "{name}: text");
            assert_eq!(
                alerts.len() as u64,
                e["count"].as_u64().unwrap_or(0),
                "{name}: count"
            );
            let want: Vec<&str> = e["ids"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .collect();
            assert_eq!(ids, want, "{name}: ids");
            ran += 1;
        }
        assert!(
            cases
                .iter()
                .any(|c| c["name"] == "fc_failsafe_fresh_is_critical"),
            "the failsafe case is in the file"
        );
        assert_eq!(ran, cases.len(), "every shared case runs");
    }

    #[test]
    fn an_unknown_arm_state_with_a_lost_controller_alerts() {
        let p = Pilot {
            hold_reason: Some("controller-lost".into()),
            ..fresh(vec![], vec![])
        };
        assert!(controller_lost(&p), "unknown arm may be airborne");
        let landed = Pilot {
            landed: Some(true),
            ..p.clone()
        };
        assert!(!controller_lost(&landed));
        let disarmed = Pilot {
            armed: Some(false),
            ..p
        };
        assert!(!controller_lost(&disarmed));
    }
}
