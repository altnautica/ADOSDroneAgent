//! `flight.rate_setpoint.send`: a plugin's thrust reaches the wire as thrust.

use super::*;
use crate::frame_link::{FrameLink, Wire, LINK_DEPTH};
use ados_protocol::ipc::{InboundCommand, IpcBroadcast};
use ados_protocol::mavlink::{parse_v2, MavMessage};
use std::time::Duration;

const ARDUPILOT: u8 = 3;
const PX4: u8 = 12;
const QUADROTOR: u8 = 2;
/// `ATTITUDE_TARGET_TYPEMASK` body roll, pitch and yaw rates ignored.
const RATES_IGNORED: i64 = 0b111;
const THROTTLE_IGNORE: i64 = 64;

/// A host on a live stub router link whose parameter cache records `autopilot`
/// and `vehicle_type` and, when given, `GUID_OPTIONS`.
struct Rig {
    host: RealHost,
    inbound: tokio::sync::mpsc::Receiver<InboundCommand>,
    _server: IpcBroadcast,
    _dir: tempfile::TempDir,
}

async fn rig(tag: &str, autopilot: u8, vehicle_type: u8, guid_options: Option<f64>) -> Rig {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("params.fc.json"),
        format!(r#"{{"system_id":1,"autopilot":{autopilot},"vehicle_type":{vehicle_type}}}"#),
    )
    .unwrap();
    let params = guid_options.map_or_else(
        || r#"{"WPNAV_SPEED":{"value":500.0,"param_type":9,"last_updated":1.0}}"#.to_string(),
        |v| format!(r#"{{"GUID_OPTIONS":{{"value":{v:.1},"param_type":6,"last_updated":1.0}}}}"#),
    );
    std::fs::write(dir.path().join("params.json"), params).unwrap();

    let sock = dir.path().join(format!("router-{tag}.sock"));
    let (server, inbound) = IpcBroadcast::bind(&sock, LINK_DEPTH, false, Some(16))
        .await
        .unwrap();
    let client = std::sync::Arc::new(FrameLink::spawn(
        &sock,
        Wire::MavlinkSocket,
        Box::new(Vec::new),
    ));
    assert!(client.connected_within(Duration::from_secs(2)).await);
    let host = RealHost::new()
        .with_mavlink(client)
        .with_params_path(dir.path().join("params.json"));
    host.fc_identity().set(1, 1);
    tokio::time::sleep(Duration::from_millis(50)).await;
    Rig {
        host,
        inbound: inbound.expect("inbound channel requested"),
        _server: server,
        _dir: dir,
    }
}

/// A level attitude (identity quaternion) with body rates ignored, plus
/// `thrust`.
fn attitude_args(type_mask: i64, thrust: f64) -> Value {
    map(&[
        ("type_mask", Value::from(type_mask)),
        ("qw", Value::F64(1.0)),
        ("thrust", Value::F64(thrust)),
    ])
}

fn assert_refused(m: &[(Value, Value)], reason: &str) {
    assert_eq!(field(m, "sent").and_then(Value::as_bool), Some(false));
    assert_eq!(field(m, "reason").and_then(Value::as_str), Some(reason));
    assert!(field(m, "detail").and_then(Value::as_str).is_some());
}

async fn next_attitude_target(
    rig: &mut Rig,
) -> ados_protocol::mavlink::ardupilotmega::SET_ATTITUDE_TARGET_DATA {
    let frame = tokio::time::timeout(Duration::from_millis(500), rig.inbound.recv())
        .await
        .expect("a frame arrives in time")
        .expect("the link stays open")
        .payload;
    match parse_v2(&frame).expect("decode succeeds").1 {
        MavMessage::SET_ATTITUDE_TARGET(d) => d,
        other => panic!("expected SET_ATTITUDE_TARGET, got {other:?}"),
    }
}

async fn assert_nothing_sent(rig: &mut Rig) {
    let got = tokio::time::timeout(Duration::from_millis(200), rig.inbound.recv()).await;
    assert!(got.is_err(), "no frame reaches the router: {got:?}");
}

#[tokio::test]
async fn a_copter_reading_thrust_gets_the_plugin_thrust_unchanged() {
    // GUID_OPTIONS bit 3 ORed into another operator bit.
    let mut rig = rig("thrust", ARDUPILOT, QUADROTOR, Some(9.0)).await;
    let m = ok_map(
        rig.host
            .rate_setpoint_send("p", &attitude_args(RATES_IGNORED, 0.37)),
    );
    assert_eq!(field(&m, "sent").and_then(Value::as_bool), Some(true));
    assert_eq!(field(&m, "msg_id").and_then(Value::as_i64), Some(82));
    let d = next_attitude_target(&mut rig).await;
    assert_eq!(d.thrust, 0.37_f32);
    assert_eq!(d.q, [1.0, 0.0, 0.0, 0.0]);
    assert_eq!(d.type_mask.bits(), RATES_IGNORED as u8);
    assert_eq!((d.target_system, d.target_component), (1, 1));
}

#[tokio::test]
async fn a_copter_reading_a_climb_rate_refuses_the_thrust_and_sends_nothing() {
    let mut rig = rig("climb", ARDUPILOT, QUADROTOR, Some(0.0)).await;
    let m = ok_map(
        rig.host
            .rate_setpoint_send("p", &attitude_args(RATES_IGNORED, 0.37)),
    );
    assert_refused(&m, "thrust_is_climb_rate");
    assert_nothing_sent(&mut rig).await;
}

#[tokio::test]
async fn a_copter_whose_guid_options_is_unread_refuses_the_thrust() {
    let mut rig = rig("unknown", ARDUPILOT, QUADROTOR, None).await;
    let m = ok_map(
        rig.host
            .rate_setpoint_send("p", &attitude_args(RATES_IGNORED, 0.37)),
    );
    assert_refused(&m, "thrust_semantics_unknown");
    assert_nothing_sent(&mut rig).await;
}

#[tokio::test]
async fn a_setpoint_that_ignores_throttle_is_sent_whatever_the_options() {
    let mut rig = rig("ignored", ARDUPILOT, QUADROTOR, Some(0.0)).await;
    let args = map(&[
        ("type_mask", Value::from(RATES_IGNORED | THROTTLE_IGNORE)),
        ("qw", Value::F64(1.0)),
    ]);
    let m = ok_map(rig.host.rate_setpoint_send("p", &args));
    assert_eq!(field(&m, "sent").and_then(Value::as_bool), Some(true));
    let d = next_attitude_target(&mut rig).await;
    assert_eq!(d.type_mask.bits(), (RATES_IGNORED | THROTTLE_IGNORE) as u8);
}

#[tokio::test]
async fn px4_gets_the_plugin_thrust_unchanged() {
    let mut rig = rig("px4", PX4, QUADROTOR, None).await;
    let m = ok_map(
        rig.host
            .rate_setpoint_send("p", &attitude_args(RATES_IGNORED, 0.61)),
    );
    assert_eq!(field(&m, "sent").and_then(Value::as_bool), Some(true));
    assert_eq!(next_attitude_target(&mut rig).await.thrust, 0.61_f32);
}

#[test]
fn a_thrust_setpoint_must_name_its_thrust() {
    let host = host_with_fc();
    let args = map(&[
        ("type_mask", Value::from(RATES_IGNORED)),
        ("qw", Value::F64(1.0)),
    ]);
    assert!(err_body(host.rate_setpoint_send("p", &args)).contains("thrust is required"));
}
