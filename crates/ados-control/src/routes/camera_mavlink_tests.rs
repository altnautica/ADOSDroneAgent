use super::*;

use ados_protocol::mavlink::ardupilotmega::{MavResult, COMMAND_ACK_DATA};
use ados_protocol::mavlink_socket::encode_record;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixListener;

/// The vehicle's system id in every test.
const SYS: u8 = 7;
/// The camera component the fake vehicle answers from.
const CAM: u8 = 100;

fn timing() -> CameraTiming {
    CameraTiming {
        probe_window: Duration::from_millis(150),
        ack: AckConfig {
            base_timeout: Duration::from_millis(200),
            retries: 0,
            inprogress_timeout: Duration::from_millis(300),
        },
    }
}

/// An `AppState` whose MAVLink client points at `sock`, with a connected
/// ArduPilot flight controller at system 7 and short wait bounds.
fn state_with_socket(dir: &std::path::Path, sock: std::path::PathBuf) -> AppState {
    use crate::auth::PairingState;
    use crate::ipc::{LogdQueryClient, StateIpcClient};
    use crate::state::PairingPaths;
    let pairing_paths = PairingPaths {
        config: dir.join("config.yaml"),
        pairing_json: dir.join("pairing.json"),
        wfb_key_dir: dir.join("wfb"),
        bind_state: dir.join("bind-state.json"),
        profile_conf: dir.join("profile.conf"),
        mesh_role: dir.join("mesh-role"),
        relay_secret: dir.join("relay-peer-secret"),
    };
    let mut state = AppState::new(
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
    .with_params_path(dir.join("params.json"));
    state.mavlink_cameras = MavlinkCameras::with_timing(timing());
    state.state.set_snapshot_for_test(json!({
        "fc_connected": true,
        "autopilot": 3,
        "fc_system_id": SYS,
        "fc_component_id": 1,
    }));
    state
}

/// A `CAMERA_INFORMATION` frame from `(system, component)`.
fn camera_information(system: u8, component: u8, flags: CameraCapFlags) -> Vec<u8> {
    let mut vendor = [0u8; 32];
    vendor[..7].copy_from_slice(b"TestCam");
    let mut model = [0u8; 32];
    model[..7].copy_from_slice(b"Model-1");
    let info = CAMERA_INFORMATION_DATA {
        time_boot_ms: 1000,
        firmware_version: 0x0001_0203,
        focal_length: 4.8,
        sensor_size_h: 6.2,
        sensor_size_v: 4.6,
        flags,
        resolution_h: 1920,
        resolution_v: 1080,
        cam_definition_version: 0,
        vendor_name: vendor,
        model_name: model,
        lens_id: 0,
        cam_definition_uri: "".into(),
    };
    let header = MavHeader {
        system_id: system,
        component_id: component,
        sequence: 0,
    };
    mavlink::serialize_v2(header, &MavMessage::CAMERA_INFORMATION(info)).unwrap()
}

fn ack_frame(component: u8, command: MavCmd, result: MavResult) -> Vec<u8> {
    let header = MavHeader {
        system_id: SYS,
        component_id: component,
        sequence: 0,
    };
    mavlink::serialize_v2(
        header,
        &MavMessage::COMMAND_ACK(COMMAND_ACK_DATA { command, result }),
    )
    .unwrap()
}

/// A fake vehicle on the MAVLink socket. A `REQUEST_MESSAGE` is answered with
/// each of `cameras` (plus a camera on another system, which must be ignored);
/// any other command with `ack` from the camera component, or not at all.
/// Every `COMMAND_LONG` it reads is recorded.
struct FakeVehicle {
    commands: Arc<parking_lot::Mutex<Vec<COMMAND_LONG_DATA>>>,
}

impl FakeVehicle {
    fn start(
        path: &std::path::Path,
        cameras: Vec<(u8, CameraCapFlags)>,
        ack: Option<MavResult>,
    ) -> Self {
        let listener = UnixListener::bind(path).unwrap();
        let commands = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let seen = commands.clone();
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let (mut rd, mut wr) = stream.into_split();
                let cameras = cameras.clone();
                let seen = seen.clone();
                tokio::spawn(async move {
                    loop {
                        let mut hdr = [0u8; 4];
                        if rd.read_exact(&mut hdr).await.is_err() {
                            return;
                        }
                        let mut body = vec![0u8; u32::from_be_bytes(hdr) as usize];
                        if rd.read_exact(&mut body).await.is_err() {
                            return;
                        }
                        let Ok((_h, MavMessage::COMMAND_LONG(long))) = mavlink::parse_v2(&body)
                        else {
                            continue;
                        };
                        seen.lock().push(long.clone());
                        let mut replies = Vec::new();
                        if long.command == MavCmd::MAV_CMD_REQUEST_MESSAGE {
                            replies.push(camera_information(
                                SYS + 2,
                                CAM,
                                CameraCapFlags::CAMERA_CAP_FLAGS_CAPTURE_IMAGE,
                            ));
                            for (comp, flags) in &cameras {
                                replies.push(camera_information(SYS, *comp, *flags));
                            }
                        } else if let Some(result) = ack {
                            replies.push(ack_frame(long.target_component, long.command, result));
                        }
                        for frame in replies {
                            let framed = encode_record(1, &frame).unwrap();
                            if wr.write_all(&framed).await.is_err() {
                                return;
                            }
                        }
                        let _ = wr.flush().await;
                    }
                });
            }
        });
        Self { commands }
    }

    fn commands(&self) -> Vec<COMMAND_LONG_DATA> {
        self.commands.lock().clone()
    }
}

