//! Navigation sensor health on the pilot kind: flow and range from the FC,
//! the lidar from the capture service, and the in-flight flow warning.

use ados_protocol::mavlink::ardupilotmega::*;
use ados_protocol::pilot::{FlowState, LidarState, RangeState};
use serde_json::json;

use super::harness::*;
use crate::modes::copter;
use crate::sensor_spec::spec;

fn flow(quality: u8) -> MavMessage {
    MavMessage::OPTICAL_FLOW(OPTICAL_FLOW_DATA {
        quality,
        ..Default::default()
    })
}

/// Run `ms` with one OPTICAL_FLOW per tick from `sysid`.
fn fly_with_flow(w: &mut World, sysid: u8, quality: u8, ms: u64) {
    let end = w.now + ms;
    while w.now < end {
        w.p.on_fc(sysid, FC_COMP, &flow(quality), w.now);
        w.run(20);
    }
}

fn flow_item(w: &World) -> Option<(bool, String)> {
    w.p.state(w.now)
        .readiness
        .into_iter()
        .find(|r| r.id == "flow")
        .map(|r| (r.ok, r.msg))
}

#[test]
fn sensors_ride_the_pilot_kind_while_the_fc_is_linked() {
    let p = crate::core::Pilot::new(crate::config::Settings::default());
    let v = serde_json::to_value(p.state(1000)).unwrap();
    assert_eq!(v.get("sensors"), Some(&serde_json::Value::Null));

    let mut w = World::without_lidar();
    w.agl = Some(0.5);
    fly_with_flow(&mut w, FC_SYS, 180, 400);
    let s = w.p.state(w.now).sensors.expect("sensors with a link");
    assert_eq!(s.flow.state, FlowState::Ok);
    assert_eq!(s.flow.quality, Some(180));
    assert_eq!(s.range.state, RangeState::Ok);
    assert_eq!(s.range.height_m, Some(0.5));
    assert_eq!(s.lidar.state, LidarState::Absent);
    // A reading beyond the sensor's own max.
    w.agl = Some(13.0);
    w.run(40);
    let s = w.p.state(w.now).sensors.unwrap();
    assert_eq!(s.range.state, RangeState::OutOfRange);
}

#[test]
fn the_flow_module_s_own_frames_are_not_the_fc_s_flow() {
    let mut w = World::new();
    fly_with_flow(&mut w, 200, 255, 400);
    let s = w.p.state(w.now).sensors.unwrap();
    assert_eq!(s.flow.state, FlowState::Absent);
    assert_eq!(flow_item(&w), None);
}

#[test]
fn the_lidar_reports_through_scans_and_its_status_topic() {
    let mut w = World::without_lidar();
    let scan = json!({"frame": "body_frd", "angle_start_rad": 0.0,
                      "angle_increment_rad": 0.1, "ranges_m": []});
    w.p.on_feed(
        "plugin.world-engine.lidar",
        &json!({"v": 1, "device": "/dev/ttyUSB0", "connected": true, "health": "good",
                "health_code": 0, "rate_hz": 10.0, "scans": 0, "dropped_scans": 0, "reconnects": 0}),
        w.now,
    );
    for _ in 0..25 {
        w.p.on_feed("plugin.world-engine.scan", &scan, w.now);
        w.run(100);
    }
    let s = w.p.state(w.now).sensors.unwrap();
    assert_eq!(s.lidar.state, LidarState::Ok);
    let rate = s.lidar.rate_hz.unwrap();
    assert!((9.0..=11.0).contains(&rate), "{rate}");
    let item =
        w.p.state(w.now)
            .readiness
            .into_iter()
            .find(|r| r.id == "lidar")
            .unwrap();
    assert!(item.ok, "{item:?}");
    w.run(600);
    assert_eq!(
        w.p.state(w.now).sensors.unwrap().lidar.state,
        LidarState::Lost
    );
}

