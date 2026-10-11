use super::*;
use axum::http::HeaderValue;

fn headers_with(pairs: &[(&str, &str)]) -> HeaderMap {
    let mut h = HeaderMap::new();
    for (k, v) in pairs {
        h.append(
            axum::http::HeaderName::from_bytes(k.as_bytes()).unwrap(),
            HeaderValue::from_str(v).unwrap(),
        );
    }
    h
}

// -- uplink frame from the live sentinel -------------------------------

#[tokio::test]
async fn uplink_frame_comes_from_the_sentinel_without_the_log_store() {
    // The durable log store is off by default; the stream must still emit
    // from the router's sentinel, which is the only source it reads.
    let dir = tempfile::tempdir().unwrap();
    let flag = dir.path().join("uplink-active");
    std::fs::write(
        &flag,
        json!({
            "active_uplink": "eth0",
            "internet_reachable": true,
            "timestamp_ms": 1234,
            "data_cap_state": "warn_80",
        })
        .to_string(),
    )
    .unwrap();
    let payload = uplink_ws_payload(&read_uplink_flag(&flag).await, Some(true));
    assert_eq!(
        payload,
        Some(json!({
            "kind": "health_changed",
            "active_uplink": "eth0",
            "available": [],
            "internet_reachable": true,
            "data_cap_state": "warn_80",
            "timestamp_ms": 1234,
            "stale": false,
        }))
    );
}

#[tokio::test]
async fn an_absent_sentinel_is_no_uplink_and_an_unreadable_one_sends_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let flag = dir.path().join("uplink-active");
    let none = uplink_ws_payload(&read_uplink_flag(&flag).await, Some(true)).unwrap();
    assert_eq!(none["active_uplink"], Value::Null);
    assert_eq!(none["internet_reachable"], json!(false));
    assert_eq!(none["data_cap_state"], Value::Null);
    assert_eq!(none["stale"], json!(false));

    std::fs::write(&flag, "{ torn").unwrap();
    assert!(uplink_ws_payload(&read_uplink_flag(&flag).await, Some(true)).is_none());
}

#[tokio::test]
async fn a_sentinel_left_by_a_stopped_router_is_stale() {
    let dir = tempfile::tempdir().unwrap();
    let flag = dir.path().join("uplink-active");
    std::fs::write(
        &flag,
        json!({"active_uplink": "wwan0", "internet_reachable": true}).to_string(),
    )
    .unwrap();
    let body = read_uplink_flag(&flag).await;
    // Stopped, or systemd gave no answer: the sentinel is not current.
    for router in [Some(false), None] {
        let frame = uplink_ws_payload(&body, router).unwrap();
        assert_eq!(frame["active_uplink"], json!("wwan0"));
        assert_eq!(frame["stale"], json!(true), "{router:?}");
    }
}

// -- subprotocol parsing ----------------------------------------------

#[test]
fn offered_subprotocols_flattens_comma_and_multi_header() {
    let h = headers_with(&[("sec-websocket-protocol", "ados-ws-ticket, v1|s|1|2|ff")]);
    assert_eq!(
        offered_subprotocols(&h),
        vec!["ados-ws-ticket".to_string(), "v1|s|1|2|ff".to_string()]
    );
    let mut multi = HeaderMap::new();
    multi.append(
        "sec-websocket-protocol",
        HeaderValue::from_static("ados-ws-ticket"),
    );
    multi.append(
        "sec-websocket-protocol",
        HeaderValue::from_static("v1|s|1|2|ff"),
    );
    assert_eq!(
        offered_subprotocols(&multi),
        vec!["ados-ws-ticket".to_string(), "v1|s|1|2|ff".to_string()]
    );
}

#[test]
fn extract_ticket_finds_the_value_after_the_marker() {
    let offered = vec!["ados-ws-ticket".to_string(), "v1|s|1|2|ff".to_string()];
    assert_eq!(extract_ticket(&offered), Some("v1|s|1|2|ff"));
    assert_eq!(extract_ticket(&["ados-ws-ticket".to_string()]), None);
    assert_eq!(extract_ticket(&["mavlink".to_string()]), None);
}

// -- auth decision ----------------------------------------------------

