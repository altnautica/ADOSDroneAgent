use super::*;
use ados_protocol::mavlink::ardupilotmega::{
    MavResult, MavSeverity, COMMAND_ACK_DATA, STATUSTEXT_DATA,
};
use ados_protocol::mavlink_socket::encode_record;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixListener;

/// The default FC target the frame tests address and answer from.
fn target() -> (u8, u8) {
    crate::fc_target::DEFAULT_TARGET
}

/// [`build_command`] addressed to [`target`], as the route sends it.
fn build_addressed(cmd: &str, args: &[Value], fc: FcIdentity) -> (COMMAND_LONG_DATA, Value) {
    let (long, body) = build_command(cmd, args, fc).unwrap();
    (addressed(long, target()), body)
}

/// An ArduCopter quadrotor (`MAV_AUTOPILOT_ARDUPILOTMEGA` 3, `MAV_TYPE_QUADROTOR` 2)
/// whose banner has been seen.
const COPTER: FcIdentity = FcIdentity {
    autopilot: 3,
    mav_type: 2,
    firmware: Some(ArduPilotFirmware::Copter),
    q_enabled: false,
};

/// A PX4 quadrotor; the vehicle type does not select PX4's mode encoding.
const PX4: FcIdentity = FcIdentity {
    autopilot: MAV_AUTOPILOT_PX4,
    mav_type: 2,
    firmware: None,
    q_enabled: false,
};

// ── G6: command building + PX4 mode encoding ────────────────────────────

#[test]
fn arm_is_component_arm_disarm_param1_one() {
    let (d, body) = build_command("arm", &[], COPTER).unwrap();
    assert_eq!(d.command, MavCmd::MAV_CMD_COMPONENT_ARM_DISARM);
    assert_eq!(d.param1, 1.0);
    assert_eq!((d.target_system, d.target_component), (0, 0), "unaddressed");
    let d = addressed(d, (7, 1));
    assert_eq!(d.target_system, 7);
    assert_eq!(d.target_component, 1);
    assert_eq!(body["cmd"], json!("arm"));
    assert_eq!(body["status"], json!("ok"));
}

#[test]
fn disarm_is_component_arm_disarm_param1_zero() {
    let (d, _b) = build_command("disarm", &[], COPTER).unwrap();
    assert_eq!(d.command, MavCmd::MAV_CMD_COMPONENT_ARM_DISARM);
    assert_eq!(d.param1, 0.0);
}

#[test]
fn takeoff_default_alt_is_ten_in_param7() {
    let (d, body) = build_command("takeoff", &[], COPTER).unwrap();
    assert_eq!(d.command, MavCmd::MAV_CMD_NAV_TAKEOFF);
    assert_eq!(d.param7, 10.0);
    assert_eq!(body["altitude"], json!(10.0));
}

#[test]
fn takeoff_reads_the_altitude_arg_numeric_and_string() {
    let (d, body) = build_command("takeoff", &[json!(25.0)], COPTER).unwrap();
    assert_eq!(d.param7, 25.0);
    assert_eq!(body["altitude"], json!(25.0));
    // A stringly-typed numeric arg parses, matching Python float().
    let (d2, _b) = build_command("takeoff", &[json!("30")], COPTER).unwrap();
    assert_eq!(d2.param7, 30.0);
}

/// Every arg that cannot be a real altitude is a 400 and builds NO frame.
///
/// `build_command` is the only producer of the `COMMAND_LONG`, so an `Err`
/// here means nothing reaches the encoder, let alone the FC. `f32: FromStr`
/// accepts `nan`/`inf`/`infinity` case-insensitively and saturates `1e39` to
/// `+inf`, and all three used to land in `param7` verbatim; a negative value
/// commanded a descent from a takeoff request, and an unparseable string
/// silently became 10 m.
#[test]
fn takeoff_refuses_every_unflyable_altitude_arg() {
    for arg in [
        json!("NaN"),
        json!("nan"),
        json!("inf"),
        json!("-inf"),
        json!("Infinity"),
        json!("1e39"),      // overflows f32 → +inf on the cast
        json!(1e39),        // same, as a JSON number
        json!(-10.0),       // a takeoff that descends
        json!(0),           // a takeoff to the ground
        json!(5000.0),      // past MAX_TAKEOFF_ALT_M
        json!("thirty"),    // unparseable: must not become the default
        json!(""),          // ditto
        json!(true),        // wrong type
        json!(Value::Null), // wrong type
        json!([25.0]),      // wrong type
    ] {
        let err = build_command("takeoff", std::slice::from_ref(&arg), COPTER)
            .expect_err(&format!("{arg} must be refused, not flown"));
        assert_eq!(err.status, StatusCode::BAD_REQUEST, "arg {arg}");
    }
}

