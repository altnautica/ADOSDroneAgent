#[test]
fn identity_params_are_recognised_however_they_are_typed() {
    // Operators type parameter names, and MAVLink names are conventionally
    // upper case but arrive however they arrive.
    for name in [
        "SYSID_THISMAV",
        "sysid_thismav",
        " SysId_ThisMav ",
        "MAV_SYS_ID",
        "MAV_SYSID",
        "mav_sysid",
    ] {
        assert!(
            is_identity_param(name),
            "{name} changes the vehicle identity"
        );
    }
}

#[test]
fn ordinary_params_are_not_caught_by_the_identity_guard() {
    // The guard must be narrow. Refusing a benign parameter because its
    // name merely resembles one of these would be its own bug.
    for name in [
        "SYSID_MYGCS", // which GCS may command us — NOT our own identity
        "SYSID_ENFORCE",
        "NTF_LED_BRIGHT",
        "ACRO_RP_RATE",
        "AHRS_EKF_TYPE",
    ] {
        assert!(!is_identity_param(name), "{name} must remain writable");
    }
}

#[tokio::test]
async fn identity_param_still_refused() {
    // The fleet reconciler is the only writer of the vehicle's identity; a
    // REST write would re-address it under every other operator.
    let dir = tempfile::tempdir().unwrap();
    let state = test_state(dir.path());
    // Even with the parameter present in the cache (so the 404 guard would
    // pass), the write is refused.
    write_cache(dir.path(), &[("SYSID_THISMAV", 1.0)]);
    let resp = set_param(
        Path("SYSID_THISMAV".to_string()),
        State(state),
        Json(ParamSetRequest { value: 2.0 }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CONFLICT);
}

use super::*;
use crate::auth::PairingState;
use crate::ipc::{LogdQueryClient, MavlinkIpcClient, StateIpcClient};
use crate::state::PairingPaths;
use std::sync::Arc;

/// Build an `AppState` for a handler test: a disconnected state client (the
/// test primes its snapshot directly), a MAVLink client pointed at an absent
/// socket (so a send fails → the 503 path), and inert paths for the rest.
fn test_state(dir: &std::path::Path) -> AppState {
    test_state_with_socket(dir, dir.join("absent-mavlink.sock"))
}

/// [`test_state`] with the MAVLink client pointed at `mavlink_sock`.
fn test_state_with_socket(dir: &std::path::Path, mavlink_sock: std::path::PathBuf) -> AppState {
    let pairing = Arc::new(PairingState::with_path(dir.join("pairing.json")));
    let state = StateIpcClient::disconnected();
    let mavlink = MavlinkIpcClient::new(mavlink_sock);
    let logd = LogdQueryClient::new(dir.join("absent-logd.sock"));
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
        pairing,
        state,
        mavlink,
        logd,
        dir.join("board.json"),
        pairing_paths,
        std::sync::Arc::new(crate::dashboard_pin::DashboardPin::with_path(
            dir.join("dashboard-pin.json"),
        )),
        std::sync::Arc::new(crate::mcp::McpTokenStore::with_path(
            dir.join("mcp-token.json"),
        )),
    )
    .with_params_path(dir.join("params.json"))
}

/// Write a router-shaped parameter cache at the path `test_state` reads — the
/// `{name: {value, param_type, last_updated}}` document the MAVLink router
/// persists. Every entry is REAL32.
fn write_cache(dir: &std::path::Path, entries: &[(&str, f64)]) {
    let typed: Vec<(&str, f64, Value)> = entries.iter().map(|(n, v)| (*n, *v, json!(9))).collect();
    write_typed_cache(dir, &typed);
}

/// [`write_cache`] with an explicit `param_type` per entry (`null` for none).
fn write_typed_cache(dir: &std::path::Path, entries: &[(&str, f64, Value)]) {
    let doc: serde_json::Map<String, Value> = entries
        .iter()
        .map(|(name, value, param_type)| {
            (
                (*name).to_string(),
                json!({ "value": value, "param_type": param_type, "last_updated": 1.0 }),
            )
        })
        .collect();
    std::fs::write(
        dir.join("params.json"),
        serde_json::to_vec(&Value::Object(doc)).unwrap(),
    )
    .unwrap();
}

