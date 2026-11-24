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
        params: Vec::new(),
        fix: None,
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
            unverified: None,
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

/// An alert's section, snooze key and readiness detail, as a golden case's
/// `expected.detail` lists them.
#[derive(Debug, PartialEq)]
struct Detail {
    id: String,
    group: String,
    key: String,
    params: Vec<ParamDiff>,
    fix: Option<ReadinessFix>,
}

impl Detail {
    fn of(a: &PillAlert) -> Self {
        Self {
            id: a.id.clone(),
            group: a.group.name().into(),
            key: a.key.clone(),
            params: a.params.clone(),
            fix: a.fix.clone(),
        }
    }

    fn from_case(d: &Value) -> Self {
        let s = |k: &str| d[k].as_str().expect("a string field").to_string();
        Self {
            id: s("id"),
            group: s("group"),
            key: s("key"),
            params: serde_json::from_value(d["params"].clone()).unwrap_or_default(),
            fix: serde_json::from_value(d["fix"].clone()).ok(),
        }
    }
}

#[test]
fn pill_cases_match_the_shared_rules() {
    let cases: Vec<Value> = serde_json::from_str(include_str!(
        "../../../../../web-shared/src/hud/fixtures/pill-cases.json"
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
                    params: serde_json::from_value(r["params"].clone()).unwrap_or_default(),
                    fix: serde_json::from_value(r["fix"].clone()).ok(),
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
            setup_abort: i["setup_abort"].as_str().map(String::from),
            low_battery_action: i["low_battery_action"].as_str().map(String::from),
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
        let groups: Vec<&str> = group_alerts(&alerts)
            .iter()
            .map(|s| s.group.name())
            .collect();
        let want: Vec<&str> = e["groups"]
            .as_array()
            .expect("every case lists its groups")
            .iter()
            .filter_map(Value::as_str)
            .collect();
        assert_eq!(groups, want, "{name}: groups");
        if let Some(detail) = e["detail"].as_array() {
            let got: Vec<Detail> = alerts.iter().map(Detail::of).collect();
            let want: Vec<Detail> = detail.iter().map(Detail::from_case).collect();
            assert_eq!(got, want, "{name}: detail");
        }
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
fn sections_follow_the_spec_order_and_fold_one_text_to_its_worst() {
    let p = fresh(
        vec![
            item("flow", false, false, "No optical flow"),
            item("fc_profile", false, true, "FC parameters differ"),
            item("mystery", false, false, "Something else"),
        ],
        vec![st(4.0, "EKF variance")],
    );
    let alerts = pilot_alerts(&p, p.age_ms, Some(&["PreArm: EKF variance".into()]));
    let sections = group_alerts(&alerts);
    let got: Vec<(&str, Vec<&str>)> = sections
        .iter()
        .map(|s| {
            let texts = s.alerts.iter().map(|a| a.text.as_str()).collect();
            (s.title.as_str(), texts)
        })
        .collect();
    assert_eq!(
        got,
        vec![
            ("Flight controller setup", vec!["FC parameters differ"]),
            // The PreArm line and the same status text are one row.
            ("Flight controller messages", vec!["EKF variance"]),
            ("Sensors", vec!["No optical flow"]),
            ("Other", vec!["Something else"]),
        ]
    );
    assert_eq!(sections[0].alerts[0].key, "pilot:fc_profile|FC parameters differ");
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