/// The boundaries of the accepted range, so the refusal is not so wide that
/// it rejects a real operation.
#[test]
fn takeoff_accepts_the_bounded_positive_range() {
    for arg in [json!(0.5), json!(10), json!("120.5"), json!(1000.0)] {
        let (d, _b) = build_command("takeoff", std::slice::from_ref(&arg), COPTER)
            .unwrap_or_else(|_| panic!("{arg} is a flyable altitude"));
        assert!(d.param7.is_finite() && d.param7 > 0.0);
    }
    // Absent args keep the documented default; only a PRESENT arg is judged.
    let (d, _b) = build_command("takeoff", &[], COPTER).unwrap();
    assert_eq!(d.param7, DEFAULT_TAKEOFF_ALT_M);
}

#[test]
fn land_is_nav_land_all_zero() {
    let (d, _b) = build_command("land", &[], COPTER).unwrap();
    assert_eq!(d.command, MavCmd::MAV_CMD_NAV_LAND);
    for p in [
        d.param1, d.param2, d.param3, d.param4, d.param5, d.param6, d.param7,
    ] {
        assert_eq!(p, 0.0);
    }
}

#[test]
fn land_is_vehicle_aware() {
    // PX4 keeps NAV_LAND.
    let (d, _b) = build_command("land", &[], PX4).unwrap();
    assert_eq!(d.command, MavCmd::MAV_CMD_NAV_LAND);
    // A QuadPlane by VTOL type, by quadrotor type with a Plane banner, or
    // by Q_ENABLE on a fixed-wing type: QLAND (Plane custom mode 20).
    let vtol = FcIdentity {
        autopilot: 3,
        mav_type: 20,
        firmware: None,
        q_enabled: false,
    };
    let quad_typed = FcIdentity {
        mav_type: 2,
        firmware: Some(ArduPilotFirmware::Plane),
        ..vtol
    };
    let fw_typed_q = FcIdentity {
        mav_type: 1,
        q_enabled: true,
        ..quad_typed
    };
    for fc in [vtol, quad_typed, fw_typed_q] {
        let (d, _b) = build_command("land", &[], fc).unwrap();
        assert_eq!(d.command, MavCmd::MAV_CMD_DO_SET_MODE, "{fc:?}");
        assert_eq!((d.param1, d.param2), (1.0, 20.0), "{fc:?}");
    }
    // A plain fixed-wing starts its landing sequence.
    let plane = FcIdentity {
        q_enabled: false,
        ..fw_typed_q
    };
    let (d, _b) = build_command("land", &[], plane).unwrap();
    assert_eq!(d.command, MavCmd::MAV_CMD_DO_LAND_START);
}

#[test]
fn copter_rtl_is_do_set_mode_param2_six() {
    // On ArduCopter the `rtl` shortcut sends DO_SET_MODE custom_mode=6.
    let (d, _b) = build_command("rtl", &[], COPTER).unwrap();
    assert_eq!(d.command, MavCmd::MAV_CMD_DO_SET_MODE);
    assert_eq!(d.param1, 1.0);
    assert_eq!(d.param2, 6.0, "ArduCopter RTL custom_mode is 6");
    assert_eq!(d.param3, 0.0, "ArduPilot uses no sub-mode");
}

#[test]
fn copter_mode_rtl_matches_the_rtl_shortcut() {
    let (d, body) = build_command("mode", &[json!("rtl")], COPTER).unwrap();
    assert_eq!(d.command, MavCmd::MAV_CMD_DO_SET_MODE);
    assert_eq!(d.param1, 1.0);
    assert_eq!(d.param2, 6.0);
    assert_eq!(body["mode"], json!("RTL"));
    assert_eq!(body["cmd"], json!("mode"));
}

#[test]
fn copter_mode_guided_resolves_to_four() {
    let (d, _b) = build_command("mode", &[json!("GUIDED")], COPTER).unwrap();
    assert_eq!(d.param2, 4.0);
    assert_eq!(d.param3, 0.0);
}

