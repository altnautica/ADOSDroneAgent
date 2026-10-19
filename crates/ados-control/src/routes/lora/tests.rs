use super::*;

fn body(v: Value) -> Map<String, Value> {
    v.as_object().unwrap().clone()
}

#[test]
fn an_absent_lane_reads_present_false() {
    let dir = tempfile::tempdir().unwrap();
    let b = status_body(dir.path(), SystemTime::now());
    assert_eq!(b["present"], false);
    assert_eq!(b["tx_allowed"], false);
    assert_eq!(
        beacons_body(dir.path(), SystemTime::now()),
        json!({"beacons": []})
    );
}

#[test]
fn a_fresh_sidecar_is_served_with_present_true_and_without_its_version() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("lora.json"),
        r#"{"v":1,"transport":"e22","module_band":"850-930","tx_allowed":true,"channel":{"bw_khz":125}}"#,
    )
    .unwrap();
    std::fs::write(
        dir.path().join("lora-beacons.json"),
        r#"{"v":1,"beacons":[{"node_id":42,"kind":"beacon"}]}"#,
    )
    .unwrap();
    let now = SystemTime::now();
    let b = status_body(dir.path(), now);
    assert_eq!(b["present"], true);
    assert!(b.get("v").is_none());
    assert_eq!(b["module_band"], "850-930");
    assert_eq!(beacons_body(dir.path(), now)["beacons"][0]["node_id"], 42);
    // The regions table is flagged against the fitted E22-900.
    let r = regions_response(dir.path(), &dir.path().join("config.yaml"), now);
    let row = |code: &str| {
        r["regions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|x| x["code"] == code)
            .unwrap()
            .clone()
    };
    assert_eq!(row("IN")["supported_by_module"], true);
    assert_eq!(row("CN")["supported_by_module"], false);
}

#[test]
fn a_stale_sidecar_reads_as_absent() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("lora.json"), r#"{"v":1,"tx_allowed":true}"#).unwrap();
    let later = SystemTime::now() + Duration::from_secs(60);
    assert_eq!(status_body(dir.path(), later)["present"], false);
}

#[test]
fn put_validates_against_the_plan() {
    let cfg = LoraConfig::default();
    let e22 = ModuleCaps::e22_900();
    // India clamp is 25 mW ERP.
    assert!(plan_change(&body(json!({"erp_mw": 26.0})), &cfg, Some("IN"), Some(&e22)).is_err());
    let ok = plan_change(&body(json!({"erp_mw": 14.0})), &cfg, Some("IN"), Some(&e22)).unwrap();
    assert_eq!(ok, vec![("erp_mw", json!(14.0))]);
    // Off-plan frequency.
    assert!(plan_change(
        &body(json!({"freq_mhz": 868.125})),
        &cfg,
        Some("IN"),
        Some(&e22)
    )
    .is_err());
    // Unknown region code.
    assert!(plan_change(&body(json!({"region": "XX"})), &cfg, None, None).is_err());
    // Region set (uppercased) and back to following the country.
    let set = plan_change(&body(json!({"region": "eu"})), &cfg, Some("IN"), Some(&e22)).unwrap();
    assert_eq!(set, vec![("region", json!("EU"))]);
    let follow = plan_change(&body(json!({"region": null})), &cfg, None, None).unwrap();
    assert_eq!(follow, vec![("region", Value::Null)]);
    // A radio value with no resolvable region.
    assert!(plan_change(&body(json!({"sf": 9})), &cfg, None, None).is_err());
}

#[test]
fn persist_writes_the_lora_block_and_reloads_the_lane() {
    let dir = tempfile::tempdir().unwrap();
    let cfg_path = dir.path().join("config.yaml");
    std::fs::write(&cfg_path, "lora:\n  enabled: true\n").unwrap();
    let cmds = persist(
        &cfg_path,
        vec![("region", json!("EU")), ("erp_mw", json!(14.0))],
    )
    .unwrap();
    let text = std::fs::read_to_string(&cfg_path).unwrap();
    assert!(text.contains("region: EU"), "{text}");
    assert!(text.contains("erp_mw: 14"), "{text}");
    assert_eq!(
        cmds,
        vec![vec![
            "systemctl".to_string(),
            "--no-block".into(),
            "reload-or-restart".into(),
            "ados-lora.service".into()
        ]]
    );
}

#[test]
fn the_capability_flag_needs_the_profile_and_the_marker() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("config.yaml");
    assert!(!lora_present(&cfg, "ground-station"));
    std::fs::write(dir.path().join("lora-enabled"), b"").unwrap();
    assert!(lora_present(&cfg, "ground-station"));
    assert!(lora_present(&cfg, "drone"));
    assert!(!lora_present(&cfg, "visor"));
}
