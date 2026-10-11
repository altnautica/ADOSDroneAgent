use super::*;
use axum::body::to_bytes;
use std::io::Write;

/// Write a config.yaml carrying an explicit `agent.profile` into a tempdir and
/// return its path. An explicit value resolves straight to the wire profile
/// without consulting profile.conf, so the gate is deterministic with no env.
fn config_with_profile(dir: &Path, profile: &str) -> PathBuf {
    let cfg = dir.join("config.yaml");
    let mut f = std::fs::File::create(&cfg).unwrap();
    write!(f, "agent:\n  profile: {profile}\n").unwrap();
    cfg
}

async fn body_json(resp: Response) -> (StatusCode, Value) {
    let status = resp.status();
    let bytes = to_bytes(resp.into_body(), 1024 * 1024).await.unwrap();
    let value: Value = serde_json::from_slice(&bytes).unwrap();
    (status, value)
}

// -------------------------------------------------------------------
// Profile gate: drone resolves to not-ground-station, ground_station
// resolves to ground-station. Path-injectable, no env mutation.
// -------------------------------------------------------------------

#[test]
fn drone_profile_is_not_a_ground_station() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = config_with_profile(dir.path(), "drone");
    assert!(!is_ground_station_at(
        &cfg,
        &dir.path().join("absent.conf"),
        &dir.path().join("absent.role"),
    ));
}

#[test]
fn ground_station_profile_is_a_ground_station() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = config_with_profile(dir.path(), "ground_station");
    assert!(is_ground_station_at(
        &cfg,
        &dir.path().join("absent.conf"),
        &dir.path().join("absent.role"),
    ));
}

#[test]
fn an_absent_config_is_not_a_ground_station() {
    // A missing config loads the all-defaults (`profile: auto`); with no
    // profile.conf, `auto` falls back to the drone default → not a GS.
    assert!(!is_ground_station_at(
        Path::new("/nonexistent/ados/config.yaml"),
        Path::new("/nonexistent/ados/profile.conf"),
        Path::new("/nonexistent/ados/mesh-role"),
    ));
}

// -------------------------------------------------------------------
// Profile-mismatch error shape: the nested 404 body every route returns
// for a drone-profile node.
// -------------------------------------------------------------------

#[tokio::test]
async fn profile_mismatch_is_the_nested_404_body() {
    let (status, body) = body_json(profile_mismatch()).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(
        body,
        json!({ "detail": { "error": { "code": "E_PROFILE_MISMATCH" } } })
    );
}

// -------------------------------------------------------------------
// Golden-fixture parity: the steady-state ground-station bodies.
// -------------------------------------------------------------------

#[tokio::test]
async fn pair_pending_503s_when_the_daemon_socket_is_absent() {
    let dir = tempfile::tempdir().unwrap();
    // An absent daemon socket: the round-trip fails, so the route reports
    // the daemon-unavailable status rather than a closed window.
    let socket = dir.path().join("pairing.sock");
    let (status, body) = body_json(pair_pending_body(&socket).await).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        body,
        json!({ "detail": { "error": { "code": "E_PAIR_DAEMON_UNAVAILABLE" } } })
    );
}

#[tokio::test]
async fn pair_pending_relays_a_daemon_snapshot_with_an_open_window() {
    // Stand up a one-shot daemon stub on a Unix socket that answers the
    // snapshot op with an open-window result, and assert the route relays the
    // inner `result` object verbatim.
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("pairing.sock");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    let server = tokio::spawn(async move {
        let (mut conn, _) = listener.accept().await.unwrap();
        // Read the request line (we don't need to parse it for the stub).
        let mut buf = [0u8; 256];
        let _ = conn.read(&mut buf).await.unwrap();
        let reply = "{\"ok\":true,\"result\":{\"open\":true,\"opened_at_ms\":100,\"closes_at_ms\":160,\"pending\":[],\"approvals\":{}}}\n";
        conn.write_all(reply.as_bytes()).await.unwrap();
        conn.flush().await.unwrap();
    });

    let (status, body) = body_json(pair_pending_body(&socket).await).await;
    server.await.unwrap();
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body,
        json!({
            "open": true,
            "opened_at_ms": 100,
            "closes_at_ms": 160,
            "pending": [],
            "approvals": {},
        })
    );
}

/// A fake pairing daemon answering each op with `answer(op)`.
fn fake_daemon(dir: &Path, answer: fn(&str) -> Value) -> PathBuf {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let path = dir.join("pairing.sock");
    let listener = tokio::net::UnixListener::bind(&path).unwrap();
    tokio::spawn(async move {
        while let Ok((conn, _)) = listener.accept().await {
            let mut conn = BufReader::new(conn);
            let mut line = String::new();
            conn.read_line(&mut line).await.unwrap();
            let req: Value = serde_json::from_str(line.trim()).unwrap();
            let mut out = serde_json::to_vec(&answer(req["op"].as_str().unwrap())).unwrap();
            out.push(b'\n');
            conn.get_mut().write_all(&out).await.unwrap();
        }
    });
    path
}