#[test]
fn a_quadplane_reporting_a_quadrotor_type_gets_the_plane_rtl() {
    // Q_MAV_TYPE=QUADROTOR: the heartbeat says 2, the banner says Plane.
    let quadplane = FcIdentity {
        autopilot: 3,
        mav_type: 2,
        firmware: Some(ArduPilotFirmware::Plane),
        q_enabled: false,
    };
    let (d, _b) = build_command("rtl", &[], quadplane).unwrap();
    assert_eq!(d.param2, 11.0, "Plane RTL, never the copter 6 (FBWB)");
    // With no banner yet, a quadrotor-typed FC is not guessed at.
    let unidentified = FcIdentity {
        firmware: None,
        ..quadplane
    };
    let err = build_command("rtl", &[], unidentified).unwrap_err();
    assert_eq!(err.status, StatusCode::CONFLICT);
    let err = build_command("mode", &[json!("LOITER")], unidentified).unwrap_err();
    assert_eq!(err.status, StatusCode::CONFLICT);
}

#[test]
fn px4_rtl_is_do_set_mode_auto_main_rtl_sub() {
    // On PX4 the `rtl` shortcut sends DO_SET_MODE with de-packed main/sub:
    // AUTO (4) in param2, RTL (5) in param3 — NOT the copter value 6.
    let (d, _b) = build_command("rtl", &[], PX4).unwrap();
    assert_eq!(d.command, MavCmd::MAV_CMD_DO_SET_MODE);
    assert_eq!(d.param1, 1.0);
    assert_eq!(d.param2, 4.0, "PX4 RTL main_mode is AUTO=4");
    assert_eq!(d.param3, 5.0, "PX4 RTL sub_mode is 5");
    assert_ne!(d.param2, 6.0, "PX4 RTL is never the ArduCopter value 6");
}

#[test]
fn px4_mode_golden_main_sub_and_packed_values() {
    // Golden (main, sub) for the AUTO modes, plus the packed custom_mode value
    // (main<<16 | sub<<24) each pair decodes to on the HEARTBEAT side — the
    // command uses the de-packed main/sub in param2/param3, and the two must
    // stay consistent with the router's decode table.
    let cases = [
        ("RTL", 4.0_f32, 5.0_f32, 0x0504_0000_u32),
        ("LOITER", 4.0, 3.0, 0x0304_0000),
        ("MISSION", 4.0, 4.0, 0x0404_0000),
        ("TAKEOFF", 4.0, 2.0, 0x0204_0000),
        ("LAND", 4.0, 6.0, 0x0604_0000),
    ];
    for (name, main, sub, packed) in cases {
        let (d, body) = build_command("mode", &[json!(name)], PX4).unwrap();
        assert_eq!(d.param1, 1.0, "{name}: custom-mode-enabled");
        assert_eq!(d.param2, main, "{name}: main_mode in param2");
        assert_eq!(d.param3, sub, "{name}: sub_mode in param3");
        assert_eq!(body["mode"], json!(name));
        // The de-packed main/sub reconstruct the router's packed custom_mode.
        let reconstructed = ((main as u32) << 16) | ((sub as u32) << 24);
        assert_eq!(reconstructed, packed, "{name}: packed custom_mode matches");
    }
}

#[test]
fn px4_single_level_modes_have_zero_sub() {
    // POSCTL / OFFBOARD / MANUAL etc. carry no sub-mode (param3 = 0).
    for (name, main) in [
        ("POSCTL", PX4_MAIN_POSCTL),
        ("POSITION", PX4_MAIN_POSCTL),
        ("OFFBOARD", PX4_MAIN_OFFBOARD),
        ("MANUAL", PX4_MAIN_MANUAL),
        ("ALTITUDE", PX4_MAIN_ALTCTL),
        ("ACRO", PX4_MAIN_ACRO),
        ("STABILIZED", PX4_MAIN_STABILIZED),
    ] {
        let (d, _b) = build_command("mode", &[json!(name)], PX4).unwrap();
        assert_eq!(d.param2, main as f32, "{name}: main_mode");
        assert_eq!(d.param3, 0.0, "{name}: no sub-mode");
    }
}

#[test]
fn px4_dotted_mode_name_resolves_like_the_short_name() {
    // The dotted decode name the state producer reports resolves to the same
    // pair as the short operator name.
    let (dotted, _b) = build_command("mode", &[json!("AUTO.MISSION")], PX4).unwrap();
    let (short, _b2) = build_command("mode", &[json!("MISSION")], PX4).unwrap();
    assert_eq!(dotted.param2, short.param2);
    assert_eq!(dotted.param3, short.param3);
    assert_eq!(dotted.param2, 4.0);
    assert_eq!(dotted.param3, 4.0);
}

