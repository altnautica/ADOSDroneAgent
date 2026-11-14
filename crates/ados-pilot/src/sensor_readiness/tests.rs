use ados_protocol::mavlink::ardupilotmega::{MavLandedState, MavMessage, OPTICAL_FLOW_DATA};
use serde_json::json;

use super::*;
use crate::sensor_spec::spec;

const FC: (u8, u8) = (1, 1);

fn fc(params: &[(&str, f32)]) -> FcState {
    let mut fc = FcState {
        sysid: FC.0,
        compid: FC.1,
        heartbeat_ms: 1,
        landed_state: Some(MavLandedState::MAV_LANDED_STATE_ON_GROUND),
        ..Default::default()
    };
    for (k, v) in params {
        fc.params.insert((*k).to_owned(), *v);
    }
    fc
}

fn flow_msg(quality: u8) -> MavMessage {
    MavMessage::OPTICAL_FLOW(OPTICAL_FLOW_DATA {
        quality,
        ..Default::default()
    })
}

/// Feed flow at 20 Hz from the FC over `[from, to)`.
fn feed_flow(fc: &mut FcState, quality: u8, from: u64, to: u64) {
    let mut t = from;
    while t < to {
        fc.update(FC.0, FC.1, &flow_msg(quality), t);
        t += 50;
    }
}

fn find(items: &[Readiness], id: &str) -> Option<(bool, Severity, String)> {
    items
        .iter()
        .find(|r| r.id == id)
        .map(|r| (r.ok, r.severity, r.msg.clone()))
}

fn run(
    fc: &FcState,
    mode: ControlMode,
    lidar: &LidarTrack,
    avoid_on: bool,
    now: u64,
) -> Vec<Readiness> {
    let mounts = FcSensors::default();
    let ctx = SensorCtx {
        lidar,
        avoid_on,
        mounts: &mounts,
        nav_imu: &crate::nav_imu::NavImuTrack::default(),
    };
    items(fc, mode, &ctx, now)
}

#[test]
fn flow_items_appear_only_when_flow_is_configured_or_seen() {
    let none = LidarTrack::default();
    let ids = |f: &FcState| -> Vec<String> {
        run(f, ControlMode::Ados, &none, false, 1000)
            .into_iter()
            .map(|r| r.id)
            .collect()
    };
    assert_eq!(ids(&fc(&[("FLOW_TYPE", 0.0)])), ["rangefinder"]);
    assert_eq!(
        ids(&fc(&[("FLOW_TYPE", 5.0)])),
        ["rangefinder", "flow", "sensor_params"]
    );
    let mut f = fc(&[]);
    feed_flow(&mut f, 200, 0, 500);
    assert_eq!(ids(&f), ["rangefinder", "flow", "sensor_params"]);
    // Unread: flow seen, FLOW_TYPE not yet read.
    let t = &spec().readiness.sensor_params;
    assert_eq!(
        find(
            &run(&f, ControlMode::Ados, &none, true, 500),
            "sensor_params"
        ),
        Some((false, Severity::Warn, t.unread.clone()))
    );
}

#[test]
fn flow_blocks_only_in_ados_when_the_ekf_fuses_flow() {
    let none = LidarTrack::default();
    let absent = spec().readiness.flow.absent.clone();
    let fused = fc(&[("FLOW_TYPE", 5.0), ("EK3_SRC2_VELXY", 5.0)]);
    let gps = fc(&[("FLOW_TYPE", 5.0), ("EK3_SRC1_VELXY", 3.0)]);
    let at = |f: &FcState, m| find(&run(f, m, &none, true, 1000), "flow");
    assert_eq!(
        at(&fused, ControlMode::Ados),
        Some((false, Severity::Block, absent.clone()))
    );
    assert_eq!(
        at(&fused, ControlMode::FcManual),
        Some((false, Severity::Warn, absent.clone()))
    );
    assert_eq!(
        at(&gps, ControlMode::Ados),
        Some((false, Severity::Warn, absent))
    );
    // Lost after it was seen.
    let mut f = fused.clone();
    feed_flow(&mut f, 200, 0, 1000);
    assert_eq!(
        find(&run(&f, ControlMode::Ados, &none, true, 1000), "flow"),
        Some((true, Severity::Block, spec().readiness.flow.ok.clone()))
    );
    assert_eq!(
        find(&run(&f, ControlMode::Ados, &none, true, 2000), "flow"),
        Some((false, Severity::Block, spec().readiness.flow.lost.clone()))
    );
    // Low quality on the ground is a warning, fused or not.
    feed_flow(&mut f, 40, 2000, 3100);
    assert_eq!(
        find(&run(&f, ControlMode::Ados, &none, true, 3100), "flow"),
        Some((false, Severity::Warn, spec().readiness.flow.low.clone()))
    );
}