fn state_with_pairing(body: &str) -> (tempfile::TempDir, AppState) {
    use std::io::Write;
    let dir = tempfile::tempdir().unwrap();
    let pairing_json = dir.path().join("pairing.json");
    std::fs::File::create(&pairing_json)
        .unwrap()
        .write_all(body.as_bytes())
        .unwrap();
    let pairing = std::sync::Arc::new(crate::auth::PairingState::with_path(pairing_json.clone()));
    let paths = crate::state::PairingPaths {
        config: dir.path().join("config.yaml"),
        pairing_json,
        wfb_key_dir: dir.path().join("wfb"),
        bind_state: dir.path().join("bind-state.json"),
        profile_conf: dir.path().join("profile.conf"),
        mesh_role: dir.path().join("mesh-role"),
        relay_secret: dir.path().join("relay-peer-secret"),
    };
    let state = AppState::new(
        pairing,
        crate::ipc::StateIpcClient::disconnected(),
        crate::ipc::MavlinkIpcClient::new(dir.path().join("mavlink.sock")),
        crate::ipc::LogdQueryClient::new(dir.path().join("logd-query.sock")),
        dir.path().join("board.json"),
        paths,
        std::sync::Arc::new(crate::dashboard_pin::DashboardPin::with_path(
            dir.path().join("dashboard-pin.json"),
        )),
        std::sync::Arc::new(crate::mcp::McpTokenStore::with_path(
            dir.path().join("mcp-token.json"),
        )),
    );
    (dir, state)
}

