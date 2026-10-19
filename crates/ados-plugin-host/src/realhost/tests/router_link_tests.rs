//! Guided-setpoint and PIC-gate sends over a live router link.

use super::*;

#[tokio::test]
// Asserts on the spec-superseded MAV_FRAME_BODY_NED, which is what the
// guided-setpoint builder emits and ArduPilot reads; see
// `ados_protocol::mavlink::mav_frame_from_u8`.
#[allow(deprecated)]
async fn guided_setpoint_frame_reaches_the_router_and_decodes() {
    // With a live mavlink client wired to a stub router socket, the handler
    // builds the SET_POSITION_TARGET_LOCAL_NED (84) frame and writes it; the
    // router side reads it back and it decodes to the same message + fields.
    use crate::frame_link::{FrameLink, Wire, LINK_DEPTH};
    use ados_protocol::ipc::IpcBroadcast;
    use ados_protocol::mavlink::{ardupilotmega, parse_v2, MavMessage};

    let mut sock = std::env::temp_dir();
    sock.push(format!("ados-realhost-sp-{}.sock", std::process::id()));
    let _ = std::fs::remove_file(&sock);
    let (_server, inbound) = IpcBroadcast::bind(&sock, LINK_DEPTH, false, Some(16))
        .await
        .unwrap();
    let mut inbound = inbound.expect("inbound channel requested");

    let client = std::sync::Arc::new(FrameLink::spawn(
        &sock,
        Wire::MavlinkSocket,
        Box::new(Vec::new),
    ));
    assert!(
        client
            .connected_within(std::time::Duration::from_secs(2))
            .await
    );
    // A fleet drone with system id 3: the setpoint must be addressed to it.
    let host = RealHost::new().with_mavlink(client);
    host.fc_identity().set(3, 1);
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let m = ok_map(host.guided_setpoint_send("p", &velocity_args()));
    assert_eq!(field(&m, "sent").and_then(Value::as_bool), Some(true));
    assert_eq!(field(&m, "msg_id").and_then(Value::as_i64), Some(84));

    // The router reads the raw frame; decode it and check the fields.
    let frame = tokio::time::timeout(std::time::Duration::from_millis(500), inbound.recv())
        .await
        .unwrap()
        .expect("a frame arrives")
        .payload;
    let (_h, decoded) = parse_v2(&frame).expect("decode succeeds");
    match decoded {
        MavMessage::SET_POSITION_TARGET_LOCAL_NED(d) => {
            assert_eq!(d.vx, 2.5);
            assert_eq!(d.vy, -1.0);
            assert_eq!(d.vz, 0.5);
            assert_eq!(d.target_system, 3);
            assert_eq!(d.target_component, 1);
            assert_eq!(
                d.coordinate_frame,
                ardupilotmega::MavFrame::MAV_FRAME_BODY_NED
            );
        }
        other => panic!("expected SET_POSITION_TARGET_LOCAL_NED, got {other:?}"),
    }
}

#[tokio::test]
async fn a_command_the_pic_gate_refuses_reports_pic_refused_and_is_never_queued() {
    // With injector arbitration armed, the router drops the host's commands
    // while an operator holds PIC and says nothing back. The plugin must read
    // that refusal, not `sent: true`, and the frame must not be queued.
    use crate::frame_link::{FrameLink, Wire, LINK_DEPTH};
    use crate::pic_gate::PicGate;
    use ados_protocol::ipc::IpcBroadcast;

    let dir = tempfile::tempdir().unwrap();
    let sidecar = dir.path().join("pic-state.json");
    std::fs::write(&sidecar, r#"{"state":"claimed","claimed_by":"hdmi-kiosk"}"#).unwrap();

    let mut sock = std::env::temp_dir();
    sock.push(format!("ados-realhost-pic-{}.sock", std::process::id()));
    let _ = std::fs::remove_file(&sock);
    let (_server, inbound) = IpcBroadcast::bind(&sock, LINK_DEPTH, false, Some(16))
        .await
        .unwrap();
    let mut inbound = inbound.expect("inbound channel requested");

    let link = FrameLink::spawn(&sock, Wire::MavlinkSocket, Box::new(Vec::new))
        .with_pic_gate(PicGate::with_path(sidecar.clone(), "plugin-host"));
    let client = std::sync::Arc::new(link);
    assert!(
        client
            .connected_within(std::time::Duration::from_secs(2))
            .await
    );
    let host = RealHost::new().with_mavlink(client);
    host.fc_identity().set(3, 1);

    let m = ok_map(host.guided_setpoint_send("p", &velocity_args()));
    assert_eq!(field(&m, "sent").and_then(Value::as_bool), Some(false));
    assert_eq!(
        field(&m, "reason").and_then(Value::as_str),
        Some("pic_refused")
    );
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(200), inbound.recv())
            .await
            .is_err(),
        "a refused command never reaches the router"
    );

    // The operator releases PIC: the next command goes out.
    std::fs::write(&sidecar, r#"{"state":"unclaimed"}"#).unwrap();
    let m = ok_map(host.guided_setpoint_send("p", &velocity_args()));
    assert_eq!(field(&m, "sent").and_then(Value::as_bool), Some(true));
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(500), inbound.recv())
            .await
            .unwrap()
            .is_some()
    );
}
