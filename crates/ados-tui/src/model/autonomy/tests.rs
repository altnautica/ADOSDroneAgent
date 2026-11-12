use super::*;
use serde_json::json;

#[test]
fn every_kind_null_reads_as_nothing_published() {
    let a = Autonomy::from_value(
        &json!({"pose": null, "scan": null, "nav": null, "follow": null, "map": null}),
    );
    assert_eq!(a, Autonomy::default());
    assert!(!a.any());
}

#[test]
fn kinds_are_read_with_their_age() {
    let a = Autonomy::from_value(&json!({
        "follow": {"ts_ms": 1, "age_ms": 40, "data": {
            "phase": "coasting", "track_id": 12, "range_m": 10.2, "range_sigma_m": 0.6,
            "reacquire_mode": "smart", "coast_remaining_s": 1.4}},
        "nav": {"ts_ms": 1, "age_ms": 9000, "data": {
            "mode": "external_nav", "active_set": 1, "extnav_health": "good",
            "odom_rate_hz": 30.0, "obstacle_rate_hz": 10.0, "min_obstacle_m": 3.2, "gps_fix": 0}},
        "scan": {"ts_ms": 1, "age_ms": 50, "data": {"ranges_m": [0.0, 4.5, 2.25, 0.0]}},
        "map": {"ts_ms": 1, "age_ms": 500, "data": {
            "placement": "hybrid", "sweep_points": 120000, "area_m2": 850.5, "voxel_m": 0.1,
            "memory_used_mb": 210, "memory_budget_mb": 614, "keyframes_dropped": 3}},
    }));
    let follow = a.follow.unwrap();
    assert_eq!(follow.phase.as_deref(), Some("coasting"));
    assert_eq!(follow.track_id, Some(12));
    assert!(!follow.stale);
    assert_eq!(follow.age_ms, Some(40));
    let nav = a.nav.unwrap();
    assert_eq!(nav.active_set, Some(1));
    assert!(nav.stale, "9 s old is stale");
    assert_eq!(a.lidar.unwrap().min_range_m, Some(2.25));
    let map = a.map.unwrap();
    assert_eq!(map.sweep_points, Some(120000));
    assert_eq!(map.memory_budget_mb, Some(614.0));
}

#[test]
fn a_scan_with_no_return_has_no_nearest_range() {
    let a = Autonomy::from_value(
        &json!({"scan": {"ts_ms": 1, "age_ms": 1, "data": {"ranges_m": [0.0, 0.0]}}}),
    );
    assert_eq!(a.lidar.unwrap().min_range_m, None);
}

/// The shared golden pilot frame, read from the control crate's fixture.
pub(crate) fn pilot_frame() -> Value {
    serde_json::from_str(include_str!(
        "../../../../ados-control/tests/fixtures/pilot-frame.json"
    ))
    .expect("pilot frame fixture is JSON")
}

#[test]
fn the_pilot_kind_reads_the_golden_frame() {
    let a =
        Autonomy::from_value(&json!({"pilot": {"ts_ms": 1, "age_ms": 100, "data": pilot_frame()}}));
    assert!(a.any());
    let p = a.pilot.unwrap();
    assert_eq!(p.owner.as_deref(), Some("skill"));
    assert_eq!(p.skill_label.as_deref(), Some("Follow"));
    assert_eq!(p.control_mode.as_deref(), Some("ados"));
    assert_eq!(p.profile.as_deref(), Some("normal"));
    assert_eq!(p.avoid.as_deref(), Some("brake"));
    assert_eq!(p.avoid_state.as_deref(), Some("detour"));
    assert_eq!(p.hold_reason, None);
    assert_eq!(p.min_clearance_m, Some(2.4));
    assert_eq!(p.fc_mode, Some(4.0));
    assert_eq!(p.fc_mode_name.as_deref(), Some("GUIDED"));
    assert_eq!(p.vehicle_class.as_deref(), Some("multirotor"));
    assert_eq!(p.goal_ned, Some([12.0, -3.5, -4.0]));
    assert_eq!(p.readiness.len(), 3);
    assert!(!p.readiness[2].ok && !p.readiness[2].block);
    assert_eq!(p.link_loss_in_s, None);
    assert_eq!(p.link_loss_action.as_deref(), Some("rth"));
    assert_eq!(p.agl_m, Some(4.2));
    assert_eq!(p.agl_source.as_deref(), Some("rangefinder"));
    assert_eq!(
        p.statustext,
        vec![FcStatusText {
            severity: 4.0,
            text: "PreArm: Compass not calibrated".into()
        }]
    );
    assert_eq!(p.setup_step, None);
    assert_eq!(
        p.mission,
        Some(PilotMission {
            state: Some("none".into()),
            seq: Some(0.0),
            total: Some(0.0),
            landing_seq: false,
        })
    );
    assert_eq!(p.age_ms, Some(100));
    assert!(!p.stale);
}