#[test]
fn in_flight_flow_warning_appears_after_the_hold_only_airborne() {
    let s = spec();
    let hold = s.thresholds.in_flight_flow_warn_ms;
    let low_hold = s.thresholds.flow.low_hold_ms;
    let mut w = World::new();
    w.p.fc.params.insert("FLOW_TYPE".into(), 5.0);
    w.airborne(copter::GUIDED);
    fly_with_flow(&mut w, FC_SYS, 200, 500);
    assert_eq!(flow_item(&w).map(|i| i.0), Some(true));
    // Featureless ground: low after the low hold, warned after the warn hold.
    fly_with_flow(&mut w, FC_SYS, 30, low_hold + hold - 100);
    assert_eq!(flow_item(&w).map(|i| i.0), Some(true));
    fly_with_flow(&mut w, FC_SYS, 30, 200);
    assert_eq!(
        flow_item(&w),
        Some((false, s.warning.flow_degraded.clone()))
    );
    // The status pill renders failing readiness by id and text.
    let pilot = w.p.state(w.now);
    assert!(pilot
        .readiness
        .iter()
        .any(|r| r.id == "flow" && !r.ok && r.msg == s.warning.flow_degraded));

    // On the ground the same flow reads as the readiness text right away.
    let mut g = World::new();
    g.p.fc.params.insert("FLOW_TYPE".into(), 5.0);
    fly_with_flow(&mut g, FC_SYS, 30, low_hold + 100);
    assert_eq!(flow_item(&g), Some((false, s.readiness.flow.low.clone())));
}

#[test]
fn the_imu_feed_of_the_lidar_pose_is_a_readiness_item() {
    let mut w = World::new();
    let item = |w: &World| {
        w.p.state(w.now)
            .readiness
            .into_iter()
            .find(|r| r.id == "nav_imu")
            .map(|r| (r.ok, r.severity))
    };
    assert_eq!(item(&w), None);
    w.p.on_feed(
        "plugin.world-engine.map_status",
        &json!({"imu_feed": {"state": "degraded", "reason": "starved", "rate_hz": 12.0}}),
        w.now,
    );
    assert_eq!(
        item(&w),
        Some((false, ados_protocol::pilot::Severity::Block))
    );
    w.p.on_feed(
        "plugin.world-engine.map_status",
        &json!({"imu_feed": {"state": "ok", "reason": null, "rate_hz": 100.0}}),
        w.now,
    );
    assert_eq!(item(&w).map(|i| i.0), Some(true));
    w.p.on_feed(
        "plugin.world-engine.map_status",
        &json!({"imu_feed": null}),
        w.now,
    );
    assert_eq!(item(&w), None);
}

fn ekf(flags: EkfStatusFlags, pos_horiz_variance: f32) -> MavMessage {
    MavMessage::EKF_STATUS_REPORT(EKF_STATUS_REPORT_DATA {
        flags,
        pos_horiz_variance,
        ..Default::default()
    })
}

/// Run `ms` with one EKF_STATUS_REPORT every 200 ms from the FC.
fn run_with_ekf(w: &mut World, msg: &MavMessage, ms: u64) {
    let end = w.now + ms;
    while w.now < end {
        w.p.on_fc(FC_SYS, FC_COMP, msg, w.now);
        w.run(200);
    }
}

fn position_item(w: &World) -> (bool, ados_protocol::pilot::Severity, String) {
    let r =
        w.p.state(w.now)
            .readiness
            .into_iter()
            .find(|r| r.id == "position")
            .expect("a position item");
    (r.ok, r.severity, r.msg)
}

/// Every EKF-derived `position` message starts with this, so it never reads
/// like the navigator's position check.
const EKF_PREFIX: &str = "Position degraded (EKF): ";

#[test]
fn an_ekf_without_absolute_horizontal_position_blocks_the_position_item() {
    use ados_protocol::pilot::Severity;
    let mut w = World::new();
    let good = EkfStatusFlags::EKF_ATTITUDE
        | EkfStatusFlags::EKF_VELOCITY_HORIZ
        | EkfStatusFlags::EKF_POS_HORIZ_REL
        | EkfStatusFlags::EKF_POS_HORIZ_ABS;
    run_with_ekf(&mut w, &ekf(good, 0.1), 600);
    assert!(position_item(&w).0, "{:?}", position_item(&w));
    // The lidar pose stops: local position still streams, the EKF has lost
    // its absolute horizontal position.
    run_with_ekf(
        &mut w,
        &ekf(good - EkfStatusFlags::EKF_POS_HORIZ_ABS, 0.1),
        600,
    );
    let (ok, sev, msg) = position_item(&w);
    assert!(!ok, "{msg}");
    assert_eq!(sev, Severity::Block);
    assert!(msg.contains("absolute"), "{msg}");
    assert!(msg.starts_with(EKF_PREFIX), "{msg}");
}