#[test]
fn mode_with_no_name_is_a_400() {
    let err = build_command("mode", &[], COPTER).unwrap_err();
    assert_eq!(err.status, StatusCode::BAD_REQUEST);
    assert_eq!(err.detail, "Mode name required");
}

#[test]
fn unknown_mode_name_is_a_400_on_both_families() {
    let err = build_command("mode", &[json!("NOPE")], COPTER).unwrap_err();
    assert_eq!(err.status, StatusCode::BAD_REQUEST);
    assert_eq!(err.detail, "Unknown mode: NOPE");
    // A copter-only mode name is unknown on PX4.
    let err2 = build_command("mode", &[json!("POSHOLD")], PX4).unwrap_err();
    assert_eq!(err2.detail, "Unknown mode: POSHOLD");
}

#[test]
fn unknown_command_is_a_400() {
    let err = build_command("fly-to-the-moon", &[], COPTER).unwrap_err();
    assert_eq!(err.status, StatusCode::BAD_REQUEST);
    assert_eq!(err.detail, "Unknown command: fly-to-the-moon");
}

#[test]
fn command_serializes_to_a_valid_v2_frame() {
    // The built COMMAND_LONG serializes to a parseable v2 frame carrying the
    // same fields — proving the encode path the send uses is intact.
    let (d, _b) = build_command("mode", &[json!("RTL")], PX4).unwrap();
    let header = MavHeader {
        system_id: SOURCE_SYSTEM_ID,
        component_id: SOURCE_COMPONENT_ID,
        sequence: 0,
    };
    let frame = mavlink::serialize_v2(header, &MavMessage::COMMAND_LONG(d)).unwrap();
    assert_eq!(frame[0], 0xFD, "a v2 frame starts with 0xFD");
    let (_h, msg) = mavlink::parse_v2(&frame).unwrap();
    match msg {
        MavMessage::COMMAND_LONG(back) => {
            assert_eq!(back.command, MavCmd::MAV_CMD_DO_SET_MODE);
            assert_eq!(back.param2, 4.0);
            assert_eq!(back.param3, 5.0);
        }
        other => panic!("expected COMMAND_LONG, got {other:?}"),
    }
}

#[test]
fn catalog_lists_every_dispatchable_command() {
    let names: Vec<_> = SIMPLE_COMMANDS.iter().map(|(n, _)| *n).collect();
    assert_eq!(
        names,
        [
            "arm",
            "disarm",
            "takeoff",
            "land",
            "rtl",
            "mode",
            "killSwitch",
            "pauseMission",
            "resumeMission",
        ]
    );
}

#[test]
fn killswitch_is_a_force_disarm() {
    // The board sends camelCase; the route lowercases before dispatch, so
    // "killswitch" is what build_command matches. It force-disarms: disarm
    // (param1=0) with the 21196 force magic in param2 so the FC cuts motors
    // in flight instead of refusing.
    let (d, body) = build_command("killswitch", &[], COPTER).unwrap();
    assert_eq!(d.command, MavCmd::MAV_CMD_COMPONENT_ARM_DISARM);
    assert_eq!(d.param1, 0.0, "disarm");
    assert_eq!(
        d.param2, 21196.0,
        "force magic bypasses the in-flight guard"
    );
    assert_eq!(body["cmd"], json!("killSwitch"));
    // It is deliberately NOT a plain disarm: the force magic is the whole
    // point (a plain disarm carries param2=0 and the FC refuses it in flight).
    let (plain, _b) = build_command("disarm", &[], COPTER).unwrap();
    assert_eq!(plain.param2, 0.0);
    assert_ne!(d.param2, plain.param2);
}

#[test]
fn pause_and_resume_are_do_pause_continue() {
    // Both use the one autopilot-agnostic pause/continue opcode; param1
    // distinguishes them (0 pause, 1 continue), the arm/disarm pattern.
    let (pause, pbody) = build_command("pausemission", &[], COPTER).unwrap();
    assert_eq!(pause.command, MavCmd::MAV_CMD_DO_PAUSE_CONTINUE);
    assert_eq!(pause.param1, 0.0, "pause");
    assert_eq!(pbody["cmd"], json!("pauseMission"));

    let (resume, rbody) = build_command("resumemission", &[], COPTER).unwrap();
    assert_eq!(resume.command, MavCmd::MAV_CMD_DO_PAUSE_CONTINUE);
    assert_eq!(resume.param1, 1.0, "continue");
    assert_eq!(rbody["cmd"], json!("resumeMission"));
}

