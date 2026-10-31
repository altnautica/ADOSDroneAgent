use super::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

/// A role channel whose loop is gone, for the bind-op tests.
fn no_roles() -> mpsc::Sender<RoleRequest> {
    mpsc::channel(1).0
}

#[tokio::test]
async fn dispatch_status_when_idle_is_null_session() {
    let orch = Arc::new(BindOrchestrator::new());
    let v = dispatch(br#"{"op":"bind_status"}"#, &orch, &no_roles()).await;
    assert_eq!(v["ok"], true);
    assert!(v["session"].is_null());
}

#[tokio::test]
async fn dispatch_cancel_is_ok_when_idle() {
    let orch = Arc::new(BindOrchestrator::new());
    let v = dispatch(br#"{"op":"cancel_bind"}"#, &orch, &no_roles()).await;
    assert_eq!(v["ok"], true);
}

#[tokio::test]
async fn dispatch_bad_json_and_bad_op_and_bad_role() {
    let orch = Arc::new(BindOrchestrator::new());
    assert_eq!(dispatch(b"not json", &orch, &no_roles()).await["ok"], false);
    assert_eq!(
        dispatch(br#"{"op":"frob"}"#, &orch, &no_roles()).await["ok"],
        false
    );
    let bad_role = dispatch(br#"{"op":"start_bind","role":"bogus"}"#, &orch, &no_roles()).await;
    assert_eq!(bad_role["ok"], false);
    assert_eq!(bad_role["error"], "E_BAD_ROLE");
}

#[tokio::test]
async fn dispatch_start_bind_drone_fails_preflight_off_rig() {
    // No /etc/bind.key on the dev host → the FSM lands on "failed", and the
    // op still returns ok:true with the terminal session (a successful RPC
    // carrying a failed bind, which is what the REST route relays).
    let orch = Arc::new(BindOrchestrator::new());
    let v = dispatch(
        br#"{"op":"start_bind","role":"drone","source":"operator"}"#,
        &orch,
        &no_roles(),
    )
    .await;
    assert_eq!(v["ok"], true);
    assert_eq!(v["session"]["state"], "failed");
    assert_eq!(v["session"]["role"], "drone");
}

#[test]
fn a_fleet_assignment_needs_both_halves() {
    // A slot with no fleet is unaddressable and a fleet with no slot leaves
    // a drone parked, so a half-filled request must write NOTHING rather
    // than stamp half an identity over a working one.
    assert_eq!(
        fleet_from_request(Some(2), Some(7)),
        Some(FleetIdentity {
            fleet_id: 2,
            fleet_slot: 7
        })
    );
    assert_eq!(fleet_from_request(Some(2), None), None);
    assert_eq!(fleet_from_request(None, Some(7)), None);
    assert_eq!(fleet_from_request(None, None), None);
}

#[test]
fn the_start_bind_request_parses_the_fleet_assignment() {
    // The wire seam: the GCS forwards the slot the ground station's registry
    // just issued, and an older caller that omits both fields still parses.
    let with: Request = serde_json::from_slice(
        br#"{"op":"start_bind","role":"drone","fleet_id":4,"fleet_slot":9}"#,
    )
    .unwrap();
    assert_eq!(
        fleet_from_request(with.fleet_id, with.fleet_slot),
        Some(FleetIdentity {
            fleet_id: 4,
            fleet_slot: 9
        })
    );
    let without: Request =
        serde_json::from_slice(br#"{"op":"start_bind","role":"drone"}"#).unwrap();
    assert_eq!(
        fleet_from_request(without.fleet_id, without.fleet_slot),
        None
    );
}

#[tokio::test(start_paused = true)]
async fn a_socket_that_fails_to_bind_is_retried_until_it_binds() {
    // The parent of the socket path is a regular file, so the first bind
    // fails. Once the obstacle is gone the next fixed-interval attempt must
    // bind: a supervisor that gave up here could not be paired until it was
    // restarted by hand.
    let dir = tempfile::tempdir().unwrap();
    let parent = dir.path().join("run");
    std::fs::write(&parent, b"not a directory").unwrap();
    let sock = parent.join("supervisor.sock");
    let server = tokio::spawn({
        let sock = sock.clone();
        async move { serve(Arc::new(BindOrchestrator::new()), no_roles(), &sock).await }
    });
    tokio::task::yield_now().await;
    assert!(!sock.exists());

    std::fs::remove_file(&parent).unwrap();
    std::fs::create_dir(&parent).unwrap();
    tokio::time::advance(SOCKET_BIND_RETRY + std::time::Duration::from_millis(10)).await;
    for _ in 0..50 {
        if sock.exists() {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert!(sock.exists(), "the retry must bind once the path is usable");
    server.abort();
}

#[tokio::test]
async fn end_to_end_socket_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("supervisor.sock");
    let orch = Arc::new(BindOrchestrator::new());
    let server = tokio::spawn({
        let sock = sock.clone();
        async move { serve(orch, no_roles(), &sock).await }
    });
    // Wait for the socket file to appear (bind happens inside serve()).
    for _ in 0..50 {
        if sock.exists() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let mut client = UnixStream::connect(&sock).await.unwrap();
    client
        .write_all(b"{\"op\":\"bind_status\"}\n")
        .await
        .unwrap();
    let mut resp = Vec::new();
    // Read until the server writes its newline-terminated reply + closes.
    let mut chunk = [0u8; 256];
    loop {
        let n = client.read(&mut chunk).await.unwrap();
        if n == 0 {
            break;
        }
        resp.extend_from_slice(&chunk[..n]);
        if resp.contains(&b'\n') {
            break;
        }
    }
    let v: Value = serde_json::from_slice(resp.split(|&b| b == b'\n').next().unwrap()).unwrap();
    assert_eq!(v["ok"], true);
    assert!(v["session"].is_null());
    server.abort();
}
