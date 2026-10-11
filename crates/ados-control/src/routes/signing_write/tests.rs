use super::*;

#[test]
fn disable_clears_the_fc_the_caller_names() {
    let named = DisableRequest {
        target_system: Some(7),
        target_component: Some(1),
    };
    assert_eq!(disable_target(Some(named), (1, 1)), Ok((7, 1)));
    // No body: the resolved FC target.
    assert_eq!(disable_target(None, (9, 1)), Ok((9, 1)));
    // A partial body falls back field by field.
    let partial = DisableRequest {
        target_system: None,
        target_component: Some(2),
    };
    assert_eq!(disable_target(Some(partial), (9, 1)), Ok((9, 2)));
    let bad = DisableRequest {
        target_system: Some(0),
        target_component: Some(1),
    };
    assert!(disable_target(Some(bad), (1, 1)).is_err());
}
use serde_json::Value;
use tokio::io::AsyncReadExt;
use tokio::net::UnixListener;

// ── hex parsing: parity with the Python parse_key_hex messages ────────────

#[test]
fn parse_key_hex_accepts_a_64_char_lowercase_key() {
    let hex = "00".repeat(32);
    let bytes = parse_key_hex(&hex).unwrap();
    assert_eq!(bytes.len(), 32);
    assert!(bytes.iter().all(|b| *b == 0));
}

#[test]
fn parse_key_hex_round_trips_a_known_value() {
    let hex = format!("0102{}", "ff".repeat(30));
    let bytes = parse_key_hex(&hex).unwrap();
    assert_eq!(bytes[0], 0x01);
    assert_eq!(bytes[1], 0x02);
    assert_eq!(bytes[2], 0xff);
}

#[test]
fn parse_key_hex_wrong_length_is_the_python_message() {
    let err = parse_key_hex("00").unwrap_err();
    assert_eq!(err, "key_hex must be 64 hex chars, got 2");
    // A too-long input also reports its actual length.
    let long = "0".repeat(65);
    assert_eq!(
        parse_key_hex(&long).unwrap_err(),
        "key_hex must be 64 hex chars, got 65"
    );
}

#[test]
fn parse_key_hex_non_hex_is_the_python_message_prefix() {
    // 64 chars but with a non-hex digit: the message carries the "not valid
    // hex:" prefix the Python route surfaces from binascii.
    let bad = format!("0g{}", "0".repeat(62));
    let err = parse_key_hex(&bad).unwrap_err();
    assert!(
        err.starts_with("key_hex is not valid hex:"),
        "unexpected: {err}"
    );
}

// ── fingerprint: parity with the Python _fingerprint ──────────────────────

#[test]
fn fingerprint_is_first_eight_hex_of_sha256() {
    // sha256("") = e3b0c44298fc1c14... → first 8 = "e3b0c442".
    assert_eq!(fingerprint(b""), "e3b0c442");
    // A 32-zero-byte key has a fixed, stable fingerprint.
    let fp = fingerprint(&[0u8; 32]);
    assert_eq!(fp.len(), 8);
    assert!(fp.chars().all(|c| c.is_ascii_hexdigit()));
}

// ── bounded coercion: parity with the Pydantic field validators ───────────

#[test]
fn bounded_u8_enforces_the_range() {
    assert_eq!(bounded_u8(1, 1, 255), Ok(1));
    assert_eq!(bounded_u8(255, 1, 255), Ok(255));
    assert_eq!(bounded_u8(0, 0, 255), Ok(0));
    // Out of range (0 below a min-1, 256 above the max).
    assert_eq!(bounded_u8(0, 1, 255), Err(()));
    assert_eq!(bounded_u8(256, 0, 255), Err(()));
    assert_eq!(bounded_u8(-1, 0, 255), Err(()));
}

// ── timestamp: parity with the Python _initial_timestamp_10us ─────────────

#[test]
fn initial_timestamp_is_ten_us_since_2015() {
    // At exactly the 2015 epoch the timestamp is 0; one second later is
    // 100_000 (10-µs units), matching the Python `* 100_000` scaling.
    let epoch_2015 = OffsetDateTime::from_unix_timestamp(EPOCH_2015_UNIX_SECONDS).unwrap();
    assert_eq!(initial_timestamp_10us(epoch_2015), 0);
    let one_second_later =
        OffsetDateTime::from_unix_timestamp(EPOCH_2015_UNIX_SECONDS + 1).unwrap();
    assert_eq!(initial_timestamp_10us(one_second_later), 100_000);
}

