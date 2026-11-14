use std::collections::HashMap;

use super::*;
use crate::config::{FlowMount, RangefinderMount};

fn good() -> HashMap<String, f32> {
    [
        ("FLOW_TYPE", 5.0),
        ("RNGFND1_TYPE", 10.0),
        ("RNGFND1_ORIENT", 25.0),
        ("FLOW_ORIENT_YAW", 0.0),
        ("FLOW_FXSCALER", 0.0),
        ("FLOW_FYSCALER", 0.0),
        ("FLOW_POS_X", 0.05),
        ("FLOW_POS_Y", 0.0),
        ("FLOW_POS_Z", 0.02),
        ("RNGFND1_POS_X", 0.05),
        ("RNGFND1_POS_Y", 0.0),
        ("RNGFND1_POS_Z", 0.03),
        ("EK3_FLOW_DELAY", 10.0),
        ("RNGFND1_MAX", 8.0),
        ("EK3_SRC_OPTIONS", 0.0),
        ("PRX1_TYPE", 0.0),
        ("PRX_IGN_GND", 0.0),
        ("SERIAL3_PROTOCOL", 2.0),
        ("SERIAL3_OPTIONS", 1024.0),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_owned(), v))
    .collect()
}

fn run(p: &HashMap<String, f32>, m: &FcSensors) -> Vec<String> {
    check(|n| p.get(n).copied(), m)
        .into_iter()
        .map(|f| f.detail)
        .collect()
}

fn with(changes: &[(&str, f32)]) -> HashMap<String, f32> {
    let mut p = good();
    for (k, v) in changes {
        p.insert((*k).to_owned(), *v);
    }
    p
}

#[test]
fn a_matching_table_passes() {
    assert!(run(&good(), &FcSensors::default()).is_empty());
}

#[test]
fn eq_range_and_max() {
    let p = with(&[
        ("FLOW_TYPE", 0.0),
        ("FLOW_FXSCALER", 900.0),
        ("EK3_FLOW_DELAY", 0.0),
        ("RNGFND1_MAX", 20.0),
    ]);
    assert_eq!(
        run(&p, &FcSensors::default()),
        [
            "FLOW_TYPE 0 (want 5)",
            "FLOW_FXSCALER 900 (want -800 to 800)",
            "EK3_FLOW_DELAY 0 (want 1 to 250)",
            "RNGFND1_MAX 20 (want at most 12)",
        ]
    );
    let sev: Vec<_> = check(|n| p.get(n).copied(), &FcSensors::default())
        .into_iter()
        .map(|f| f.severity)
        .collect();
    assert_eq!(
        sev,
        [
            Severity::Block,
            Severity::Warn,
            Severity::Warn,
            Severity::Warn
        ]
    );
}

#[test]
fn undeclared_yaw_must_be_a_right_angle() {
    let none = FcSensors::default();
    assert_eq!(
        run(&with(&[("FLOW_ORIENT_YAW", 4500.0)]), &none),
        ["FLOW_ORIENT_YAW 4500 (want multiple of 9000)"]
    );
    assert!(run(&with(&[("FLOW_ORIENT_YAW", -9000.0)]), &none).is_empty());
    assert_eq!(
        run(&with(&[("FLOW_ORIENT_YAW", -18000.0)]), &none),
        ["FLOW_ORIENT_YAW -18000 (want -17999 to 18000)"]
    );
}

#[test]
fn a_declared_mount_is_compared_exactly() {
    let m = FcSensors {
        flow: Some(FlowMount {
            pos_m: Some([0.05, 0.0, 0.025]),
            orient_yaw_cdeg: Some(9000),
        }),
        rangefinder: Some(RangefinderMount {
            pos_m: Some([0.10, 0.0, 0.03]),
        }),
    };
    // Yaw 0 is a right angle but not the declared 9000; FLOW_POS_Z is within
    // the tolerance; RNGFND1_POS_X is 5 cm off.
    assert_eq!(
        run(&good(), &m),
        [
            "FLOW_ORIENT_YAW 0 (want 9000)",
            "RNGFND1_POS_X 0.05 (want 0.1)"
        ]
    );
    let p = with(&[("FLOW_ORIENT_YAW", 9000.0), ("RNGFND1_POS_X", 0.1)]);
    assert!(run(&p, &m).is_empty());
}