#[test]
fn unpaired_admits_the_box_and_lifelines_but_the_lan_needs_a_pin_ticket() {
    let (_d, state) = state_with_pairing(r#"{"paired": false}"#);
    for caller in [CallerClass::OnBox, CallerClass::Lifeline] {
        assert!(
            matches!(
                decide_ws_auth(&state, &HeaderMap::new(), SCOPE_UPLINK_EVENTS, caller),
                WsAuth::AcceptPlain
            ),
            "{caller:?}"
        );
    }
    // Operator LAN, no ticket: refused, as every unpaired data route is
    // without a PIN session.
    assert!(matches!(
        decide_ws_auth(
            &state,
            &HeaderMap::new(),
            SCOPE_UPLINK_EVENTS,
            CallerClass::OperatorLan
        ),
        WsAuth::Reject
    ));
    // With a ticket keyed by the PIN salt: admitted.
    state.dashboard_pin.set_pin("1234", 0.0).unwrap();
    let salt = state.dashboard_pin.salt().unwrap();
    let offer = |token: String| {
        headers_with(&[(
            "sec-websocket-protocol",
            &format!("ados-ws-ticket, {token}"),
        )])
    };
    let token = WsTicketIssuer::issuer_unpaired(&salt)
        .mint(SCOPE_UPLINK_EVENTS, 30)
        .unwrap()
        .token;
    assert!(matches!(
        decide_ws_auth(
            &state,
            &offer(token),
            SCOPE_UPLINK_EVENTS,
            CallerClass::OperatorLan
        ),
        WsAuth::AcceptTicket
    ));
    // An empty-key ticket (anyone can mint one) opens nothing.
    let forged = WsTicketIssuer::from_api_key("")
        .mint(SCOPE_UPLINK_EVENTS, 30)
        .unwrap()
        .token;
    assert!(matches!(
        decide_ws_auth(
            &state,
            &offer(forged),
            SCOPE_UPLINK_EVENTS,
            CallerClass::OperatorLan
        ),
        WsAuth::Reject
    ));
}

#[test]
fn an_unpaired_node_refuses_a_remote_caller() {
    // Every data route refuses a remote caller while unpaired; these
    // streams used to be the exception.
    let (_d, state) = state_with_pairing(r#"{"paired": false}"#);
    assert!(matches!(
        decide_ws_auth(
            &state,
            &HeaderMap::new(),
            SCOPE_UPLINK_EVENTS,
            CallerClass::Remote
        ),
        WsAuth::Reject
    ));
}

#[test]
fn an_unreadable_pairing_file_serves_only_the_on_box_operator() {
    let (_d, state) = state_with_pairing("this is not json");
    for caller in [
        CallerClass::Remote,
        CallerClass::OperatorLan,
        CallerClass::Lifeline,
    ] {
        assert!(
            matches!(
                decide_ws_auth(&state, &HeaderMap::new(), SCOPE_MESH_EVENTS, caller),
                WsAuth::Reject
            ),
            "{caller:?}"
        );
    }
    assert!(matches!(
        decide_ws_auth(
            &state,
            &HeaderMap::new(),
            SCOPE_MESH_EVENTS,
            CallerClass::OnBox
        ),
        WsAuth::AcceptPlain
    ));
}

#[test]
fn a_ticket_opens_one_socket() {
    let (_d, state) = state_with_pairing(r#"{"paired": true, "api_key": "k"}"#);
    let token = WsTicketIssuer::from_api_key("k")
        .mint(SCOPE_PIC_EVENTS, 30)
        .unwrap()
        .token;
    let h = headers_with(&[(
        "sec-websocket-protocol",
        &format!("ados-ws-ticket, {token}"),
    )]);
    assert!(matches!(
        decide_ws_auth(&state, &h, SCOPE_PIC_EVENTS, CallerClass::OperatorLan),
        WsAuth::AcceptTicket
    ));
    assert!(matches!(
        decide_ws_auth(&state, &h, SCOPE_PIC_EVENTS, CallerClass::OperatorLan),
        WsAuth::Reject
    ));
}

#[test]
fn paired_admits_with_the_key_header() {
    let (_d, state) = state_with_pairing(r#"{"paired": true, "api_key": "k"}"#);
    let h = headers_with(&[("x-ados-key", "k")]);
    assert!(matches!(
        decide_ws_auth(&state, &h, SCOPE_UPLINK_EVENTS, CallerClass::OperatorLan),
        WsAuth::AcceptPlain
    ));
}

#[test]
fn paired_rejects_a_wrong_key_and_no_ticket() {
    let (_d, state) = state_with_pairing(r#"{"paired": true, "api_key": "k"}"#);
    let h = headers_with(&[("x-ados-key", "wrong")]);
    assert!(matches!(
        decide_ws_auth(&state, &h, SCOPE_UPLINK_EVENTS, CallerClass::OperatorLan),
        WsAuth::Reject
    ));
    // No credential at all is also rejected.
    assert!(matches!(
        decide_ws_auth(
            &state,
            &HeaderMap::new(),
            SCOPE_UPLINK_EVENTS,
            CallerClass::OperatorLan
        ),
        WsAuth::Reject
    ));
}

#[test]
fn paired_admits_with_a_valid_scoped_ticket() {
    let (_d, state) = state_with_pairing(r#"{"paired": true, "api_key": "k"}"#);
    let token = WsTicketIssuer::from_api_key("k")
        .mint(SCOPE_UPLINK_EVENTS, 30)
        .unwrap()
        .token;
    let h = headers_with(&[(
        "sec-websocket-protocol",
        &format!("ados-ws-ticket, {token}"),
    )]);
    assert!(matches!(
        decide_ws_auth(&state, &h, SCOPE_UPLINK_EVENTS, CallerClass::OperatorLan),
        WsAuth::AcceptTicket
    ));
}

#[test]
fn a_ticket_for_the_wrong_scope_is_rejected() {
    let (_d, state) = state_with_pairing(r#"{"paired": true, "api_key": "k"}"#);
    // Minted for pic_events but presented to the uplink scope.
    let token = WsTicketIssuer::from_api_key("k")
        .mint(SCOPE_PIC_EVENTS, 30)
        .unwrap()
        .token;
    let h = headers_with(&[(
        "sec-websocket-protocol",
        &format!("ados-ws-ticket, {token}"),
    )]);
    assert!(matches!(
        decide_ws_auth(&state, &h, SCOPE_UPLINK_EVENTS, CallerClass::OperatorLan),
        WsAuth::Reject
    ));
}

#[test]
fn a_ticket_for_the_wrong_key_is_rejected() {
    let (_d, state) = state_with_pairing(r#"{"paired": true, "api_key": "k"}"#);
    let token = WsTicketIssuer::from_api_key("other-key")
        .mint(SCOPE_UPLINK_EVENTS, 30)
        .unwrap()
        .token;
    let h = headers_with(&[(
        "sec-websocket-protocol",
        &format!("ados-ws-ticket, {token}"),
    )]);
    assert!(matches!(
        decide_ws_auth(&state, &h, SCOPE_UPLINK_EVENTS, CallerClass::OperatorLan),
        WsAuth::Reject
    ));
}

// -- mesh stream: auth scope + the two-journal fan -------------------

#[test]
fn mesh_stream_admits_with_a_valid_mesh_scoped_ticket() {
    let (_d, state) = state_with_pairing(r#"{"paired": true, "api_key": "k"}"#);
    let token = WsTicketIssuer::from_api_key("k")
        .mint(SCOPE_MESH_EVENTS, 30)
        .unwrap()
        .token;
    let h = headers_with(&[(
        "sec-websocket-protocol",
        &format!("ados-ws-ticket, {token}"),
    )]);
    assert!(matches!(
        decide_ws_auth(&state, &h, SCOPE_MESH_EVENTS, CallerClass::OperatorLan),
        WsAuth::AcceptTicket
    ));
}

#[test]
fn mesh_stream_rejects_a_wrong_scope_ticket() {
    let (_d, state) = state_with_pairing(r#"{"paired": true, "api_key": "k"}"#);
    // A uplink-scoped ticket presented to the mesh scope is rejected.
    let token = WsTicketIssuer::from_api_key("k")
        .mint(SCOPE_UPLINK_EVENTS, 30)
        .unwrap()
        .token;
    let h = headers_with(&[(
        "sec-websocket-protocol",
        &format!("ados-ws-ticket, {token}"),
    )]);
    assert!(matches!(
        decide_ws_auth(&state, &h, SCOPE_MESH_EVENTS, CallerClass::OperatorLan),
        WsAuth::Reject
    ));
}

// -- button stream: auth scope --------------------------------------------

#[test]
fn button_stream_admits_with_a_valid_button_scoped_ticket() {
    let (_d, state) = state_with_pairing(r#"{"paired": true, "api_key": "k"}"#);
    let token = WsTicketIssuer::from_api_key("k")
        .mint(SCOPE_BUTTON_EVENTS, 30)
        .unwrap()
        .token;
    let h = headers_with(&[(
        "sec-websocket-protocol",
        &format!("ados-ws-ticket, {token}"),
    )]);
    assert!(matches!(
        decide_ws_auth(&state, &h, SCOPE_BUTTON_EVENTS, CallerClass::OperatorLan),
        WsAuth::AcceptTicket
    ));
}

#[test]
fn button_stream_rejects_a_wrong_scope_ticket() {
    let (_d, state) = state_with_pairing(r#"{"paired": true, "api_key": "k"}"#);
    // A pic-scoped ticket presented to the button scope is rejected.
    let token = WsTicketIssuer::from_api_key("k")
        .mint(SCOPE_PIC_EVENTS, 30)
        .unwrap()
        .token;
    let h = headers_with(&[(
        "sec-websocket-protocol",
        &format!("ados-ws-ticket, {token}"),
    )]);
    assert!(matches!(
        decide_ws_auth(&state, &h, SCOPE_BUTTON_EVENTS, CallerClass::OperatorLan),
        WsAuth::Reject
    ));
}

#[tokio::test]
async fn journal_tail_skips_backlog_then_follows_new_lines() {
    use std::io::Write as _;
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("events.jsonl");
    // A pre-existing backlog line: the tail seeks to end on first open, so
    // this is never replayed.
    {
        let mut f = std::fs::File::create(&p).unwrap();
        writeln!(
            f,
            r#"{{"bus":"mesh","kind":"backlog","timestamp_ms":1,"payload":{{}}}}"#
        )
        .unwrap();
    }
    let mut tail = JournalTail::new(p.clone());
    // First poll opens + seeks to end: nothing to read.
    assert!(tail.next_line().await.is_none());
    // Append a fresh line; the tail reads it verbatim.
    {
        let mut f = std::fs::OpenOptions::new().append(true).open(&p).unwrap();
        writeln!(f, r#"{{"bus":"mesh","kind":"relay_connected","timestamp_ms":2,"payload":{{"relay_mac":"aa:bb"}}}}"#)
                .unwrap();
    }
    let line = tail.next_line().await.expect("a new line");
    let v: Value = serde_json::from_str(line.trim()).unwrap();
    assert_eq!(v["bus"], "mesh");
    assert_eq!(v["kind"], "relay_connected");
    assert_eq!(v["payload"]["relay_mac"], "aa:bb");
    // No further lines pending.
    assert!(tail.next_line().await.is_none());
}

#[tokio::test]
async fn a_journal_created_after_connect_is_read_from_its_start() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("late.jsonl");
    let mut tail = JournalTail::new(p.clone());
    // The file does not exist yet: a poll yields nothing without erroring.
    assert!(tail.next_line().await.is_none());
    // It appears later, so everything in it was written after the client
    // connected and streams.
    {
        use std::io::Write as _;
        let mut f = std::fs::File::create(&p).unwrap();
        writeln!(f, r#"{{"bus":"pair","kind":"accept_window_opened","timestamp_ms":3,"payload":{{"duration_s":60}}}}"#)
                .unwrap();
    }
    let line = tail
        .next_line()
        .await
        .expect("the first line of the new file");
    let v: Value = serde_json::from_str(line.trim()).unwrap();
    assert_eq!(v["kind"], "accept_window_opened");
    assert!(tail.next_line().await.is_none());
}

#[tokio::test]
async fn a_journal_replaced_by_rename_is_followed_from_its_start() {
    // A writer restart recreates the journal by rename; the new file can
    // already be longer than the old offset when the tail next looks.
    use std::io::Write as _;
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("mesh-events.jsonl");
    std::fs::write(&p, "{\"kind\":\"old\"}\n").unwrap();
    let mut tail = JournalTail::new(p.clone());
    // Opens and seeks past the pre-connect backlog.
    assert!(tail.next_line().await.is_none());

    let replacement = dir.path().join("mesh-events.jsonl.tmp");
    {
        let mut f = std::fs::File::create(&replacement).unwrap();
        writeln!(
            f,
            r#"{{"bus":"mesh","kind":"relay_connected","timestamp_ms":5,"payload":{{}}}}"#
        )
        .unwrap();
        writeln!(
            f,
            r#"{{"bus":"mesh","kind":"receiver_unreachable","timestamp_ms":6,"payload":{{}}}}"#
        )
        .unwrap();
    }
    std::fs::rename(&replacement, &p).unwrap();

    // The old handle is at EOF; the tail notices the new inode, then reads
    // the new file from its first line.
    let mut kinds = Vec::new();
    for _ in 0..4 {
        if let Some(line) = tail.next_line().await {
            let v: Value = serde_json::from_str(line.trim()).unwrap();
            kinds.push(v["kind"].as_str().unwrap().to_string());
        }
    }
    assert_eq!(kinds, ["relay_connected", "receiver_unreachable"]);
}

#[tokio::test]
async fn two_journals_fan_with_bus_envelopes_intact() {
    // The frame the handler forwards is the journal line itself: the mesh
    // journal stamps `bus:"mesh"`, the pairing journal stamps `bus:"pair"`,
    // and both carry the `{bus,kind,timestamp_ms,payload}` envelope the
    // residual `ws_mesh_events` sent. Tail both and assert each line
    // round-trips with its bus marker.
    let dir = tempfile::tempdir().unwrap();
    let mesh_p = dir.path().join("mesh-events.jsonl");
    let pair_p = dir.path().join("pair-events.jsonl");
    std::fs::write(&mesh_p, "").unwrap();
    std::fs::write(&pair_p, "").unwrap();
    let mut mesh_tail = JournalTail::new(mesh_p.clone());
    let mut pair_tail = JournalTail::new(pair_p.clone());
    // Prime both tails (open + seek to end of the empty files).
    assert!(mesh_tail.next_line().await.is_none());
    assert!(pair_tail.next_line().await.is_none());

    use std::io::Write as _;
    {
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&mesh_p)
            .unwrap();
        writeln!(f, r#"{{"bus":"mesh","kind":"role_changed","timestamp_ms":10,"payload":{{"role":"relay"}}}}"#)
                .unwrap();
    }
    {
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&pair_p)
            .unwrap();
        writeln!(f, r#"{{"bus":"pair","kind":"join_request_received","timestamp_ms":11,"payload":{{"device_id":"d2"}}}}"#)
                .unwrap();
    }

    let mesh_line = mesh_tail.next_line().await.expect("mesh line");
    let mv: Value = serde_json::from_str(mesh_line.trim()).unwrap();
    assert_eq!(mv["bus"], "mesh");
    assert_eq!(mv["kind"], "role_changed");
    assert_eq!(mv["timestamp_ms"], 10);
    assert_eq!(mv["payload"]["role"], "relay");

    let pair_line = pair_tail.next_line().await.expect("pair line");
    let pv: Value = serde_json::from_str(pair_line.trim()).unwrap();
    assert_eq!(pv["bus"], "pair");
    assert_eq!(pv["kind"], "join_request_received");
    assert_eq!(pv["timestamp_ms"], 11);
    assert_eq!(pv["payload"]["device_id"], "d2");
}