#[test]
fn window_writes_are_receiver_only() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = config_with_profile(dir.path(), "ground_station");
    let role = dir.path().join("role");
    std::fs::write(&role, "relay\n").unwrap();
    let refused = receiver_gate(&cfg, &dir.path().join("profile.conf"), &role).unwrap();
    assert_eq!(refused.status(), StatusCode::CONFLICT);
    std::fs::write(&role, "receiver\n").unwrap();
    assert!(receiver_gate(&cfg, &dir.path().join("profile.conf"), &role).is_none());
    let drone = config_with_profile(dir.path(), "drone");
    let refused = receiver_gate(&drone, &dir.path().join("profile.conf"), &role).unwrap();
    assert_eq!(refused.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn accept_relays_the_window_code_and_a_refusal_is_a_503() {
    let dir = tempfile::tempdir().unwrap();
    let sock = fake_daemon(
        dir.path(),
        |_| json!({"ok": true, "result": {"opened_at_ms": 1000, "closes_at_ms": 61000, "code": "123456"}}),
    );
    let (status, body) = body_json(pair_accept_at(&sock, 60).await).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body,
        json!({"opened_at_ms": 1000, "closes_at_ms": 61000, "duration_s": 60, "code": "123456"})
    );
    let (status, _) = body_json(pair_accept_at(&sock, 301).await).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

    let absent = dir.path().join("absent.sock");
    let (status, body) = body_json(pair_accept_at(&absent, 60).await).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        body["detail"]["error"]["code"],
        json!("E_PAIR_DAEMON_UNAVAILABLE")
    );
}

#[tokio::test]
async fn approve_maps_a_closed_window_and_an_unknown_request() {
    let dir = tempfile::tempdir().unwrap();
    let (a, b) = (dir.path().join("a"), dir.path().join("b"));
    std::fs::create_dir(&a).unwrap();
    std::fs::create_dir(&b).unwrap();
    let closed = fake_daemon(&a, |_| json!({"ok": true, "result": {"open": false}}));
    let (status, body) = body_json(pair_approve_at(&closed, "relay-1").await).await;
    assert_eq!(status, StatusCode::GONE);
    assert_eq!(
        body["detail"]["error"]["code"],
        json!("E_PAIR_WINDOW_EXPIRED")
    );

    let unknown = fake_daemon(&b, |op| match op {
        "is_window_open" => json!({"ok": true, "result": {"open": true}}),
        _ => json!({"ok": false, "error": "request not found"}),
    });
    let (status, body) = body_json(pair_approve_at(&unknown, "relay-1").await).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(
        body["detail"]["error"]["code"],
        json!("E_PAIR_REQUEST_NOT_FOUND")
    );

    let c = dir.path().join("c");
    std::fs::create_dir(&c).unwrap();
    let no_invite = fake_daemon(&c, |op| match op {
        "is_window_open" => json!({"ok": true, "result": {"open": true}}),
        _ => json!({"ok": true, "result": {"invite_blob_hex": "", "issued_at_ms": 1}}),
    });
    let (status, body) = body_json(pair_approve_at(&no_invite, "relay-1").await).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        body["detail"]["error"]["code"],
        json!("E_PAIR_DAEMON_UNAVAILABLE")
    );
}

#[test]
fn a_receiver_cannot_join_a_mesh_but_a_direct_node_can() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = config_with_profile(dir.path(), "ground_station");
    let role = dir.path().join("role");
    std::fs::write(&role, "receiver\n").unwrap();
    let refused = join_gate(&cfg, &dir.path().join("profile.conf"), &role).unwrap();
    assert_eq!(refused.status(), StatusCode::CONFLICT);
    std::fs::write(&role, "direct\n").unwrap();
    assert!(join_gate(&cfg, &dir.path().join("profile.conf"), &role).is_none());
}

#[tokio::test]
async fn a_join_relays_the_mesh_and_carries_the_daemon_error_code() {
    let dir = tempfile::tempdir().unwrap();
    let (a, b) = (dir.path().join("a"), dir.path().join("b"));
    std::fs::create_dir(&a).unwrap();
    std::fs::create_dir(&b).unwrap();
    let req = PairJoinRequest {
        code: "123456".into(),
        receiver_host: None,
        receiver_port: None,
    };
    let joined = fake_daemon(&a, |op| {
        assert_eq!(op, "join");
        json!({"ok": true, "result": {"mesh_id": "mesh-1", "receiver_host": "gs-a.local"}})
    });
    let (status, body) = body_json(pair_join_at(&joined, &req).await).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body,
        json!({"mesh_id": "mesh-1", "receiver_host": "gs-a.local", "ok": true})
    );

    let refused = fake_daemon(
        &b,
        |_| json!({"ok": false, "error": "bad code", "error_code": "E_INVITE_DECRYPT"}),
    );
    let (status, body) = body_json(pair_join_at(&refused, &req).await).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body["detail"]["error"]["code"], json!("E_INVITE_DECRYPT"));

    let bad = PairJoinRequest {
        code: "12a456".into(),
        receiver_host: None,
        receiver_port: None,
    };
    let (status, _) = body_json(pair_join_at(&refused, &bad).await).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
}
