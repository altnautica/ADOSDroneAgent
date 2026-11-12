//! The alerts the cockpit's status pill draws from the pilot kind and the
//! flight controller: a pilot state too old to trust, the FC's own failsafe,
//! failing readiness items, the FC's PreArm/Arm texts and its status texts at
//! WARNING or worse. The same rules, ids, texts and order as the shared
//! `status-pill.ts` `pillAlerts` for these sources, held to the golden
//! `pill-cases.json`.

use super::autonomy::Pilot;
use super::flight_state::stale_after_ms;
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

/// Every active alert, critical first, then in source order: the pilot not
/// answering or the FC failsafe, readiness, the newest PreArm/Arm text, the
/// status texts (one per text, at the worst severity it was sent with). A
/// stale pilot state contributes only the not-answering item. `age_ms` is the
/// pilot state's age now (the read's age plus the time since the read), so a
/// hung poll ages the alerts as it ages the rows.
pub fn pilot_alerts(p: &Pilot, age_ms: Option<u64>, prearm: Option<&[String]>) -> Vec<PillAlert> {
    let mut out = Vec::new();
    let stale = pilot_stale(p, age_ms);
    if stale {
        out.push(PillAlert {
            id: "pilot_not_answering".into(),
            severity: Severity::Warning,
            text: spec_text("alert.pilot_not_answering", &[]),
        });
    } else if p.fc_failsafe {
        out.push(PillAlert {
            id: "failsafe".into(),
            severity: Severity::Critical,
            text: failsafe_text(p.failsafe_cause.as_deref()),
        });
    }
    let readiness = if stale { &[][..] } else { &p.readiness[..] };
    for r in readiness.iter().filter(|r| !r.ok) {
        // A blocking item refuses take-off: critical, as the pre-flight sheet fails it.
        out.push(PillAlert {
            id: format!("pilot:{}", r.id),
            severity: if r.block {
                Severity::Critical
            } else {
                Severity::Warning
            },
            text: if r.msg.is_empty() {
                r.id.clone()
            } else {
                r.msg.clone()
            },
        });
    }
    if let Some(last) = prearm.and_then(<[String]>::last) {
        let text = prearm_body(last).unwrap_or(last);
        out.push(PillAlert {
            id: format!("fc:{text}"),
            severity: Severity::Warning,
            text: spec_text("flow.preflight.fc_prearm.failing", &[("text", text)]),
        });
    }
    let fc_start = out.len();
    let fc_status = if stale { &[][..] } else { &p.statustext[..] };
    for s in fc_status {
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
            None => out.push(PillAlert {
                id: format!("fcst:{}", s.text),
                severity,
                text: s.text.clone(),
            }),
        }
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
                (Severity::Warning, "Flight controller: Throttle too high"),
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

    /// Whether a golden case uses only the sources this port models (the
    /// pilot kind and the FC's PreArm texts): the terminal has no surface
    /// alerts, storage, agent-reachability, controller, checklist or
    /// navigation-source rules. An `armed` input only hides checklist items,
    /// so a case without them reads the same whatever it says.
    fn pilot_sources_only(i: &Value) -> bool {
        let empty = |k: &str| i[k].as_array().is_none_or(Vec::is_empty);
        empty("alerts")
            && empty("preflight_failing")
            && i["agent_reachable"] != Value::Bool(false)
            && i["controller_lost"] != Value::Bool(true)
            && i["storage_pct"].as_f64().is_none_or(|s| s < 90.0)
            && i["nav_source"].is_null()
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
            if !pilot_sources_only(i) {
                continue;
            }
            let p = Pilot {
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
            let alerts = pilot_alerts(&p, p.age_ms, prearm.as_deref());
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
        assert!(ran >= 10, "only {ran} pill cases use the pilot sources");
    }
}