#[test]
fn new_commands_are_autopilot_agnostic() {
    // kill / pause / resume build the same frame on PX4 as on ArduPilot (no
    // mode-table lookup), so the board drives them identically either family.
    for cmd in ["killswitch", "pausemission", "resumemission"] {
        let (ap, _a) = build_command(cmd, &[], COPTER).unwrap();
        let (px4, _p) = build_command(cmd, &[], PX4).unwrap();
        assert_eq!(ap.command, px4.command, "{cmd}: same opcode either family");
        assert_eq!(ap.param1, px4.param1, "{cmd}: same param1 either family");
        assert_eq!(ap.param2, px4.param2, "{cmd}: same param2 either family");
    }
}

// ── G7: COMMAND_ACK correlation ─────────────────────────────────────────

/// Serialize a COMMAND_ACK frame from `src_system` acknowledging `command`
/// with `result`.
fn ack_frame(src_system: u8, command: MavCmd, result: MavResult) -> Vec<u8> {
    let header = MavHeader {
        system_id: src_system,
        component_id: 1,
        sequence: 0,
    };
    mavlink::serialize_v2(
        header,
        &MavMessage::COMMAND_ACK(COMMAND_ACK_DATA { command, result }),
    )
    .unwrap()
}

/// Serialize a STATUSTEXT frame carrying `text`.
fn statustext_frame(text: &str) -> Vec<u8> {
    let mut buf = [0u8; 50];
    let bytes = text.as_bytes();
    let n = bytes.len().min(50);
    buf[..n].copy_from_slice(&bytes[..n]);
    let header = MavHeader {
        system_id: target().0,
        component_id: 1,
        sequence: 0,
    };
    mavlink::serialize_v2(
        header,
        &MavMessage::STATUSTEXT(STATUSTEXT_DATA {
            severity: MavSeverity::MAV_SEVERITY_WARNING,
            text: buf.into(),
        }),
    )
    .unwrap()
}

#[test]
fn match_command_ack_correlates_on_command_and_source() {
    let frame = ack_frame(
        target().0,
        MavCmd::MAV_CMD_DO_SET_MODE,
        MavResult::MAV_RESULT_ACCEPTED,
    );
    // Matching command + source → the result.
    assert_eq!(
        match_command_ack(&frame, MavCmd::MAV_CMD_DO_SET_MODE, target().0),
        Some(0)
    );
    // Wrong command id → no match.
    assert_eq!(
        match_command_ack(&frame, MavCmd::MAV_CMD_NAV_TAKEOFF, target().0),
        None
    );
    // Wrong source system → no match.
    assert_eq!(
        match_command_ack(&frame, MavCmd::MAV_CMD_DO_SET_MODE, 42),
        None
    );
    // A non-ACK frame → no match.
    assert_eq!(
        match_command_ack(
            &statustext_frame("hi"),
            MavCmd::MAV_CMD_DO_SET_MODE,
            target().0
        ),
        None
    );
}

#[test]
fn match_command_ack_reads_the_result_value() {
    let denied = ack_frame(
        target().0,
        MavCmd::MAV_CMD_COMPONENT_ARM_DISARM,
        MavResult::MAV_RESULT_DENIED,
    );
    assert_eq!(
        match_command_ack(&denied, MavCmd::MAV_CMD_COMPONENT_ARM_DISARM, target().0),
        Some(2)
    );
}

#[test]
fn statustext_of_decodes_and_trims() {
    assert_eq!(
        statustext_of(&statustext_frame("PreArm: GPS")),
        Some("PreArm: GPS".to_string())
    );
    // A COMMAND_ACK is not a STATUSTEXT.
    assert_eq!(
        statustext_of(&ack_frame(
            target().0,
            MavCmd::MAV_CMD_DO_SET_MODE,
            MavResult::MAV_RESULT_ACCEPTED
        )),
        None
    );
}

#[test]
fn merge_ack_writes_accepted_block() {
    let mut body = json!({"status": "ok", "cmd": "arm"});
    merge_ack(
        &mut body,
        &AckOutcome::Acked {
            result: 0,
            statustext: None,
        },
    );
    assert_eq!(body["ack"]["observed"], json!(true));
    assert_eq!(body["ack"]["result"], json!(0));
    assert_eq!(body["ack"]["result_name"], json!("ACCEPTED"));
    assert_eq!(body["ack"]["accepted"], json!(true));
    // The original body is preserved.
    assert_eq!(body["cmd"], json!("arm"));
}