#[test]
fn an_unknown_owner_or_enum_value_reads_as_absent() {
    let mut frame = pilot_frame();
    frame["owner"] = json!("warp");
    frame["profile"] = json!("ludicrous");
    frame["link_loss_action"] = json!("teleport");
    let p = Autonomy::from_value(&json!({"pilot": {"ts_ms": 1, "age_ms": 1, "data": frame}}))
        .pilot
        .unwrap();
    assert_eq!(p.owner, None);
    assert_eq!(p.profile, None);
    assert_eq!(p.link_loss_action, None);
}

#[test]
fn an_old_pilot_kind_is_stale_and_a_null_one_is_absent() {
    let old = Autonomy::from_value(
        &json!({"pilot": {"ts_ms": 1, "age_ms": 4000, "data": pilot_frame()}}),
    );
    assert!(old.pilot.unwrap().stale);
    assert_eq!(Autonomy::from_value(&json!({"pilot": null})).pilot, None);
}

#[test]
fn a_kind_at_the_spec_threshold_is_stale() {
    let at = |age: u64| {
        Autonomy::from_value(&json!({"pilot": {"ts_ms": 1, "age_ms": age, "data": pilot_frame()}}))
            .pilot
            .unwrap()
            .stale
    };
    assert_eq!(stale_after_ms(), 2000);
    assert!(!at(1999), "1999 ms is fresh");
    assert!(at(2000), "2000 ms is stale: the threshold itself is stale");
    assert!(at(2500), "2.5 s is past the spec's 2 s");
}

#[test]
fn a_kind_without_a_whole_millisecond_age_is_stale() {
    for entry in [
        json!({"ts_ms": 1, "data": {"phase": "following"}}),
        json!({"ts_ms": 1, "age_ms": null, "data": {"phase": "following"}}),
        json!({"ts_ms": 1, "age_ms": "40", "data": {"phase": "following"}}),
        json!({"ts_ms": 1, "age_ms": -5, "data": {"phase": "following"}}),
        json!({"ts_ms": 1, "age_ms": 40.5, "data": {"phase": "following"}}),
    ] {
        let a = Autonomy::from_value(&json!({ "follow": entry }));
        assert!(a.follow.unwrap().stale, "{entry}");
    }
}

#[test]
fn mark_stale_marks_every_kind() {
    let mut a = Autonomy::from_value(&json!({
        "pilot": {"ts_ms": 1, "age_ms": 10, "data": pilot_frame()},
        "follow": {"ts_ms": 1, "age_ms": 10, "data": {}},
        "nav": {"ts_ms": 1, "age_ms": 10, "data": {}},
        "scan": {"ts_ms": 1, "age_ms": 10, "data": {}},
        "map": {"ts_ms": 1, "age_ms": 10, "data": {}},
    }));
    a.mark_stale();
    assert!(a.pilot.unwrap().stale);
    assert!(a.follow.unwrap().stale);
    assert!(a.nav.unwrap().stale);
    assert!(a.lidar.unwrap().stale);
    assert!(a.map.unwrap().stale);
}

#[test]
fn the_ar_kind_carries_the_next_waypoints_eta_while_fresh() {
    let ar: Value = serde_json::from_str(include_str!(
        "../../../../ados-control/tests/fixtures/ar-frame.json"
    ))
    .unwrap();
    let eta = |age: u64| {
        Autonomy::from_value(&json!({"ar": {"ts_ms": 1, "age_ms": age, "data": ar}})).ar_eta_s
    };
    assert_eq!(eta(100), Some(42.0));
    assert_eq!(eta(AR_ETA_MAX_AGE_MS + 1), None);
}

#[test]
fn tones_cover_the_health_words() {
    assert_eq!(health_tone("degraded"), Tone::Caution);
}