#[test]
fn param_rows_block_only_with_block_severity_and_fused_flow() {
    let none = LidarTrack::default();
    let mismatch = &spec().readiness.sensor_params.mismatch;
    let f = fc(&[
        ("FLOW_TYPE", 5.0),
        ("RNGFND1_TYPE", 0.0),
        ("EK3_SRC1_VELXY", 5.0),
    ]);
    assert_eq!(
        find(&run(&f, ControlMode::Ados, &none, true, 0), "sensor_params"),
        Some((
            false,
            Severity::Block,
            format!("{mismatch}: RNGFND1_TYPE 0 (want 10)")
        ))
    );
    assert_eq!(
        find(&run(&f, ControlMode::Elrs, &none, true, 0), "sensor_params").map(|r| r.1),
        Some(Severity::Warn)
    );
    let f = fc(&[
        ("FLOW_TYPE", 5.0),
        ("FLOW_FXSCALER", 900.0),
        ("EK3_SRC1_VELXY", 5.0),
    ]);
    assert_eq!(
        find(&run(&f, ControlMode::Ados, &none, true, 0), "sensor_params"),
        Some((
            false,
            Severity::Warn,
            format!("{mismatch}: FLOW_FXSCALER 900 (want -800 to 800)")
        ))
    );
    let f = fc(&[("FLOW_TYPE", 5.0)]);
    assert_eq!(
        find(&run(&f, ControlMode::Ados, &none, true, 0), "sensor_params"),
        Some((
            true,
            Severity::Warn,
            spec().readiness.sensor_params.ok.clone()
        ))
    );
}

#[test]
fn lidar_item_by_state_mode_and_avoidance() {
    let f = fc(&[]);
    let t = &spec().readiness.lidar;
    let mut l = LidarTrack::default();
    // No lidar: with avoidance on, one warning that avoidance is unavailable
    // (in every mode); with it off, no item.
    for mode in [ControlMode::Ados, ControlMode::FcManual] {
        assert_eq!(
            find(&run(&f, mode, &l, true, 0), "lidar"),
            Some((false, Severity::Warn, t.absent.clone()))
        );
    }
    assert_eq!(
        find(&run(&f, ControlMode::Ados, &l, false, 0), "lidar"),
        None
    );
    let mut now = 100;
    while now <= 2500 {
        l.on_scan(now);
        now += 100;
    }
    let now = now - 100;
    assert_eq!(
        find(&run(&f, ControlMode::Ados, &l, true, now), "lidar"),
        Some((true, Severity::Block, t.ok.clone()))
    );
    let later = now + 500;
    assert_eq!(
        find(&run(&f, ControlMode::Ados, &l, true, later), "lidar"),
        Some((false, Severity::Block, t.lost.clone()))
    );
    // An expected lidar that stopped blocks in ADOS mode even with avoidance
    // off: the pilot holds on a stale expected lidar in flight.
    assert_eq!(
        find(&run(&f, ControlMode::Ados, &l, false, later), "lidar").map(|r| r.1),
        Some(Severity::Block)
    );
    assert_eq!(
        find(&run(&f, ControlMode::FcManual, &l, true, later), "lidar").map(|r| r.1),
        Some(Severity::Warn)
    );
    l.on_status(&json!({"connected": true, "health": "error"}), now);
    assert_eq!(
        find(&run(&f, ControlMode::Ados, &l, true, now), "lidar"),
        Some((false, Severity::Warn, t.degraded.clone()))
    );
    // A report alone (no scan yet) makes the lidar expected.
    let mut only = LidarTrack::default();
    only.on_status(&json!({"connected": false, "health": null}), 0);
    assert_eq!(
        find(&run(&f, ControlMode::Ados, &only, true, 0), "lidar"),
        Some((false, Severity::Block, t.lost.clone()))
    );
}

#[test]
fn in_flight_flow_warning_waits_for_the_hold_and_only_airborne() {
    let none = LidarTrack::default();
    let s = spec();
    let hold = s.thresholds.in_flight_flow_warn_ms;
    let mut air = fc(&[("FLOW_TYPE", 5.0)]);
    air.armed = true;
    air.landed_state = Some(MavLandedState::MAV_LANDED_STATE_IN_AIR);
    feed_flow(&mut air, 200, 0, 1000);
    // Quality falls at 1000: low from 2000 (the low hold), warned from 2000 + hold.
    feed_flow(&mut air, 40, 1000, 2000 + hold + 100);
    let flow_at = |f: &FcState, now| find(&run(f, ControlMode::Ados, &none, true, now), "flow");
    assert_eq!(
        flow_at(&air, 2000 + hold - 50),
        Some((true, Severity::Warn, s.readiness.flow.ok.clone()))
    );
    assert_eq!(
        flow_at(&air, 2000 + hold),
        Some((false, Severity::Warn, s.warning.flow_degraded.clone()))
    );
    // The same flow on the ground: the plain readiness text, no hold.
    let mut ground = air.clone();
    ground.armed = false;
    ground.landed_state = Some(MavLandedState::MAV_LANDED_STATE_ON_GROUND);
    assert_eq!(
        flow_at(&ground, 2000 + hold - 50),
        Some((false, Severity::Warn, s.readiness.flow.low.clone()))
    );
    // Flow stopping in the air: lost after lost_ms, warned once that has held.
    let mut air = fc(&[("FLOW_TYPE", 5.0)]);
    air.armed = true;
    air.landed_state = Some(MavLandedState::MAV_LANDED_STATE_IN_AIR);
    feed_flow(&mut air, 200, 0, 1000);
    let lost_at = 950 + s.thresholds.flow.lost_ms;
    assert_eq!(flow_at(&air, lost_at + hold - 1).map(|r| r.0), Some(true));
    assert_eq!(
        flow_at(&air, lost_at + hold),
        Some((false, Severity::Warn, s.warning.flow_lost.clone()))
    );
}

#[test]
fn flow_from_another_system_is_ignored() {
    let mut f = fc(&[]);
    // The module's own raw frames, forwarded alongside the FC's.
    f.update(200, 1, &flow_msg(255), 100);
    f.update(FC.0, 197, &flow_msg(255), 100);
    assert!(!f.flow_track.seen());
    f.update(FC.0, FC.1, &flow_msg(255), 100);
    assert!(f.flow_track.seen());
}
