use super::*;

use std::sync::atomic::AtomicUsize;

use tokio::io::AsyncReadExt;
use tokio::net::UnixListener;

/// An `AppState` whose MAVLink client points at `sock`; every other path is
/// inert under `dir`.
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
}

/// A connected ArduPilot FC at system 7 that reports a fresh gimbal attitude.
fn with_gimbal(state: &AppState) {
    state.state.set_snapshot_for_test(json!({
        "fc_connected": true,
        "autopilot": 3,
        "fc_system_id": 7,
        "fc_component_id": 1,
        "gimbal": {"pitch_deg": -10.0, "yaw_deg": 0.0, "earth_frame": true, "age_ms": 40},
    }));
}

/// The seven params, the command id and the target of a `COMMAND_LONG` v2
/// frame, read straight off the wire (payload from byte 10; trailing zero
/// bytes may be truncated, so the tail is zero-padded first).
fn decode(frame: &[u8]) -> ([f32; 7], u16, u8, u8) {
    assert_eq!(frame[0], 0xFD, "v2 frame");
    assert_eq!(&frame[7..10], &[76, 0, 0], "COMMAND_LONG");
    let len = frame[1] as usize;
    let mut payload = frame[10..10 + len].to_vec();
    payload.resize(33, 0);
    let mut params = [0f32; 7];
    for (i, p) in params.iter_mut().enumerate() {
        *p = f32::from_le_bytes(payload[i * 4..i * 4 + 4].try_into().unwrap());
    }
    let cmd = u16::from_le_bytes([payload[28], payload[29]]);
    (params, cmd, payload[30], payload[31])
}

async fn body_json(resp: Response) -> Value {
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

fn req(pitch: f32, yaw: f32, hold: Option<u64>) -> Json<RateRequest> {
    Json(RateRequest {
        pitch_dps: pitch,
        yaw_dps: yaw,
        hold_ms: hold,
    })
}

#[test]
fn a_rate_is_a_pitchyaw_command_with_no_angle_target() {
    let (p, cmd, sys, comp) = decode(&rate_frame(-12.5, 30.0, (7, 1)));
    assert_eq!(cmd, 1000, "MAV_CMD_DO_GIMBAL_MANAGER_PITCHYAW");
    assert_eq!((sys, comp), (7, 1));
    assert!(
        p[0].is_nan() && p[1].is_nan(),
        "no pitch or yaw angle target"
    );
    assert_eq!((p[2], p[3]), (-12.5, 30.0), "pitch and yaw rates");
    assert_eq!(&p[4..], &[0.0, 0.0, 0.0], "no flags, every gimbal device");
}

#[test]
fn rates_and_holds_outside_their_bounds_are_refused() {
    let r = |pitch: f32, yaw: f32, hold: Option<u64>| {
        validate(&RateRequest {
            pitch_dps: pitch,
            yaw_dps: yaw,
            hold_ms: hold,
        })
    };
    assert_eq!(r(10.0, -10.0, None), Ok(Duration::from_millis(500)));
    assert_eq!(r(180.0, -180.0, Some(100)), Ok(Duration::from_millis(100)));
    assert!(r(f32::NAN, 0.0, None).is_err());
    assert!(r(0.0, f32::INFINITY, None).is_err());
    assert!(r(180.5, 0.0, None).is_err());
    assert!(r(0.0, 0.0, Some(99)).is_err());
    assert!(r(0.0, 0.0, Some(2001)).is_err());
}

/// A held pad keeps renewing and never stops; once the renewals end the stop
/// fires once, a hold window after the last; a zero rate cancels the pending
/// stop (it is the stop).
#[tokio::test(start_paused = true)]
async fn an_unrenewed_rate_is_stopped_after_its_hold() {
    let timer = HoldTimer::default();
    let stops = Arc::new(AtomicUsize::new(0));
    let arm = |moving: bool| {
        let stops = stops.clone();
        timer.renew(moving, Duration::from_millis(300), move || async move {
            stops.fetch_add(1, Ordering::SeqCst);
        });
    };
    let step = |ms: u64| tokio::time::sleep(Duration::from_millis(ms));
    for _ in 0..5 {
        arm(true);
        step(200).await;
    }
    assert_eq!(stops.load(Ordering::SeqCst), 0, "renewed in time");
    step(150).await;
    assert_eq!(stops.load(Ordering::SeqCst), 1, "stopped after the hold");
    step(1000).await;
    assert_eq!(stops.load(Ordering::SeqCst), 1, "stopped once");

    arm(true);
    step(100).await;
    arm(false);
    step(1000).await;
    assert_eq!(stops.load(Ordering::SeqCst), 1, "a zero rate is the stop");
}

/// The route sends the rate to the FC, then a zero rate once the hold runs
/// out without a renewal.
#[tokio::test]
async fn the_route_sends_the_rate_and_stops_it_when_the_hold_runs_out() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mavlink.sock");
    let listener = UnixListener::bind(&path).unwrap();
    let frames = tokio::spawn(async move {
        let mut out = Vec::new();
        for _ in 0..2 {
            let (mut s, _) = listener.accept().await.unwrap();
            let mut hdr = [0u8; 4];
            s.read_exact(&mut hdr).await.unwrap();
            let mut body = vec![0u8; u32::from_be_bytes(hdr) as usize];
            s.read_exact(&mut body).await.unwrap();
            out.push((tokio::time::Instant::now(), body));
        }
        out
    });
    let state = state_with_socket(dir.path(), path);
    with_gimbal(&state);
    let sent_at = tokio::time::Instant::now();
    let resp = post_gimbal_rate(State(state), req(-20.0, 15.0, Some(150))).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(
        body,
        json!({"sent": true, "accepted": null, "hold_ms": 150})
    );
    let got = tokio::time::timeout(Duration::from_secs(3), frames)
        .await
        .expect("the hold-expiry stop never arrived")
        .unwrap();
    let (p, cmd, sys, _) = decode(&got[0].1);
    assert_eq!((cmd, sys, p[2], p[3]), (1000, 7, -20.0, 15.0));
    let (p, cmd, _, _) = decode(&got[1].1);
    assert_eq!((cmd, p[2], p[3]), (1000, 0.0, 0.0), "zero-rate stop");
    assert!(
        got[1].0 - sent_at >= Duration::from_millis(150),
        "not before the hold"
    );
}