fn photo_and_zoom() -> CameraCapFlags {
    CameraCapFlags::CAMERA_CAP_FLAGS_CAPTURE_IMAGE
        | CameraCapFlags::CAMERA_CAP_FLAGS_HAS_BASIC_ZOOM
        | CameraCapFlags::CAMERA_CAP_FLAGS_HAS_VIDEO_STREAM
}

async fn body_json(resp: Response) -> Value {
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

async fn capture(state: &AppState, component_id: u8) -> Response {
    post_capture(State(state.clone()), Json(CaptureRequest { component_id })).await
}

async fn zoom(state: &AppState, component_id: u8, kind: ZoomType, value: f32) -> Response {
    post_zoom(
        State(state.clone()),
        Json(ZoomRequest {
            component_id,
            kind,
            value,
        }),
    )
    .await
}

#[tokio::test]
async fn the_list_is_empty_without_a_flight_controller_or_a_socket() {
    let dir = tempfile::tempdir().unwrap();
    let state = state_with_socket(dir.path(), dir.path().join("absent.sock"));
    let body = list_cameras(State(state.clone())).await.0;
    assert_eq!(body, json!({ "cameras": [] }), "no socket");

    state
        .state
        .set_snapshot_for_test(json!({ "fc_connected": false }));
    let body = list_cameras(State(state)).await.0;
    assert_eq!(body, json!({ "cameras": [] }), "no flight controller");
}

#[tokio::test]
async fn the_list_is_empty_when_no_camera_answers_and_the_request_names_camera_information() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("mavlink.sock");
    let vehicle = FakeVehicle::start(&sock, vec![], None);
    let state = state_with_socket(dir.path(), sock);
    let body = list_cameras(State(state)).await.0;
    assert_eq!(body, json!({ "cameras": [] }));
    let sent = vehicle.commands();
    assert_eq!(sent.len(), 1, "{sent:?}");
    assert_eq!(sent[0].command, MavCmd::MAV_CMD_REQUEST_MESSAGE);
    assert_eq!(sent[0].param1, 259.0, "CAMERA_INFORMATION");
    assert_eq!(
        (sent[0].target_system, sent[0].target_component),
        (SYS, 0),
        "every component of the vehicle"
    );
}

#[tokio::test]
async fn the_list_reports_each_camera_information_of_the_vehicle() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("mavlink.sock");
    let _vehicle = FakeVehicle::start(
        &sock,
        vec![
            (CAM, photo_and_zoom()),
            (101, CameraCapFlags::CAMERA_CAP_FLAGS_CAPTURE_VIDEO),
        ],
        None,
    );
    let state = state_with_socket(dir.path(), sock);
    let body = list_cameras(State(state)).await.0;
    assert_eq!(
        body,
        json!({ "cameras": [
            {
                "component_id": 100,
                "vendor": "TestCam",
                "model": "Model-1",
                "firmware_version": 0x0001_0203,
                "flags": 2 | 64 | 256,
                "can_capture_image": true,
                "can_capture_video": false,
                "can_zoom": true,
                "can_focus": false,
                "has_video_stream": true,
                "resolution_h": 1920,
                "resolution_v": 1080,
                "focal_length_mm": 4.8f32,
                "lens_id": 0,
                "cam_definition_uri": "",
            },
            {
                "component_id": 101,
                "vendor": "TestCam",
                "model": "Model-1",
                "firmware_version": 0x0001_0203,
                "flags": 1,
                "can_capture_image": false,
                "can_capture_video": true,
                "can_zoom": false,
                "can_focus": false,
                "has_video_stream": false,
                "resolution_h": 1920,
                "resolution_v": 1080,
                "focal_length_mm": 4.8f32,
                "lens_id": 0,
                "cam_definition_uri": "",
            },
        ]}),
        "the camera on another system is not listed"
    );
}