/// A Unix socket that reads one framed MAVLink message and hands it back.
fn one_frame_socket(sock: &std::path::Path) -> tokio::task::JoinHandle<Vec<u8>> {
    use tokio::io::AsyncReadExt;
    let listener = tokio::net::UnixListener::bind(sock).unwrap();
    tokio::spawn(async move {
        use ados_protocol::frame::{decode_len, HEADER_SIZE, MAVLINK_MAX_FRAME};
        let (mut conn, _addr) = listener.accept().await.unwrap();
        let mut header = [0u8; HEADER_SIZE];
        conn.read_exact(&mut header).await.unwrap();
        let len = decode_len(header, MAVLINK_MAX_FRAME, false).unwrap();
        let mut body = vec![0u8; len];
        conn.read_exact(&mut body).await.unwrap();
        body
    })
}

/// Decode a built PARAM_SET message back into its data for the parity asserts.
fn round_trip(msg: &MavMessage) -> PARAM_SET_DATA {
    let header = MavHeader {
        system_id: SOURCE_SYSTEM_ID,
        component_id: SOURCE_COMPONENT_ID,
        sequence: 0,
    };
    let frame = mavlink::serialize_v2(header, msg).unwrap();
    let (_h, decoded) = mavlink::parse_v2(&frame).unwrap();
    match decoded {
        MavMessage::PARAM_SET(d) => d,
        other => panic!("expected PARAM_SET, got {other:?}"),
    }
}

/// The 16-byte param_id, trimmed of its null padding, as a string.
fn param_name(d: &PARAM_SET_DATA) -> String {
    let end = d
        .param_id
        .iter()
        .position(|&b| b == 0)
        .unwrap_or(PARAM_ID_LEN);
    String::from_utf8_lossy(&d.param_id[..end]).to_string()
}

/// The default FC target the frame-builder tests address.
fn target() -> (u8, u8) {
    crate::fc_target::DEFAULT_TARGET
}

// ── the built frame ──────────────────────────────────────────────────────

#[test]
fn builds_a_param_set_carrying_the_declared_type() {
    let msg = build_param_set(
        "WPNAV_SPEED",
        750.0,
        param_codec::MAV_PARAM_TYPE_REAL32,
        target(),
    );
    let d = round_trip(&msg);
    assert_eq!(param_name(&d), "WPNAV_SPEED");
    assert_eq!(d.param_value, 750.0);
    assert_eq!(d.target_system, 1);
    assert_eq!(d.target_component, 1);
    assert_eq!(d.param_type, MavParamType::MAV_PARAM_TYPE_REAL32);
    let d = round_trip(&build_param_set(
        "FRAME_CLASS",
        1.0,
        param_codec::MAV_PARAM_TYPE_INT8,
        target(),
    ));
    assert_eq!(d.param_type, MavParamType::MAV_PARAM_TYPE_INT8);
}

#[test]
fn the_frame_header_carries_the_source_identity() {
    let msg = build_param_set(
        "ATC_RAT_RLL_P",
        0.135,
        param_codec::MAV_PARAM_TYPE_REAL32,
        target(),
    );
    let header = MavHeader {
        system_id: SOURCE_SYSTEM_ID,
        component_id: SOURCE_COMPONENT_ID,
        sequence: 0,
    };
    let frame = mavlink::serialize_v2(header, &msg).unwrap();
    let (h, _msg) = mavlink::parse_v2(&frame).unwrap();
    assert_eq!(h.system_id, 1, "source system is the companion identity");
    assert_eq!(
        h.component_id, 191,
        "source component is the companion identity"
    );
}

#[test]
fn a_long_param_name_is_truncated_to_sixteen_bytes() {
    // The wire param_id is fixed at 16 bytes; a longer name is truncated.
    let msg = build_param_set(
        "THIS_NAME_IS_WAY_TOO_LONG_FOR_THE_FIELD",
        1.0,
        param_codec::MAV_PARAM_TYPE_REAL32,
        target(),
    );
    let d = round_trip(&msg);
    assert_eq!(param_name(&d), "THIS_NAME_IS_WAY");
}

// ── the success body ─────────────────────────────────────────────────────

#[test]
fn the_acked_success_body_has_an_empty_message() {
    let body = build_set_response("WPNAV_SPEED", 750.0, true, Some(750.0));
    assert_eq!(
        body,
        json!({
            "name": "WPNAV_SPEED",
            "value": 750.0,
            "ack": true,
            "cached_value": 750.0,
            "message": "",
        })
    );
}

#[test]
fn the_unacked_success_body_carries_the_no_echo_text_and_null_cache() {
    let body = build_set_response("WPNAV_SPEED", 750.0, false, None);
    assert_eq!(
        body,
        json!({
            "name": "WPNAV_SPEED",
            "value": 750.0,
            "ack": false,
            "cached_value": Value::Null,
            "message": "FC did not echo PARAM_VALUE within 2s",
        })
    );
}