#[test]
fn merge_ack_writes_rejection_with_statustext() {
    let mut body = json!({"status": "ok", "cmd": "arm"});
    merge_ack(
        &mut body,
        &AckOutcome::Acked {
            result: 4,
            statustext: Some("PreArm: compass not calibrated".to_string()),
        },
    );
    assert_eq!(body["ack"]["accepted"], json!(false));
    assert_eq!(body["ack"]["result_name"], json!("FAILED"));
    assert_eq!(
        body["ack"]["statustext"],
        json!("PreArm: compass not calibrated")
    );
}

#[test]
fn merge_ack_writes_unobserved_for_no_ack() {
    let mut body = json!({"status": "ok", "cmd": "arm"});
    merge_ack(&mut body, &AckOutcome::NoAck);
    assert_eq!(body["ack"]["observed"], json!(false));
    assert!(body["ack"].get("result").is_none());
}

#[test]
fn only_an_accepted_ack_keeps_the_ok_status() {
    let accepted = AckOutcome::Acked {
        result: 0,
        statustext: None,
    };
    let mut body = json!({"status": "ok", "cmd": "arm"});
    settle_status(&mut body, &accepted);
    assert_eq!(body["status"], json!("ok"));
    assert!(body.get("message").is_none());

    let denied = AckOutcome::Acked {
        result: 2,
        statustext: Some("PreArm: 3D accel calibration needed".to_string()),
    };
    let mut body = json!({"status": "ok", "cmd": "arm"});
    settle_status(&mut body, &denied);
    assert_eq!(body["status"], json!("error"));
    assert_eq!(
        body["message"],
        json!("PreArm: 3D accel calibration needed")
    );

    let failed = AckOutcome::Acked {
        result: 4,
        statustext: None,
    };
    let mut body = json!({"status": "ok", "cmd": "rtl"});
    settle_status(&mut body, &failed);
    assert_eq!(body["status"], json!("error"));
    assert_eq!(body["message"], json!("flight controller answered FAILED"));

    let mut body = json!({"status": "ok", "cmd": "land"});
    settle_status(&mut body, &AckOutcome::NoAck);
    assert_eq!(body["status"], json!("error"));
}

#[tokio::test]
async fn a_close_after_in_progress_reports_in_progress_not_silence() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mavlink.sock");
    let listener = UnixListener::bind(&path).unwrap();
    // IN_PROGRESS, then the stream closes before any final result.
    let responses = vec![ack_frame(
        target().0,
        MavCmd::MAV_CMD_NAV_TAKEOFF,
        MavResult::MAV_RESULT_IN_PROGRESS,
    )];
    let server = tokio::spawn(fake_fc(listener, responses, Duration::ZERO));

    let client = MavlinkIpcClient::new(path.clone());
    let (long, _b) = build_addressed("takeoff", &[json!(15.0)], COPTER);
    let outcome = send_awaiting_ack(&client, &long, &test_cfg())
        .await
        .unwrap();
    assert_eq!(
        outcome,
        AckOutcome::Acked {
            result: MAV_RESULT_IN_PROGRESS,
            statustext: None
        }
    );
    server.await.unwrap();
}

/// A minimal fake FC on a unix socket: accept one client, drain its
/// length-prefixed command frames, then (after `delay`) broadcast `responses`
/// back framed the way the router does.
async fn fake_fc(listener: UnixListener, responses: Vec<Vec<u8>>, delay: Duration) {
    let (stream, _addr) = listener.accept().await.unwrap();
    let (mut rd, mut wr) = stream.into_split();
    // Drain inbound command frames so the client's writes never block.
    tokio::spawn(async move {
        let mut hdr = [0u8; 4];
        loop {
            if rd.read_exact(&mut hdr).await.is_err() {
                break;
            }
            let len = u32::from_be_bytes(hdr) as usize;
            let mut body = vec![0u8; len];
            if len > 0 && rd.read_exact(&mut body).await.is_err() {
                break;
            }
        }
    });
    if !delay.is_zero() {
        tokio::time::sleep(delay).await;
    }
    for r in responses {
        let framed = encode_record(1, &r).unwrap();
        if wr.write_all(&framed).await.is_err() {
            break;
        }
        let _ = wr.flush().await;
    }
    // Hold the write half open so the client can read before EOF.
    tokio::time::sleep(Duration::from_millis(80)).await;
}

fn test_cfg() -> AckConfig {
    AckConfig {
        base_timeout: Duration::from_millis(200),
        retries: 1,
        inprogress_timeout: Duration::from_millis(400),
    }
}