#[tokio::test]
async fn a_capture_sends_one_image_start_capture_and_maps_the_ack() {
    for (result, name, ok) in [
        (MavResult::MAV_RESULT_ACCEPTED, "ACCEPTED", true),
        (MavResult::MAV_RESULT_DENIED, "DENIED", false),
        (
            MavResult::MAV_RESULT_TEMPORARILY_REJECTED,
            "TEMPORARILY_REJECTED",
            false,
        ),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("mavlink.sock");
        let vehicle = FakeVehicle::start(&sock, vec![(CAM, photo_and_zoom())], Some(result));
        let state = state_with_socket(dir.path(), sock);
        let resp = capture(&state, CAM).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            body_json(resp).await,
            json!({ "ok": ok, "result": name, "component_id": CAM })
        );
        // The unknown component was probed for first, then captured.
        let sent = vehicle.commands();
        assert_eq!(sent.len(), 2, "{sent:?}");
        assert_eq!(sent[0].command, MavCmd::MAV_CMD_REQUEST_MESSAGE);
        let c = &sent[1];
        assert_eq!(c.command, MavCmd::MAV_CMD_IMAGE_START_CAPTURE);
        assert_eq!((c.target_system, c.target_component), (SYS, CAM));
        assert_eq!(
            [c.param1, c.param2, c.param3, c.param4],
            [0.0, 0.0, 1.0, 0.0],
            "all cameras of the component, interval 0, one image"
        );
    }
}

#[tokio::test]
async fn a_listed_camera_is_captured_without_probing_again() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("mavlink.sock");
    let vehicle = FakeVehicle::start(
        &sock,
        vec![(CAM, photo_and_zoom())],
        Some(MavResult::MAV_RESULT_ACCEPTED),
    );
    let state = state_with_socket(dir.path(), sock);
    let _ = list_cameras(State(state.clone())).await;
    assert_eq!(capture(&state, CAM).await.status(), StatusCode::OK);
    let kinds: Vec<MavCmd> = vehicle.commands().iter().map(|c| c.command).collect();
    assert_eq!(
        kinds,
        vec![
            MavCmd::MAV_CMD_REQUEST_MESSAGE,
            MavCmd::MAV_CMD_IMAGE_START_CAPTURE
        ]
    );
}

#[tokio::test]
async fn a_capture_with_no_ack_is_a_timeout() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("mavlink.sock");
    let _vehicle = FakeVehicle::start(&sock, vec![(CAM, photo_and_zoom())], None);
    let state = state_with_socket(dir.path(), sock);
    let resp = capture(&state, CAM).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        body_json(resp).await,
        json!({ "ok": false, "result": "TIMEOUT", "component_id": CAM })
    );
}

#[tokio::test]
async fn a_zoom_sends_set_camera_zoom_with_its_type_and_value() {
    for (kind, value, zoom_type) in [
        (ZoomType::Step, 1.0, 0.0),
        (ZoomType::Continuous, -0.5, 1.0),
        (ZoomType::Range, 50.0, 2.0),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("mavlink.sock");
        let vehicle = FakeVehicle::start(
            &sock,
            vec![(CAM, photo_and_zoom())],
            Some(MavResult::MAV_RESULT_ACCEPTED),
        );
        let state = state_with_socket(dir.path(), sock);
        let resp = zoom(&state, CAM, kind, value).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            body_json(resp).await,
            json!({ "ok": true, "result": "ACCEPTED", "component_id": CAM })
        );
        let sent = vehicle.commands();
        let c = sent.last().unwrap();
        assert_eq!(c.command, MavCmd::MAV_CMD_SET_CAMERA_ZOOM);
        assert_eq!((c.target_system, c.target_component), (SYS, CAM));
        assert_eq!(
            [c.param1, c.param2, c.param3],
            [zoom_type, value, 0.0],
            "{kind:?}"
        );
    }
}