#[test]
fn an_all_zero_lever_arm_is_reported_once_per_group() {
    let p = with(&[
        ("FLOW_POS_X", 0.0),
        ("FLOW_POS_Z", 0.0),
        ("RNGFND1_POS_X", 0.0),
        ("RNGFND1_POS_Z", 0.0),
    ]);
    assert_eq!(
        run(&p, &FcSensors::default()),
        [
            "FLOW_POS all 0 (want the lever arm from the CG)",
            "RNGFND1_POS all 0 (want the lever arm from the CG)",
        ]
    );
    // One non-zero member passes the group.
    let p = with(&[("FLOW_POS_X", 0.0), ("FLOW_POS_Z", 0.0)]);
    assert_eq!(
        run(&p, &FcSensors::default()),
        ["FLOW_POS all 0 (want the lever arm from the CG)"]
    );
    // A declared flow mount replaces the group rule for the flow rows.
    let m = FcSensors {
        flow: Some(FlowMount {
            pos_m: Some([0.0; 3]),
            orient_yaw_cdeg: None,
        }),
        rangefinder: None,
    };
    let p = with(&[("FLOW_POS_X", 0.0), ("FLOW_POS_Z", 0.0)]);
    assert!(run(&p, &m).is_empty());
}

#[test]
fn when_gates_a_row_on_other_parameters() {
    let p = with(&[("PRX1_TYPE", 4.0)]);
    assert_eq!(run(&p, &FcSensors::default()), ["PRX_IGN_GND 0 (want 1)"]);
    let p = with(&[("PRX1_TYPE", 4.0), ("RNGFND1_ORIENT", 0.0)]);
    assert_eq!(
        run(&p, &FcSensors::default()),
        ["RNGFND1_ORIENT 0 (want 25)"]
    );
}

#[test]
fn some_port_must_stop_forwarding() {
    let no = "SERIALn_OPTIONS no port with 1024 set, nor MAVn_OPTIONS 2 \
              (want the flow port's forwarding off)";
    let mut p = with(&[("SERIAL3_OPTIONS", 0.0)]);
    assert_eq!(run(&p, &FcSensors::default()), [no]);
    // The option on a port that does not speak MAVLink does not count.
    p.insert("SERIAL4_PROTOCOL".into(), 5.0);
    p.insert("SERIAL4_OPTIONS".into(), 1024.0);
    assert_eq!(run(&p, &FcSensors::default()), [no]);
    // MAVn_OPTIONS on 4.7 and later.
    p.insert("MAV5_OPTIONS".into(), 2.0);
    assert!(run(&p, &FcSensors::default()).is_empty());
    // No SERIAL or MAV parameters read: skipped.
    let p: HashMap<_, _> = good()
        .into_iter()
        .filter(|(k, _)| !k.starts_with("SERIAL"))
        .collect();
    assert!(run(&p, &FcSensors::default()).is_empty());
}

#[test]
fn absent_rows_are_skipped() {
    let p: HashMap<String, f32> = [("FLOW_TYPE".to_owned(), 5.0)].into_iter().collect();
    assert!(run(&p, &FcSensors::default()).is_empty());
}

#[test]
fn every_rule_parameter_is_requested() {
    let n = param_names();
    for want in [
        "FLOW_TYPE",
        "FLOW_POS_Z",
        "PRX1_TYPE",
        "PRX_IGN_GND",
        "EK3_SRC3_VELXY",
        "SERIAL1_PROTOCOL",
        "SERIAL9_OPTIONS",
        "MAV1_OPTIONS",
        "MAV9_OPTIONS",
    ] {
        assert!(n.iter().any(|x| x == want), "{want} missing from {n:?}");
    }
    assert!(!n
        .iter()
        .any(|x| x.contains("{n}") || x == "SERIALn_OPTIONS"));
}

/// Every parameter an ArduCopter SITL holds after boot, one name per line.
const SITL_PARAMS: &str = include_str!("fixtures/arducopter-x500-ados-params.txt");

#[test]
fn every_rule_parameter_is_a_real_ardupilot_name() {
    let real: std::collections::HashSet<&str> = SITL_PARAMS
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .collect();
    // The per-port families depend on the board's port count and firmware
    // version (MAVn_OPTIONS is 4.7 and later), so only their first is checked.
    let per_port = |x: &str| {
        (x.starts_with("SERIAL") || x.starts_with("MAV"))
            && !matches!(x, "SERIAL1_PROTOCOL" | "SERIAL1_OPTIONS" | "MAV1_OPTIONS")
    };
    let unknown: Vec<&String> = param_names()
        .iter()
        .filter(|x| !per_port(x) && !real.contains(x.as_str()))
        .collect();
    assert!(unknown.is_empty(), "not ArduPilot parameters: {unknown:?}");
}