// ── cached_param_value + the cache-backed guards ─────────────────────────

/// A truncated cache must never be read as "this param exists": that would
/// let a PARAM_SET through for a name the agent has never actually seen.
#[tokio::test]
async fn a_corrupt_cache_is_a_404_not_a_write() {
    let dir = tempfile::tempdir().unwrap();
    let state = test_state(dir.path());
    std::fs::write(dir.path().join("params.json"), b"{ truncated").unwrap();
    state
        .state
        .set_snapshot_for_test(json!({ "fc_connected": true }));
    let resp = set_param(
        Path("WPNAV_SPEED".to_string()),
        State(state),
        Json(ParamSetRequest { value: 1.0 }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

/// A cached name with no recorded type is refused rather than guessed: a
/// guessed float would corrupt an integer parameter on PX4.
#[tokio::test]
async fn a_param_without_a_cached_type_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let state = test_state(dir.path());
    write_typed_cache(dir.path(), &[("COM_FLTMODE1", 5.0, Value::Null)]);
    state
        .state
        .set_snapshot_for_test(json!({ "fc_connected": true, "autopilot": 12 }));
    let resp = set_param(
        Path("COM_FLTMODE1".to_string()),
        State(state),
        Json(ParamSetRequest { value: 7.0 }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    assert_eq!(
        body_json(resp).await["error"],
        json!("E_PARAM_TYPE_UNKNOWN")
    );
}

/// A value its type cannot hold is a 400, never a wrapped write.
#[tokio::test]
async fn a_value_outside_the_params_type_is_a_400() {
    let dir = tempfile::tempdir().unwrap();
    let state = test_state(dir.path());
    write_typed_cache(dir.path(), &[("FRAME_CLASS", 1.0, json!(2))]);
    state
        .state
        .set_snapshot_for_test(json!({ "fc_connected": true, "autopilot": 3 }));
    let resp = set_param(
        Path("FRAME_CLASS".to_string()),
        State(state),
        Json(ParamSetRequest { value: 300.0 }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

/// PX4 takes an integer parameter as its integer bytes. Sent as the float
/// 5.0, the vehicle would store 1084227584.
#[tokio::test]
async fn a_px4_integer_write_sends_the_integer_bytes_and_type() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("mavlink.sock");
    let server = one_frame_socket(&sock);
    let state = test_state_with_socket(dir.path(), sock);
    // The cache already holds the decoded target, so the first poll acks.
    write_typed_cache(dir.path(), &[("COM_FLTMODE1", 5.0, json!(6))]);
    state
        .state
        .set_snapshot_for_test(json!({ "fc_connected": true, "autopilot": 12 }));
    let resp = set_param(
        Path("COM_FLTMODE1".to_string()),
        State(state),
        Json(ParamSetRequest { value: 5.0 }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let (_h, decoded) = mavlink::parse_v2(&server.await.unwrap()).unwrap();
    let MavMessage::PARAM_SET(d) = decoded else {
        panic!("expected PARAM_SET on the socket");
    };
    assert_eq!(d.param_value.to_le_bytes(), 5i32.to_le_bytes());
    assert_eq!(d.param_type, MavParamType::MAV_PARAM_TYPE_INT32);
    assert_eq!(body_json(resp).await["ack"], json!(true));
}

/// ArduPilot takes the same integer as a float, with its declared type.
#[tokio::test]
async fn an_ardupilot_integer_write_sends_the_float_and_type() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("mavlink.sock");
    let server = one_frame_socket(&sock);
    let state = test_state_with_socket(dir.path(), sock);
    write_typed_cache(dir.path(), &[("FLTMODE1", 5.0, json!(2))]);
    state
        .state
        .set_snapshot_for_test(json!({ "fc_connected": true, "autopilot": 3 }));
    let resp = set_param(
        Path("FLTMODE1".to_string()),
        State(state),
        Json(ParamSetRequest { value: 5.0 }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let (_h, decoded) = mavlink::parse_v2(&server.await.unwrap()).unwrap();
    let MavMessage::PARAM_SET(d) = decoded else {
        panic!("expected PARAM_SET on the socket");
    };
    assert_eq!(d.param_value, 5.0);
    assert_eq!(d.param_type, MavParamType::MAV_PARAM_TYPE_INT8);
}

/// The PARAM_SET goes to the system id the router latched from the FC's
/// HEARTBEAT, not to the 1/1 convention.
#[tokio::test]
async fn param_set_targets_latched_system() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("mavlink.sock");
    let server = one_frame_socket(&sock);
    let state = test_state_with_socket(dir.path(), sock);
    write_cache(dir.path(), &[("WPNAV_SPEED", 750.0)]);
    state.state.set_snapshot_for_test(json!({
        "fc_connected": true,
        "autopilot": 3,
        "fc_system_id": 7,
        "fc_component_id": 1,
    }));
    let resp = set_param(
        Path("WPNAV_SPEED".to_string()),
        State(state),
        Json(ParamSetRequest { value: 750.0 }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let (_h, decoded) = mavlink::parse_v2(&server.await.unwrap()).unwrap();
    let MavMessage::PARAM_SET(d) = decoded else {
        panic!("expected PARAM_SET on the socket");
    };
    assert_eq!(d.target_system, 7);
    assert_eq!(d.target_component, 1);
}

/// A float write whose f32 echo differs from the f64 request in the seventh
/// digit still acks: the vehicle holds exactly what it was sent.
#[tokio::test]
async fn a_landed_float_write_acks_at_f32_precision() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("mavlink.sock");
    let server = one_frame_socket(&sock);
    let state = test_state_with_socket(dir.path(), sock);
    write_cache(dir.path(), &[("WPNAV_SPEED", f64::from(100.1f32))]);
    state
        .state
        .set_snapshot_for_test(json!({ "fc_connected": true, "autopilot": 3 }));
    let resp = set_param(
        Path("WPNAV_SPEED".to_string()),
        State(state),
        Json(ParamSetRequest { value: 100.1 }),
    )
    .await;
    server.await.unwrap();
    assert_eq!(body_json(resp).await["ack"], json!(true));
}

#[test]
fn cached_param_value_reads_a_numeric_param() {
    let dir = tempfile::tempdir().unwrap();
    let state = test_state(dir.path());
    // Absent cache reads as None.
    assert_eq!(cached_param_value(&state, "WPNAV_SPEED"), None);

    write_cache(dir.path(), &[("WPNAV_SPEED", 750.0)]);
    assert_eq!(cached_param_value(&state, "WPNAV_SPEED"), Some(750.0));
    // A name absent from the cache reads as None.
    assert_eq!(cached_param_value(&state, "OTHER"), None);
}

// ── the handler: the guard order + the write-path 503 ────────────────────

/// A non-finite value is a 400 before any snapshot read or send.
#[tokio::test]
async fn non_finite_value_is_a_400() {
    let dir = tempfile::tempdir().unwrap();
    let state = test_state(dir.path());
    // A non-finite value is rejected by the first guard (the body model holds an
    // f64; the route checks finiteness, matching the FastAPI math.isfinite gate).
    let resp = set_param(
        Path("WPNAV_SPEED".to_string()),
        State(state),
        Json(ParamSetRequest { value: f64::NAN }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert_eq!(body_detail(resp).await, "value must be a finite number");
}

/// A param absent from the snapshot's `params` blob is a 404 with the FastAPI
/// message, before the FC-connected check.
#[tokio::test]
async fn unknown_param_is_a_404() {
    let dir = tempfile::tempdir().unwrap();
    let state = test_state(dir.path());
    // No cache file at all → the name is unknown.
    state
        .state
        .set_snapshot_for_test(json!({ "fc_connected": true }));
    let resp = set_param(
        Path("NO_SUCH_PARAM".to_string()),
        State(state),
        Json(ParamSetRequest { value: 1.0 }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        body_detail(resp).await,
        "Parameter 'NO_SUCH_PARAM' not in cache; agent must observe a \
             PARAM_VALUE for it before writes are allowed"
    );
}

/// A known param with the FC disconnected is a 503, before any send.
#[tokio::test]
async fn known_param_with_fc_disconnected_is_a_503() {
    let dir = tempfile::tempdir().unwrap();
    let state = test_state(dir.path());
    write_cache(dir.path(), &[("WPNAV_SPEED", 500.0)]);
    state
        .state
        .set_snapshot_for_test(json!({ "fc_connected": false }));
    let resp = set_param(
        Path("WPNAV_SPEED".to_string()),
        State(state),
        Json(ParamSetRequest { value: 750.0 }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body_detail(resp).await, "FC not connected");
}

/// A known param, FC connected, but no MAVLink socket: the send fails, so the
/// route is a 503 "FC connection unavailable" (the native no-link posture). The
/// test client points at an absent socket, so the send errors fast.
#[tokio::test]
async fn send_failure_with_no_mavlink_socket_is_a_503() {
    let dir = tempfile::tempdir().unwrap();
    let state = test_state(dir.path());
    write_cache(dir.path(), &[("WPNAV_SPEED", 500.0)]);
    state
        .state
        .set_snapshot_for_test(json!({ "fc_connected": true }));
    let resp = set_param(
        Path("WPNAV_SPEED".to_string()),
        State(state),
        Json(ParamSetRequest { value: 750.0 }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body_detail(resp).await, "FC connection unavailable");
}

/// A known param, FC connected, a live MAVLink socket that accepts the frame,
/// and the snapshot already carrying the target value: the send succeeds, the
/// poll sees the echo immediately, and the body reports `ack: true`. This
/// exercises the full write-path against a mock socket (mirroring the command
/// route's mock-socket test).
#[tokio::test]
async fn full_write_against_a_live_socket_acks_when_the_cache_holds_the_target() {
    use tokio::io::AsyncReadExt;
    use tokio::net::UnixListener;

    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("mavlink.sock");
    let listener = UnixListener::bind(&sock).unwrap();

    // The server reads one framed message and hands the raw frame back.
    let server = tokio::spawn(async move {
        use ados_protocol::frame::{decode_len, HEADER_SIZE, MAVLINK_MAX_FRAME};
        let (mut conn, _addr) = listener.accept().await.unwrap();
        let mut header = [0u8; HEADER_SIZE];
        conn.read_exact(&mut header).await.unwrap();
        let len = decode_len(header, MAVLINK_MAX_FRAME, false).unwrap();
        let mut body = vec![0u8; len];
        conn.read_exact(&mut body).await.unwrap();
        body
    });

    // Build a state whose MAVLink client points at the live socket.
    let pairing = Arc::new(PairingState::with_path(dir.path().join("pairing.json")));
    let stateipc = StateIpcClient::disconnected();
    // The cache already holds the target, so the first poll tick acks.
    write_cache(dir.path(), &[("WPNAV_SPEED", 750.0)]);
    stateipc.set_snapshot_for_test(json!({ "fc_connected": true }));
    let mavlink = MavlinkIpcClient::new(sock.clone());
    let logd = LogdQueryClient::new(dir.path().join("absent-logd.sock"));
    let pairing_paths = PairingPaths {
        config: dir.path().join("config.yaml"),
        pairing_json: dir.path().join("pairing.json"),
        wfb_key_dir: dir.path().join("wfb"),
        bind_state: dir.path().join("bind-state.json"),
        profile_conf: dir.path().join("profile.conf"),
        mesh_role: dir.path().join("mesh-role"),
        relay_secret: dir.path().join("relay-peer-secret"),
    };
    let state = AppState::new(
        pairing,
        stateipc,
        mavlink,
        logd,
        dir.path().join("board.json"),
        pairing_paths,
        std::sync::Arc::new(crate::dashboard_pin::DashboardPin::with_path(
            dir.path().join("dashboard-pin.json"),
        )),
        std::sync::Arc::new(crate::mcp::McpTokenStore::with_path(
            dir.path().join("mcp-token.json"),
        )),
    )
    .with_params_path(dir.path().join("params.json"));

    let resp = set_param(
        Path("WPNAV_SPEED".to_string()),
        State(state),
        Json(ParamSetRequest { value: 750.0 }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);

    // The server received a PARAM_SET frame for the right param + value.
    let frame = server.await.unwrap();
    let (_h, decoded) = mavlink::parse_v2(&frame).unwrap();
    let d = match decoded {
        MavMessage::PARAM_SET(d) => d,
        other => panic!("expected PARAM_SET on the socket, got {other:?}"),
    };
    assert_eq!(param_name(&d), "WPNAV_SPEED");
    assert_eq!(d.param_value, 750.0);

    // The body acks (the cache already held the target).
    let body = body_json(resp).await;
    assert_eq!(body["name"], json!("WPNAV_SPEED"));
    assert_eq!(body["value"], json!(750.0));
    assert_eq!(body["ack"], json!(true));
    assert_eq!(body["cached_value"], json!(750.0));
    assert_eq!(body["message"], json!(""));
}

/// Read the `{"detail"}` string out of a response body.
async fn body_detail(resp: Response) -> String {
    body_json(resp).await["detail"]
        .as_str()
        .unwrap()
        .to_string()
}

/// Read a response body as JSON.
async fn body_json(resp: Response) -> Value {
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}