// ── SETUP_SIGNING frame: the wire bytes carry the key + the identity ──────

#[test]
fn setup_signing_frame_round_trips_with_the_key_and_identity() {
    let key: Vec<u8> = (0u8..32).collect();
    let frame = build_setup_signing_frame(1, 1, &key, 123).unwrap();
    // A v2 frame starts with the 0xFD magic byte.
    assert_eq!(frame[0], 0xFD);
    let (header, msg) = mavlink::parse_v2(&frame).unwrap();
    // The signing source identity is on the wire (255 / 190).
    assert_eq!(header.system_id, 255);
    assert_eq!(header.component_id, 190);
    match msg {
        MavMessage::SETUP_SIGNING(d) => {
            assert_eq!(d.target_system, 1);
            assert_eq!(d.target_component, 1);
            assert_eq!(d.initial_timestamp, 123);
            assert_eq!(d.secret_key.to_vec(), key);
        }
        other => panic!("expected SETUP_SIGNING, got {other:?}"),
    }
}

#[test]
fn disable_frame_is_setup_signing_all_zero() {
    let zero = [0u8; 32];
    let frame = build_setup_signing_frame(1, 1, &zero, 0).unwrap();
    let (_h, msg) = mavlink::parse_v2(&frame).unwrap();
    match msg {
        MavMessage::SETUP_SIGNING(d) => {
            assert_eq!(d.initial_timestamp, 0);
            assert!(d.secret_key.iter().all(|b| *b == 0));
        }
        other => panic!("expected SETUP_SIGNING, got {other:?}"),
    }
}

// ── ISO-8601 seconds precision: parity with isoformat(timespec="seconds") ─

#[test]
fn enrolled_at_matches_python_isoformat_seconds() {
    // A fixed UTC instant renders as the Python isoformat(timespec="seconds")
    // shape: zero-padded date + time and the explicit +00:00 offset, no
    // fractional seconds, never a `Z`.
    let dt = OffsetDateTime::from_unix_timestamp(1_780_000_000).unwrap();
    let got = iso8601_seconds_utc(dt);
    // 1780000000 → 2026-05-28T20:26:40 UTC.
    assert_eq!(got, "2026-05-28T20:26:40+00:00");
    // The shape is fixed-width with the +00:00 offset and no subsecond part.
    assert!(got.ends_with("+00:00"));
    assert!(!got.contains('.'));
    assert!(!got.contains('Z'));
}

// ── the write-path tests: against a mock mavlink socket ───────────────────

/// Build an AppState whose mavlink client points at the given socket path and
/// whose state snapshot reports the FC connected (so the gate passes). The
/// other seams are inert defaults the signing-write routes never touch.
fn state_with_mavlink(sock: std::path::PathBuf, fc_connected: bool) -> AppState {
    use crate::auth::PairingState;
    use crate::ipc::{LogdQueryClient, MavlinkIpcClient, StateIpcClient};
    use crate::state::PairingPaths;
    use std::sync::Arc;

    let dir = sock.parent().unwrap().to_path_buf();
    let state_client = StateIpcClient::disconnected();
    if fc_connected {
        state_client.set_snapshot_for_test(json!({ "fc_connected": true }));
    }
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
        state_client,
        MavlinkIpcClient::new(sock),
        LogdQueryClient::new(dir.join("logd-query.sock")),
        dir.join("board.json"),
        pairing_paths,
        std::sync::Arc::new(crate::dashboard_pin::DashboardPin::with_path(
            dir.join("dashboard-pin.json"),
        )),
        std::sync::Arc::new(crate::mcp::McpTokenStore::with_path(
            dir.join("mcp-token.json"),
        )),
    )
}