#[tokio::test]
async fn the_route_refuses_without_a_flight_controller_or_a_valid_rate() {
    let dir = tempfile::tempdir().unwrap();
    let state = state_with_socket(dir.path(), dir.path().join("absent.sock"));
    let resp = post_gimbal_rate(State(state.clone()), req(5.0, 0.0, None)).await;
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    let resp = post_gimbal_rate(State(state.clone()), req(500.0, 0.0, None)).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    with_gimbal(&state);
    // The FC is connected but the MAVLink socket is not there.
    let resp = post_gimbal_rate(State(state), req(5.0, 0.0, None)).await;
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn the_pad_is_offered_only_with_a_fresh_gimbal() {
    let dir = tempfile::tempdir().unwrap();
    let state = state_with_socket(dir.path(), dir.path().join("absent.sock"));
    let read = |s: AppState| async move { get_gimbal_rate(State(s)).await.0 };
    assert_eq!(read(state.clone()).await["available"], false);
    state
        .state
        .set_snapshot_for_test(json!({"fc_connected": true, "autopilot": 3}));
    let v = read(state.clone()).await;
    assert_eq!(v["available"], false);
    assert_eq!(v["reason"], "no gimbal reported by the flight controller");
    state.state.set_snapshot_for_test(json!({
        "fc_connected": true,
        "gimbal": {"pitch_deg": 0.0, "yaw_deg": 0.0, "earth_frame": true, "age_ms": 5000},
    }));
    assert_eq!(
        read(state.clone()).await["available"],
        false,
        "stale gimbal"
    );
    with_gimbal(&state);
    assert_eq!(
        read(state).await,
        json!({"available": true, "reason": null})
    );
}