#[tokio::test]
async fn send_awaiting_ack_returns_accepted() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mavlink.sock");
    let listener = UnixListener::bind(&path).unwrap();
    let ack = ack_frame(
        target().0,
        MavCmd::MAV_CMD_DO_SET_MODE,
        MavResult::MAV_RESULT_ACCEPTED,
    );
    let server = tokio::spawn(fake_fc(listener, vec![ack], Duration::ZERO));

    let client = MavlinkIpcClient::new(path.clone());
    let (long, _b) = build_addressed("rtl", &[], PX4);
    let outcome = send_awaiting_ack(&client, &long, &test_cfg())
        .await
        .unwrap();
    assert_eq!(
        outcome,
        AckOutcome::Acked {
            result: 0,
            statustext: None
        }
    );
    server.await.unwrap();
}

#[tokio::test]
async fn send_awaiting_ack_rejection_carries_statustext() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mavlink.sock");
    let listener = UnixListener::bind(&path).unwrap();
    // A STATUSTEXT arrives, then the DENIED ack referencing our command.
    let responses = vec![
        statustext_frame("PreArm: 3D accel calibration needed"),
        ack_frame(
            target().0,
            MavCmd::MAV_CMD_COMPONENT_ARM_DISARM,
            MavResult::MAV_RESULT_DENIED,
        ),
    ];
    let server = tokio::spawn(fake_fc(listener, responses, Duration::ZERO));

    let client = MavlinkIpcClient::new(path.clone());
    let (long, _b) = build_addressed("arm", &[], COPTER);
    let outcome = send_awaiting_ack(&client, &long, &test_cfg())
        .await
        .unwrap();
    assert_eq!(
        outcome,
        AckOutcome::Acked {
            result: 2,
            statustext: Some("PreArm: 3D accel calibration needed".to_string())
        }
    );
    server.await.unwrap();
}

#[tokio::test]
async fn send_awaiting_ack_in_progress_then_final() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mavlink.sock");
    let listener = UnixListener::bind(&path).unwrap();
    let responses = vec![
        ack_frame(
            target().0,
            MavCmd::MAV_CMD_NAV_TAKEOFF,
            MavResult::MAV_RESULT_IN_PROGRESS,
        ),
        ack_frame(
            target().0,
            MavCmd::MAV_CMD_NAV_TAKEOFF,
            MavResult::MAV_RESULT_ACCEPTED,
        ),
    ];
    let server = tokio::spawn(fake_fc(listener, responses, Duration::ZERO));

    let client = MavlinkIpcClient::new(path.clone());
    let (long, _b) = build_addressed("takeoff", &[json!(15.0)], COPTER);
    let outcome = send_awaiting_ack(&client, &long, &test_cfg())
        .await
        .unwrap();
    assert_eq!(
        outcome,
        AckOutcome::Acked {
            result: 0,
            statustext: None
        }
    );
    server.await.unwrap();
}

#[tokio::test]
async fn send_awaiting_ack_no_ack_is_honest() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mavlink.sock");
    let listener = UnixListener::bind(&path).unwrap();
    // The FC never acks (drains commands, sends nothing).
    let server = tokio::spawn(fake_fc(listener, vec![], Duration::ZERO));

    let client = MavlinkIpcClient::new(path.clone());
    let (long, _b) = build_addressed("rtl", &[], COPTER);
    let outcome = send_awaiting_ack(&client, &long, &test_cfg())
        .await
        .unwrap();
    assert_eq!(
        outcome,
        AckOutcome::NoAck,
        "no ack observed is reported honestly"
    );
    server.await.unwrap();
}

#[tokio::test]
async fn send_awaiting_ack_ignores_an_ack_for_another_command() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mavlink.sock");
    let listener = UnixListener::bind(&path).unwrap();
    // The FC acks a DIFFERENT command; our command gets no matching ack.
    let responses = vec![ack_frame(
        target().0,
        MavCmd::MAV_CMD_NAV_LAND,
        MavResult::MAV_RESULT_ACCEPTED,
    )];
    let server = tokio::spawn(fake_fc(listener, responses, Duration::ZERO));

    let client = MavlinkIpcClient::new(path.clone());
    let (long, _b) = build_addressed("arm", &[], COPTER);
    let outcome = send_awaiting_ack(&client, &long, &test_cfg())
        .await
        .unwrap();
    assert_eq!(outcome, AckOutcome::NoAck);
    server.await.unwrap();
}