/// A fake router: accepts ONE connection, reads `n` length-prefixed frames
/// from it, broadcasts `replies` back on the same connection, then closes it.
/// Returns the frames it read.
fn fake_router(
    listener: UnixListener,
    n: usize,
    replies: Vec<Vec<u8>>,
) -> tokio::task::JoinHandle<Vec<Vec<u8>>> {
    use tokio::io::AsyncWriteExt;
    tokio::spawn(async move {
        let (mut conn, _addr) = listener.accept().await.unwrap();
        let mut frames = Vec::with_capacity(n);
        for _ in 0..n {
            frames.push(read_framed(&mut conn).await);
        }
        for reply in replies {
            let wire =
                ados_protocol::frame::encode_frame(&reply, ados_protocol::frame::MAVLINK_MAX_FRAME)
                    .unwrap();
            conn.write_all(&wire).await.unwrap();
        }
        frames
    })
}

/// Read one length-prefixed frame (the 4-byte big-endian prefix + the payload)
/// off a connected Unix stream, returning the raw payload bytes.
async fn read_framed(conn: &mut tokio::net::UnixStream) -> Vec<u8> {
    use ados_protocol::frame::{decode_len, HEADER_SIZE, MAVLINK_MAX_FRAME};
    let mut header = [0u8; HEADER_SIZE];
    conn.read_exact(&mut header).await.unwrap();
    let len = decode_len(header, MAVLINK_MAX_FRAME, false).unwrap();
    let mut body = vec![0u8; len];
    conn.read_exact(&mut body).await.unwrap();
    body
}

/// An unsigned MAVLink 2 HEARTBEAT from the FC at `system`.
fn fc_heartbeat(system: u8) -> Vec<u8> {
    use ados_protocol::mavlink::ardupilotmega::{
        MavAutopilot, MavModeFlag, MavState, MavType, HEARTBEAT_DATA,
    };
    let header = MavHeader {
        system_id: system,
        component_id: 1,
        sequence: 0,
    };
    let msg = MavMessage::HEARTBEAT(HEARTBEAT_DATA {
        custom_mode: 0,
        mavtype: MavType::MAV_TYPE_QUADROTOR,
        autopilot: MavAutopilot::MAV_AUTOPILOT_ARDUPILOTMEGA,
        base_mode: MavModeFlag::empty(),
        system_status: MavState::MAV_STATE_STANDBY,
        mavlink_version: 3,
    });
    mavlink::serialize_v2(header, &msg).unwrap()
}

/// `frame` with the signed flag set and a signature block under `key`
/// appended. The checksum is left as it was; the route checks the signature,
/// which is what proves the key.
fn sign(mut frame: Vec<u8>, key: &[u8]) -> Vec<u8> {
    frame[2] |= IFLAG_SIGNED;
    frame.push(0); // link id
    frame.extend_from_slice(&[1, 0, 0, 0, 0, 0]); // timestamp
    let digest = Sha256::new()
        .chain_update(key)
        .chain_update(&frame)
        .finalize();
    frame.extend_from_slice(&digest[..V2_SIGNATURE_LEN]);
    frame
}

fn enroll_body(key: &[u8]) -> EnrollRequest {
    EnrollRequest {
        key_hex: key.iter().map(|b| format!("{b:02x}")).collect(),
        link_id: 0,
        target_system: Some(1),
        target_component: Some(1),
        agent_holds_key: false,
    }
}

