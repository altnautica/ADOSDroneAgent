use super::words::fmt_eta;
use super::*;
use crate::model::autonomy::Autonomy;
use crate::model::spec_text::spec_text;

/// The shared golden chip cases, read in place.
const CHIP_CASES: &str = include_str!("../../../../../web-shared/src/hud/fixtures/chip-cases.json");

fn pilot_of(wire: &Value) -> Pilot {
    Autonomy::from_value(&serde_json::json!({"pilot": {"ts_ms": 1, "age_ms": 0, "data": wire}}))
        .pilot
        .expect("the case's pilot parses")
}

#[test]
fn chip_cases() {
    let cases: Vec<Value> = serde_json::from_str(CHIP_CASES).expect("chip cases are JSON");
    assert!(cases.len() >= 30, "the golden file lost its cases");
    let mut covered = std::collections::BTreeSet::new();
    for c in &cases {
        let name = c["name"].as_str().unwrap_or("?");
        let i = &c["input"];
        let ctx = ChipContext {
            home_dist_m: i["home_dist_m"].as_f64(),
            local_pos: i["local_pos"]["x"]
                .as_f64()
                .zip(i["local_pos"]["y"].as_f64()),
            eta_s: i["ar_eta_s"].as_f64(),
            mode_detail: i["mode_detail"].as_str().map(str::to_string),
            complete_age_ms: i["complete_age_ms"].as_f64(),
        };
        let age = i["pilot_age_ms"].as_u64().expect("pilot_age_ms");
        let facts = chip_inputs(&pilot_of(&i["pilot"]), age, &ctx)
            .unwrap_or_else(|| panic!("{name}: no chip facts"));
        let v = flight_state_chip(&facts);
        let e = &c["expected"];
        assert_eq!(v.rule_id, e["rule"].as_str().unwrap(), "{name}: rule");
        assert_eq!(v.text, e["text"].as_str().unwrap(), "{name}: text");
        assert_eq!(v.color, e["color"].as_str().unwrap(), "{name}: color");
        assert_eq!(v.dim, e["dim"].as_bool().unwrap(), "{name}: dim");
        assert_eq!(v.button, e["button"].as_str(), "{name}: button");
        let secondary: Vec<&str> = v.secondary.iter().map(|s| s.text.as_str()).collect();
        let want: Vec<&str> = e["secondary"]
            .as_array()
            .expect("secondary")
            .iter()
            .filter_map(Value::as_str)
            .collect();
        assert_eq!(secondary, want, "{name}: secondary");
        covered.insert(v.rule_id);
    }
    for rule in &SPEC.rules {
        assert!(
            covered.contains(&rule.id),
            "no golden case for rule {}",
            rule.id
        );
    }
}

#[test]
fn the_stale_threshold_is_the_spec_value() {
    // A literal, so a changed spec value fails here.
    assert_eq!(stale_after_ms(), 2000);
}

#[test]
fn the_chip_reads_the_spec_threshold_as_stale() {
    let frame = serde_json::json!({"owner": "idle", "readiness": []});
    let rule_at = |age: u64| {
        let facts = chip_inputs(&pilot_of(&frame), age, &ChipContext::default()).unwrap();
        flight_state_chip(&facts).rule_id
    };
    assert_ne!(rule_at(1999), "link_stale", "1999 ms is fresh");
    assert_eq!(rule_at(2000), "link_stale", "2000 ms is stale");
}

#[test]
fn eta_reads_seconds_then_minutes() {
    assert_eq!(fmt_eta(Some(37.0)), "ETA 37 s");
    assert_eq!(fmt_eta(Some(65.0)), "ETA 1:05");
    assert_eq!(fmt_eta(None), "");
    // The total rounds first: 119.6 s reads 2:00, never 1:60.
    assert_eq!(fmt_eta(Some(119.6)), "ETA 2:00");
}

#[test]
fn the_launching_failsafe_and_paused_texts_are_the_spec_strings() {
    let rule = SPEC.rules.iter().find(|r| r.id == "launching").unwrap();
    assert_eq!(
        rule.text,
        spec_text("chip.taking_off", &[("step", "{step}")])
    );
    assert_eq!(
        SPEC.secondary.fc_failsafe.text,
        spec_text("chip.failsafe", &[])
    );
    assert_eq!(
        SPEC.secondary.mission_paused_line.text,
        spec_text(
            "chip.mission_paused_line",
            &[("seq", "{seq}"), ("total", "{total}")]
        )
    );
}