#[test]
fn a_high_ekf_position_variance_degrades_then_blocks_the_position_item() {
    use ados_protocol::pilot::Severity;
    let mut w = World::new();
    let good = EkfStatusFlags::EKF_ATTITUDE
        | EkfStatusFlags::EKF_POS_HORIZ_REL
        | EkfStatusFlags::EKF_POS_HORIZ_ABS;
    run_with_ekf(&mut w, &ekf(good, 0.6), 600);
    let (ok, sev, msg) = position_item(&w);
    assert!(!ok, "{msg}");
    assert_eq!(sev, Severity::Warn);
    assert!(msg.starts_with(EKF_PREFIX), "{msg}");
    run_with_ekf(&mut w, &ekf(good, 0.9), 600);
    let (ok, sev, msg) = position_item(&w);
    assert!(!ok, "{msg}");
    assert_eq!(sev, Severity::Block);
    assert!(msg.starts_with(EKF_PREFIX), "{msg}");
    // A report that stops after it was heard never reads as a good estimate.
    w.run(4000);
    let (ok, sev, msg) = position_item(&w);
    assert!(!ok, "{msg}");
    assert_eq!(sev, Severity::Warn);
    // The navigator's own position check says "Position unverified"; the
    // EKF item never borrows that wording.
    assert!(msg.starts_with(EKF_PREFIX), "{msg}");
    assert!(msg.contains("not reporting"), "{msg}");
}

/// The flow-only source set (`EK3_SRC1_POSXY 0`, `VELXY 5`, `FLOW_TYPE` on):
/// ArduPilot reports a relative horizontal position and never an absolute one.
fn flow_only(w: &mut World) {
    for (n, v) in [
        ("EK3_SRC1_POSXY", 0.0),
        ("EK3_SRC1_VELXY", 5.0),
        ("FLOW_TYPE", 5.0),
    ] {
        w.p.fc.params.insert(n.into(), v);
    }
}

#[test]
fn a_flow_only_ekf_with_a_relative_position_passes_on_the_ground_and_in_the_air() {
    let mut w = World::new();
    flow_only(&mut w);
    let rel = EkfStatusFlags::EKF_ATTITUDE
        | EkfStatusFlags::EKF_VELOCITY_HORIZ
        | EkfStatusFlags::EKF_POS_HORIZ_REL
        | EkfStatusFlags::EKF_PRED_POS_HORIZ_REL;
    run_with_ekf(&mut w, &ekf(rel, 0.1), 600);
    assert!(position_item(&w).0, "{:?}", position_item(&w));
    w.airborne(crate::modes::copter::LOITER);
    run_with_ekf(&mut w, &ekf(rel, 0.1), 600);
    assert!(position_item(&w).0, "{:?}", position_item(&w));
}

#[test]
fn an_ekf_with_no_position_of_any_kind_blocks_even_with_flow_enabled() {
    use ados_protocol::pilot::Severity;
    let mut w = World::new();
    flow_only(&mut w);
    run_with_ekf(
        &mut w,
        &ekf(
            EkfStatusFlags::EKF_ATTITUDE | EkfStatusFlags::EKF_VELOCITY_HORIZ,
            0.1,
        ),
        600,
    );
    let (ok, sev, msg) = position_item(&w);
    assert!(!ok, "{msg}");
    assert_eq!(sev, Severity::Block);
    assert!(msg.starts_with(EKF_PREFIX), "{msg}");
    // Armed, a relative position in constant-position mode is no position.
    w.airborne(crate::modes::copter::LOITER);
    run_with_ekf(
        &mut w,
        &ekf(
            EkfStatusFlags::EKF_ATTITUDE
                | EkfStatusFlags::EKF_POS_HORIZ_REL
                | EkfStatusFlags::EKF_CONST_POS_MODE,
            0.1,
        ),
        600,
    );
    let (ok, sev, msg) = position_item(&w);
    assert!(!ok, "{msg}");
    assert_eq!(sev, Severity::Block);
}