#[tokio::test]
async fn enroll_is_verified_by_a_frame_signed_with_the_new_key() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("mavlink.sock");
    let listener = UnixListener::bind(&sock).unwrap();
    let key: Vec<u8> = (0u8..32).collect();
    let other_key = [7u8; 32];
    // Before the key lands the FC is unsigned or signs with an older key;
    // only a frame signed with THIS key proves the enrollment.
    let server = fake_router(
        listener,
        2,
        vec![
            fc_heartbeat(1),
            sign(fc_heartbeat(1), &other_key),
            sign(fc_heartbeat(1), &key),
        ],
    );

    let state = state_with_mavlink(sock.clone(), true);
    let body = enroll_body(&key);
    let handle = tokio::spawn(async move { enroll_fc(State(state), Json(body)).await });

    let frames = server.await.unwrap();
    assert_eq!(frames.len(), 2, "the enroll sends the frame twice");
    // The two frames are byte-identical (the key is encoded once, sent twice).
    assert_eq!(frames[0], frames[1]);

    let resp = handle.await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let value = body_json(resp).await;
    assert_eq!(value["sent"], json!(true));
    assert_eq!(value["verified"], json!(true));
    assert!(value.get("success").is_none());
    // The fingerprint is the first 8 hex of sha256(key), never the key.
    assert_eq!(value["key_id"], json!(fingerprint(&key)));
    // enrolled_at is the ISO-8601 seconds-precision UTC string.
    let enrolled = value["enrolled_at"].as_str().unwrap();
    assert!(enrolled.contains('T'));
    assert!(enrolled.ends_with("+00:00"));

    // The frame on the wire is a SETUP_SIGNING carrying the key + identity.
    let (header, msg) = mavlink::parse_v2(&frames[0]).unwrap();
    assert_eq!(header.system_id, 255);
    assert_eq!(header.component_id, 190);
    match msg {
        MavMessage::SETUP_SIGNING(d) => {
            assert_eq!(d.secret_key.to_vec(), key);
        }
        other => panic!("expected SETUP_SIGNING, got {other:?}"),
    }
}

#[tokio::test]
async fn enroll_is_unverified_when_no_frame_carries_the_new_key() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("mavlink.sock");
    let listener = UnixListener::bind(&sock).unwrap();
    let key: Vec<u8> = (0u8..32).collect();
    // Unsigned, signed with another key, and signed with this key by a
    // different vehicle: none of them shows THIS FC holding THIS key.
    let server = fake_router(
        listener,
        2,
        vec![
            fc_heartbeat(1),
            sign(fc_heartbeat(1), &[7u8; 32]),
            sign(fc_heartbeat(2), &key),
        ],
    );
    let state = state_with_mavlink(sock, true);
    let resp = enroll_fc(State(state), Json(enroll_body(&key))).await;
    server.await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let value = body_json(resp).await;
    assert_eq!(value["sent"], json!(true));
    assert_eq!(value["verified"], json!(false));
}

#[tokio::test]
async fn enroll_with_fc_disconnected_is_a_503() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("mavlink.sock");
    let state = state_with_mavlink(sock, false);
    let body = EnrollRequest {
        key_hex: "00".repeat(32),
        link_id: 0,
        target_system: Some(1),
        target_component: Some(1),
        agent_holds_key: false,
    };
    let resp = enroll_fc(State(state), Json(body)).await;
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    let value = body_json(resp).await;
    assert_eq!(value, json!({ "detail": "FC not connected" }));
}

#[tokio::test]
async fn enroll_with_a_bad_key_is_a_400_with_the_parse_message() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("mavlink.sock");
    let state = state_with_mavlink(sock, true);
    let body = EnrollRequest {
        key_hex: "deadbeef".to_string(), // too short
        link_id: 0,
        target_system: Some(1),
        target_component: Some(1),
        agent_holds_key: false,
    };
    let resp = enroll_fc(State(state), Json(body)).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let value = body_json(resp).await;
    assert_eq!(
        value,
        json!({ "detail": "key_hex must be 64 hex chars, got 8" })
    );
}

#[tokio::test]
async fn enroll_with_an_absent_socket_is_the_link_unavailable_503() {
    let dir = tempfile::tempdir().unwrap();
    // No listener bound → the send fails on the first frame.
    let sock = dir.path().join("absent.sock");
    let state = state_with_mavlink(sock, true);
    let body = EnrollRequest {
        key_hex: "00".repeat(32),
        link_id: 0,
        target_system: Some(1),
        target_component: Some(1),
        agent_holds_key: false,
    };
    let resp = enroll_fc(State(state), Json(body)).await;
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    let value = body_json(resp).await;
    assert_eq!(
        value,
        json!({ "detail": "MAVLink command link unavailable" })
    );
}

