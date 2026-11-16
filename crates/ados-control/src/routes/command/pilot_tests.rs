//! `POST /api/command takeoff` while the pilot is live: the pilot's `launch`
//! in every control mode, with the requested height. Only without a live
//! pilot does the raw NAV_TAKEOFF go to the flight controller.

use super::*;
use crate::routes::nav::NavKind;
use ados_protocol::pilot;
use tokio::net::UnixListener;

/// A pilot listening under `run_dir` that accepts the first action and
/// returns the request it received as `{name, args}`.
fn fake_pilot(run_dir: &std::path::Path) -> tokio::task::JoinHandle<Value> {
    let listener = UnixListener::bind(run_dir.join(ados_protocol::paths::PILOT_SOCK)).unwrap();
    tokio::spawn(async move {
        let (stream, _addr) = listener.accept().await.unwrap();
        let (mut rd, mut wr) = stream.into_split();
        let body = pilot::read_msg(&mut rd).await.unwrap().unwrap();
        let pilot::Request::Action(req) = pilot::decode_request(&body).unwrap() else {
            panic!("expected an action request");
        };
        let reply = pilot::encode_op(
            "action_result",
            &json!({"name": req.name, "id": req.id, "ok": true}),
        )
        .unwrap();
        pilot::write_msg(&mut wr, &reply).await.unwrap();
        json!({"name": req.name, "args": req.args})
    })
}

/// An FC-connected ArduCopter state rooted at `dir`, with the pilot's live
/// control mode `mode` (`None`: no pilot state).
fn copter_state(dir: &std::path::Path, mode: Option<&str>) -> AppState {
    let state =
        super::state_with_socket(dir, dir.join("mavlink.sock")).with_run_dir(dir.to_path_buf());
    state.state.set_snapshot_for_test(json!({
        "fc_connected": true,
        "autopilot": 3,
        "vehicle_type": 2,
    }));
    if let Some(mode) = mode {
        state
            .nav
            .ingest_host(NavKind::Pilot, 1, json!({"control_mode": mode}));
    }
    state
}

async fn takeoff(state: AppState, args: Vec<Value>) -> (StatusCode, Value) {
    let resp = execute_command(
        State(state),
        Json(CommandRequest {
            cmd: "takeoff".to_string(),
            args,
        }),
    )
    .await;
    let code = resp.status();
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (code, serde_json::from_slice(&body).unwrap())
}

#[tokio::test]
async fn takeoff_is_the_pilots_launch_in_every_control_mode() {
    for mode in ["ados", "fc", "gcs"] {
        let dir = tempfile::tempdir().unwrap();
        let pilot = fake_pilot(dir.path());
        let (code, body) = takeoff(copter_state(dir.path(), Some(mode)), vec![json!(5.0)]).await;
        assert_eq!(code, StatusCode::OK, "{mode}: {body}");
        assert_eq!(body["status"], "ok", "{mode}: {body}");
        assert_eq!(body["via"], "pilot", "{mode}: {body}");
        assert_eq!(body["action"], "launch", "{mode}: {body}");
        let sent = pilot.await.unwrap();
        assert_eq!(
            sent,
            json!({"name": "launch", "args": {"after": "hold", "alt_m": 5.0}}),
            "{mode}"
        );
    }
}

/// With no height asked for, the pilot picks its own launch height for the
/// vehicle (a fixed-wing climbs to its own take-off altitude).
#[tokio::test]
async fn takeoff_without_a_height_leaves_it_to_the_pilot() {
    let dir = tempfile::tempdir().unwrap();
    let pilot = fake_pilot(dir.path());
    let (code, body) = takeoff(copter_state(dir.path(), Some("fc")), vec![]).await;
    assert_eq!(code, StatusCode::OK, "{body}");
    assert_eq!(
        pilot.await.unwrap(),
        json!({"name": "launch", "args": {"after": "hold"}})
    );
}

/// A height the raw path would refuse is refused before the pilot is asked.
#[tokio::test]
async fn an_unflyable_takeoff_height_is_refused_before_the_pilot() {
    let dir = tempfile::tempdir().unwrap();
    let (code, _body) = takeoff(copter_state(dir.path(), Some("ados")), vec![json!(-3.0)]).await;
    assert_eq!(code, StatusCode::BAD_REQUEST);
}

/// No live pilot: the raw NAV_TAKEOFF with the height in `param7`.
#[tokio::test]
async fn takeoff_without_a_pilot_is_the_raw_nav_takeoff() {
    let dir = tempfile::tempdir().unwrap();
    let listener = UnixListener::bind(dir.path().join("mavlink.sock")).unwrap();
    let fc = tokio::spawn(async move {
        let (stream, _addr) = listener.accept().await.unwrap();
        let (mut rd, _wr) = stream.into_split();
        let mut hdr = [0u8; 4];
        rd.read_exact(&mut hdr).await.unwrap();
        let mut body = vec![0u8; u32::from_be_bytes(hdr) as usize];
        rd.read_exact(&mut body).await.unwrap();
        body
    });
    let state = copter_state(dir.path(), None);
    let route = tokio::spawn(takeoff(state, vec![json!(5.0)]));
    let (_h, decoded) = mavlink::parse_v2(&fc.await.unwrap()).unwrap();
    let MavMessage::COMMAND_LONG(sent) = decoded else {
        panic!("expected COMMAND_LONG on the socket");
    };
    assert_eq!(sent.command, MavCmd::MAV_CMD_NAV_TAKEOFF);
    assert_eq!(sent.param7, 5.0);
    route.abort();
}
