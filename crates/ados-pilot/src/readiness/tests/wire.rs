//! The structured detail of a readiness item: its parameter rows and fix.

use ados_protocol::pilot::{ParamDiff, ReadinessFix};
use serde_json::json;

use super::{copter, profile};

#[test]
fn a_differing_profile_row_carries_its_parameter_and_the_profile_fix() {
    let mut fc = copter();
    fc.params.insert("MAV_GCS_SYSID".into(), 255.0);
    let r = profile(&fc);
    assert!(!r.ok);
    assert!(r.msg.starts_with("FC parameters differ: "), "{}", r.msg);
    assert!(r.params.contains(&ParamDiff {
        name: "MAV_GCS_SYSID".into(),
        have: Some(255.0),
        want: "1".into(),
    }));
    assert_eq!(r.fix, Some(ReadinessFix::ApplyProfile));
    println!("{}", serde_json::to_string(&r).unwrap());
    let v = serde_json::to_value(&r).unwrap();
    assert_eq!(v["fix"], json!({"kind": "apply_profile"}));
    assert_eq!(
        v["params"],
        json!([{"name": "MAV_GCS_SYSID", "have": 255.0, "want": "1"}])
    );
}

#[test]
fn an_ok_item_serialises_neither_params_nor_fix() {
    let r = profile(&copter());
    assert!(r.ok, "{r:?}");
    let v = serde_json::to_value(&r).unwrap();
    let mut keys: Vec<&String> = v.as_object().unwrap().keys().collect();
    keys.sort();
    assert_eq!(keys, ["id", "msg", "ok", "severity"]);
}

#[test]
fn every_fix_kind_has_its_wire_shape() {
    let cases = [
        (ReadinessFix::ApplyProfile, json!({"kind": "apply_profile"})),
        (
            ReadinessFix::SetupStep {
                step: "detect".into(),
            },
            json!({"kind": "setup_step", "step": "detect"}),
        ),
        (
            ReadinessFix::Settings {
                section: "sensors".into(),
            },
            json!({"kind": "settings", "section": "sensors"}),
        ),
        (
            ReadinessFix::Parameters {
                names: vec!["FLOW_TYPE".into()],
            },
            json!({"kind": "parameters", "names": ["FLOW_TYPE"]}),
        ),
    ];
    for (fix, wire) in cases {
        assert_eq!(serde_json::to_value(&fix).unwrap(), wire);
        assert_eq!(serde_json::from_value::<ReadinessFix>(wire).unwrap(), fix);
    }
}