#[tokio::test]
async fn disable_is_verified_by_an_unsigned_frame_from_the_fc() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("mavlink.sock");
    let listener = UnixListener::bind(&sock).unwrap();
    let key = [3u8; 32];
    // A frame still in flight from before the clear is signed; the unsigned
    // one after it shows the store was cleared.
    let server = fake_router(
        listener,
        1,
        vec![sign(fc_heartbeat(1), &key), fc_heartbeat(1)],
    );

    let state = state_with_mavlink(sock, true);
    let resp = disable_on_fc(State(state), None).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let value = body_json(resp).await;
    assert_eq!(value, json!({ "sent": true, "verified": true }));

    let frames = server.await.unwrap();
    let (_h, msg) = mavlink::parse_v2(&frames[0]).unwrap();
    match msg {
        MavMessage::SETUP_SIGNING(d) => {
            assert_eq!(d.initial_timestamp, 0);
            assert!(d.secret_key.iter().all(|b| *b == 0));
        }
        other => panic!("expected SETUP_SIGNING, got {other:?}"),
    }
}

/// An enrolled FC drops the unsigned clear on a non-USB link and keeps
/// signing. The route must not report that as done.
#[tokio::test]
async fn disable_is_unverified_while_the_fc_still_signs() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("mavlink.sock");
    let listener = UnixListener::bind(&sock).unwrap();
    let key = [3u8; 32];
    let server = fake_router(
        listener,
        1,
        vec![sign(fc_heartbeat(1), &key), sign(fc_heartbeat(1), &key)],
    );
    let state = state_with_mavlink(sock, true);
    let resp = disable_on_fc(State(state), None).await;
    server.await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        body_json(resp).await,
        json!({ "sent": true, "verified": false })
    );
}

#[tokio::test]
async fn disable_with_fc_disconnected_is_a_503() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("mavlink.sock");
    let state = state_with_mavlink(sock, false);
    let resp = disable_on_fc(State(state), None).await;
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        body_json(resp).await,
        json!({ "detail": "FC not connected" })
    );
}

#[tokio::test]
async fn disable_with_an_absent_socket_is_the_link_unavailable_503() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("absent.sock");
    let state = state_with_mavlink(sock, true);
    let resp = disable_on_fc(State(state), None).await;
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        body_json(resp).await,
        json!({ "detail": "MAVLink command link unavailable" })
    );
}

/// Enrol with `agent_holds_key` against a config with `agent_signing` set
/// to `flag`; returns the response body and the key file path.
async fn enroll_holding(flag: bool, key: &[u8]) -> (Value, std::path::PathBuf, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("config.yaml"),
        format!("mavlink:\n  agent_signing: {flag}\n"),
    )
    .unwrap();
    let sock = dir.path().join("mavlink.sock");
    let server = fake_router(UnixListener::bind(&sock).unwrap(), 2, vec![]);
    let state = state_with_mavlink(sock, true);
    let mut body = enroll_body(key);
    body.agent_holds_key = true;
    let resp = enroll_fc(State(state), Json(body)).await;
    server.await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let path = dir.path().join(AGENT_KEY_FILE);
    (body_json(resp).await, path, dir)
}

#[tokio::test]
async fn enroll_with_agent_holds_key_writes_0600() {
    use std::os::unix::fs::PermissionsExt;
    let key: Vec<u8> = (100u8..132).collect();
    let (value, path, _dir) = enroll_holding(true, &key).await;
    assert_eq!(value["agent_holds_key"], json!(true));
    assert_eq!(std::fs::read(&path).unwrap(), key);
    let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
}

#[tokio::test]
async fn enroll_with_agent_signing_off_does_not_store_the_key() {
    let (value, path, _dir) = enroll_holding(false, &[9u8; 32]).await;
    assert_eq!(value["agent_holds_key"], json!(false));
    assert!(!path.exists());
}

#[tokio::test]
async fn disable_deletes_the_agent_held_key() {
    let dir = tempfile::tempdir().unwrap();
    let key_path = dir.path().join(AGENT_KEY_FILE);
    std::fs::write(&key_path, [5u8; 32]).unwrap();
    let sock = dir.path().join("mavlink.sock");
    let server = fake_router(UnixListener::bind(&sock).unwrap(), 1, vec![fc_heartbeat(1)]);
    let resp = disable_on_fc(State(state_with_mavlink(sock, true)), None).await;
    server.await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(!key_path.exists());
}

/// Read an axum response body into a JSON Value for the parity asserts.
async fn body_json(resp: Response) -> Value {
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}