#[tokio::test]
async fn a_zoom_ack_other_than_accepted_is_not_ok() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("mavlink.sock");
    let _vehicle = FakeVehicle::start(
        &sock,
        vec![(CAM, photo_and_zoom())],
        Some(MavResult::MAV_RESULT_UNSUPPORTED),
    );
    let state = state_with_socket(dir.path(), sock);
    let resp = zoom(&state, CAM, ZoomType::Range, 10.0).await;
    assert_eq!(
        body_json(resp).await,
        json!({ "ok": false, "result": "UNSUPPORTED", "component_id": CAM })
    );
}

#[tokio::test]
async fn an_unknown_component_is_404_and_nothing_is_commanded() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("mavlink.sock");
    let vehicle = FakeVehicle::start(
        &sock,
        vec![(CAM, photo_and_zoom())],
        Some(MavResult::MAV_RESULT_ACCEPTED),
    );
    let state = state_with_socket(dir.path(), sock);
    assert_eq!(capture(&state, 42).await.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        zoom(&state, 42, ZoomType::Step, 1.0).await.status(),
        StatusCode::NOT_FOUND
    );
    assert!(
        vehicle
            .commands()
            .iter()
            .all(|c| c.command == MavCmd::MAV_CMD_REQUEST_MESSAGE),
        "only probes were sent"
    );
}

#[tokio::test]
async fn bad_requests_and_missing_capabilities_are_refused_before_any_send() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("mavlink.sock");
    let vehicle = FakeVehicle::start(
        &sock,
        vec![(CAM, CameraCapFlags::CAMERA_CAP_FLAGS_CAPTURE_VIDEO)],
        Some(MavResult::MAV_RESULT_ACCEPTED),
    );
    let state = state_with_socket(dir.path(), sock);
    assert_eq!(capture(&state, 0).await.status(), StatusCode::BAD_REQUEST);
    for (kind, value) in [
        (ZoomType::Step, 2.0),
        (ZoomType::Continuous, f32::NAN),
        (ZoomType::Range, -1.0),
        (ZoomType::Range, 100.5),
    ] {
        assert_eq!(
            zoom(&state, CAM, kind, value).await.status(),
            StatusCode::BAD_REQUEST,
            "{kind:?} {value}"
        );
    }
    assert_eq!(capture(&state, CAM).await.status(), StatusCode::CONFLICT);
    assert_eq!(
        zoom(&state, CAM, ZoomType::Step, 1.0).await.status(),
        StatusCode::CONFLICT
    );
    assert!(vehicle
        .commands()
        .iter()
        .all(|c| c.command == MavCmd::MAV_CMD_REQUEST_MESSAGE));
}

#[tokio::test]
async fn writes_without_a_vehicle_link_are_503_and_msp_is_501() {
    let dir = tempfile::tempdir().unwrap();
    let state = state_with_socket(dir.path(), dir.path().join("absent.sock"));
    assert_eq!(
        capture(&state, CAM).await.status(),
        StatusCode::SERVICE_UNAVAILABLE,
        "no MAVLink socket"
    );
    state
        .state
        .set_snapshot_for_test(json!({ "fc_connected": false }));
    assert_eq!(
        zoom(&state, CAM, ZoomType::Step, 1.0).await.status(),
        StatusCode::SERVICE_UNAVAILABLE,
        "no flight controller"
    );
    state.state.set_snapshot_for_test(json!({
        "fc_connected": true,
        "fc_link_hint": "msp_detected",
    }));
    assert_eq!(
        capture(&state, CAM).await.status(),
        StatusCode::NOT_IMPLEMENTED,
        "MSP flight controller"
    );
    assert_eq!(
        list_cameras(State(state)).await.0,
        json!({ "cameras": [] }),
        "an MSP flight controller lists no camera"
    );
}

#[test]
fn the_zoom_type_reads_from_lowercase_names() {
    let req: ZoomRequest =
        serde_json::from_value(json!({"component_id": 100, "type": "continuous", "value": 0.5}))
            .unwrap();
    assert_eq!(req.kind, ZoomType::Continuous);
    assert!(serde_json::from_value::<ZoomRequest>(
        json!({"component_id": 100, "type": "focal", "value": 1})
    )
    .is_err());
}

#[test]
fn the_three_routes_are_served_natively() {
    let table = crate::routes::describe(false);
    for (method, path) in [
        ("GET", "/api/v1/camera/mavlink"),
        ("POST", "/api/v1/camera/mavlink/capture"),
        ("POST", "/api/v1/camera/mavlink/zoom"),
    ] {
        assert!(
            table.iter().any(|r| r.path == path && r.label() == method),
            "{method} {path} is not registered"
        );
    }
}

#[path = "camera_mavlink_ack_tests.rs"]
mod ack;