#[tokio::test]
async fn send_awaiting_ack_absent_socket_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let client = MavlinkIpcClient::new(dir.path().join("absent.sock"));
    let (long, _b) = build_addressed("arm", &[], COPTER);
    // No socket → Err (the route maps this to a 503), not a fake outcome.
    assert!(send_awaiting_ack(&client, &long, &test_cfg())
        .await
        .is_err());
}

/// An `AppState` whose MAVLink client points at `sock`; every other path is
/// inert under `dir`.
fn state_with_socket(dir: &std::path::Path, sock: std::path::PathBuf) -> AppState {
    use crate::auth::PairingState;
    use crate::ipc::{LogdQueryClient, StateIpcClient};
    use crate::state::PairingPaths;
    use std::sync::Arc;
    let pairing_paths = PairingPaths {
        config: dir.join("config.yaml"),
        pairing_json: dir.join("pairing.json"),
        wfb_key_dir: dir.join("wfb"),
        bind_state: dir.join("bind-state.json"),
        profile_conf: dir.join("profile.conf"),
        mesh_role: dir.join("mesh-role"),
        relay_secret: dir.join("relay-peer-secret"),
    };
    AppState::new(
        Arc::new(PairingState::with_path(dir.join("pairing.json"))),
        StateIpcClient::disconnected(),
        MavlinkIpcClient::new(sock),
        LogdQueryClient::new(dir.join("absent-logd.sock")),
        dir.join("board.json"),
        pairing_paths,
        Arc::new(crate::dashboard_pin::DashboardPin::with_path(
            dir.join("dashboard-pin.json"),
        )),
        Arc::new(crate::mcp::McpTokenStore::with_path(
            dir.join("mcp-token.json"),
        )),
    )
    .with_params_path(dir.join("params.json"))
}

#[tokio::test]
async fn targets_the_latched_fc_system_id() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mavlink.sock");
    let listener = UnixListener::bind(&path).unwrap();
    // A fake FC that records the first command's target and answers from
    // system 7, the id it heartbeats with.
    let server = tokio::spawn(async move {
        let (stream, _addr) = listener.accept().await.unwrap();
        let (mut rd, mut wr) = stream.into_split();
        let mut hdr = [0u8; 4];
        rd.read_exact(&mut hdr).await.unwrap();
        let mut body = vec![0u8; u32::from_be_bytes(hdr) as usize];
        rd.read_exact(&mut body).await.unwrap();
        let ack = ack_frame(
            7,
            MavCmd::MAV_CMD_COMPONENT_ARM_DISARM,
            MavResult::MAV_RESULT_ACCEPTED,
        );
        let framed = encode_record(1, &ack).unwrap();
        wr.write_all(&framed).await.unwrap();
        let _ = wr.flush().await;
        tokio::spawn(async move {
            let mut sink = [0u8; 256];
            while rd.read(&mut sink).await.is_ok_and(|n| n > 0) {}
        });
        tokio::time::sleep(Duration::from_millis(200)).await;
        body
    });

    let state = state_with_socket(dir.path(), path);
    state.state.set_snapshot_for_test(json!({
        "fc_connected": true,
        "autopilot": 3,
        "fc_system_id": 7,
        "fc_component_id": 1,
    }));
    let resp = execute_command(
        State(state),
        Json(CommandRequest {
            cmd: "arm".to_string(),
            args: vec![],
        }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let (_h, decoded) = mavlink::parse_v2(&server.await.unwrap()).unwrap();
    let MavMessage::COMMAND_LONG(sent) = decoded else {
        panic!("expected COMMAND_LONG on the socket");
    };
    assert_eq!(sent.target_system, 7);
    assert_eq!(sent.target_component, 1);
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let body: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(body["ack"]["accepted"], json!(true), "{body}");

    // An ACK from system 1 does not correlate with a command to 7.
    let from_one = ack_frame(
        1,
        MavCmd::MAV_CMD_COMPONENT_ARM_DISARM,
        MavResult::MAV_RESULT_ACCEPTED,
    );
    assert_eq!(
        match_command_ack(&from_one, MavCmd::MAV_CMD_COMPONENT_ARM_DISARM, 7),
        None
    );
    let from_seven = ack_frame(
        7,
        MavCmd::MAV_CMD_COMPONENT_ARM_DISARM,
        MavResult::MAV_RESULT_ACCEPTED,
    );
    assert_eq!(
        match_command_ack(&from_seven, MavCmd::MAV_CMD_COMPONENT_ARM_DISARM, 7),
        Some(0)
    );
}
