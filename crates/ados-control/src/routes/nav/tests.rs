use super::*;

use ados_protocol::mcp_token::ScopeClass;
use ados_protocol::ws_ticket::WsTicketIssuer;
use axum::http::{HeaderValue, Method};
use serde_json::json;

use super::hub::{ClientView, KIND_COUNT};
use super::hub::{
    TOPIC_FOLLOW_LOST, TOPIC_FOLLOW_REACQUIRED, TOPIC_FOLLOW_STATE, TOPIC_MAP_STATUS,
    TOPIC_NAV_STATE, TOPIC_OCCUPANCY, TOPIC_POSE, TOPIC_SCAN,
};
use super::tiles::MAX_TILES;
use crate::mcp::route_scope;
use crate::routes::gs_ws::WsAuth;

fn live_tiles(seq: u64, tiles: &[(i64, i64, &str)]) -> Value {
    json!({
        "kind": "live",
        "seq": seq,
        "frame": "map_ned",
        "res_m": 0.05,
        "tile_cells": 64,
        "tiles": tiles
            .iter()
            .map(|(ix, iy, png)| json!({"ix": ix, "iy": iy, "png_b64": png}))
            .collect::<Vec<_>>(),
    })
}

// -- routing ---------------------------------------------------------------

#[test]
fn each_topic_is_pinned_to_its_owning_plugin() {
    assert_eq!(
        route(TOPIC_POSE, WORLD_ENGINE_ID),
        Routed::Latest(NavKind::Pose)
    );
    assert_eq!(
        route(TOPIC_SCAN, WORLD_ENGINE_ID),
        Routed::Latest(NavKind::Scan)
    );
    assert_eq!(
        route(TOPIC_MAP_STATUS, WORLD_ENGINE_ID),
        Routed::Latest(NavKind::Map)
    );
    assert_eq!(route(TOPIC_OCCUPANCY, WORLD_ENGINE_ID), Routed::Tiles);
    assert_eq!(
        route(TOPIC_NAV_STATE, VISION_NAV_ID),
        Routed::Latest(NavKind::Nav)
    );
    assert_eq!(
        route(TOPIC_FOLLOW_STATE, FOLLOW_ME_ID),
        Routed::Latest(NavKind::Follow)
    );
    assert_eq!(
        route(TOPIC_FOLLOW_REACQUIRED, FOLLOW_ME_ID),
        Routed::FollowEvent
    );
    assert_eq!(route(TOPIC_FOLLOW_LOST, FOLLOW_ME_ID), Routed::FollowEvent);
    // A look-alike from another plugin, or an unknown topic, goes nowhere.
    assert_eq!(
        route(TOPIC_FOLLOW_STATE, "com.example.other"),
        Routed::Ignored
    );
    assert_eq!(route(TOPIC_NAV_STATE, FOLLOW_ME_ID), Routed::Ignored);
    assert_eq!(route(TOPIC_POSE, VISION_NAV_ID), Routed::Ignored);
    assert_eq!(
        route("plugin.world-engine.splat", WORLD_ENGINE_ID),
        Routed::Ignored
    );
}

#[test]
fn every_routed_topic_is_subscribed() {
    for topic in EXPORT_TOPICS {
        let routed = [WORLD_ENGINE_ID, VISION_NAV_ID, FOLLOW_ME_ID]
            .iter()
            .any(|p| route(topic, p) != Routed::Ignored);
        assert!(routed, "{topic} is subscribed but never routed");
    }
}

// -- status ----------------------------------------------------------------

#[test]
fn status_reports_every_kind_null_until_it_arrives() {
    let hub = NavHub::new();
    let body = hub.status_body();
    for kind in NavKind::ALL {
        assert_eq!(body[kind.as_str()], Value::Null, "{}", kind.as_str());
    }
    hub.ingest(TOPIC_NAV_STATE, VISION_NAV_ID, 42, json!({"active_set": 1}));
    let body = hub.status_body();
    assert_eq!(body["nav"]["ts_ms"], 42);
    assert_eq!(body["nav"]["data"]["active_set"], 1);
    assert!(body["nav"]["age_ms"].as_u64().is_some());
    assert_eq!(body["pose"], Value::Null);
}

#[tokio::test]
async fn the_status_route_serves_the_hub() {
    let dir = tempfile::tempdir().unwrap();
    let state = test_state(dir.path());
    state.nav.ingest(
        TOPIC_FOLLOW_STATE,
        FOLLOW_ME_ID,
        7,
        json!({"phase": "coasting"}),
    );
    let Json(body) = get_nav_status(State(state), Query(NavStatusQuery::default())).await;
    assert_eq!(body["follow"]["data"]["phase"], "coasting");
    assert_eq!(body["scan"], Value::Null);
}

// -- stream coalescing -----------------------------------------------------

#[test]
fn frames_since_sends_only_the_latest_of_each_changed_kind() {
    let hub = NavHub::new();
    let mut sent = [0u64; KIND_COUNT];
    assert!(hub.frames_since(&mut sent, ClientView::All).is_empty());

    for n in 0..5 {
        hub.ingest(TOPIC_POSE, WORLD_ENGINE_ID, n, json!({"n": n}));
    }
    hub.ingest(TOPIC_SCAN, WORLD_ENGINE_ID, 9, json!({"ranges_m": []}));
    let frames = hub.frames_since(&mut sent, ClientView::All);
    assert_eq!(frames.len(), 2, "one frame per changed kind: {frames:?}");
    assert_eq!(frames[0]["kind"], "pose");
    assert_eq!(frames[0]["data"]["n"], 4);
    assert_eq!(frames[0]["ts_ms"], 4);
    assert_eq!(frames[1]["kind"], "scan");

    // Nothing changed: nothing to send.
    assert!(hub.frames_since(&mut sent, ClientView::All).is_empty());
    hub.ingest(
        TOPIC_MAP_STATUS,
        WORLD_ENGINE_ID,
        10,
        json!({"voxel_m": 0.1}),
    );
    let frames = hub.frames_since(&mut sent, ClientView::All);
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0]["kind"], "map");
}

#[tokio::test]
async fn a_latest_value_change_wakes_the_watch() {
    let hub = NavHub::new();
    let mut changes = hub.watch();
    hub.ingest(TOPIC_POSE, WORLD_ENGINE_ID, 1, json!({}));
    tokio::time::timeout(std::time::Duration::from_secs(1), changes.changed())
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn follow_events_fan_out_as_discrete_frames() {
    let hub = NavHub::new();
    let mut events = hub.follow_events();
    hub.ingest(
        TOPIC_FOLLOW_REACQUIRED,
        FOLLOW_ME_ID,
        11,
        json!({"track_id": 12, "similarity": 0.86, "gap_s": 1.4}),
    );
    let frame = events.recv().await.unwrap();
    assert_eq!(frame["kind"], "follow_event");
    assert_eq!(frame["topic"], TOPIC_FOLLOW_REACQUIRED);
    assert_eq!(frame["data"]["track_id"], 12);
    // A follow event is not a latest-value kind.
    assert_eq!(hub.status_body()["follow"], Value::Null);
}

// -- live map --------------------------------------------------------------

#[test]
fn map_live_is_empty_before_any_tile() {
    let body = NavHub::new().map_live_body(0);
    assert_eq!(body["seq"], 0);
    assert_eq!(body["frame"], "map_ned");
    assert_eq!(body["tiles"], json!([]));
}

#[test]
fn map_live_returns_the_latest_tile_changed_after_since() {
    let hub = NavHub::new();
    hub.ingest(
        TOPIC_OCCUPANCY,
        WORLD_ENGINE_ID,
        1,
        live_tiles(1, &[(0, 0, "a"), (1, 0, "b")]),
    );
    hub.ingest(
        TOPIC_OCCUPANCY,
        WORLD_ENGINE_ID,
        2,
        live_tiles(2, &[(0, 0, "a2")]),
    );

    let all = hub.map_live_body(0);
    assert_eq!(all["seq"], 2);
    assert_eq!(all["res_m"], 0.05);
    assert_eq!(all["tile_cells"], 64);
    assert_eq!(
        all["tiles"],
        json!([{"ix": 0, "iy": 0, "png_b64": "a2"}, {"ix": 1, "iy": 0, "png_b64": "b"}])
    );
    let since_one = hub.map_live_body(1);
    assert_eq!(
        since_one["tiles"],
        json!([{"ix": 0, "iy": 0, "png_b64": "a2"}])
    );
    assert_eq!(hub.map_live_body(2)["tiles"], json!([]));
}

#[test]
fn a_product_payload_does_not_touch_the_live_tiles() {
    let hub = NavHub::new();
    hub.ingest(
        TOPIC_OCCUPANCY,
        WORLD_ENGINE_ID,
        1,
        json!({"kind": "product", "seq": 5}),
    );
    assert_eq!(hub.map_live_body(0)["seq"], 0);
}

#[test]
fn a_restarted_producer_starts_a_new_session() {
    let hub = NavHub::new();
    hub.ingest(
        TOPIC_OCCUPANCY,
        WORLD_ENGINE_ID,
        1,
        live_tiles(40, &[(5, 5, "old")]),
    );
    hub.ingest(
        TOPIC_OCCUPANCY,
        WORLD_ENGINE_ID,
        2,
        live_tiles(1, &[(0, 0, "new")]),
    );
    let body = hub.map_live_body(0);
    assert_eq!(body["seq"], 1);
    assert_eq!(body["tiles"], json!([{"ix": 0, "iy": 0, "png_b64": "new"}]));
    // A client still holding the old session's seq gets every tile.
    assert_eq!(hub.map_live_body(40)["tiles"].as_array().unwrap().len(), 1);
}

#[test]
fn the_tile_store_drops_the_tiles_changed_longest_ago_past_its_cap() {
    let hub = NavHub::new();
    let first: Vec<(i64, i64, &str)> = (0..MAX_TILES as i64).map(|i| (i, 0, "p")).collect();
    hub.ingest(TOPIC_OCCUPANCY, WORLD_ENGINE_ID, 1, live_tiles(1, &first));
    hub.ingest(
        TOPIC_OCCUPANCY,
        WORLD_ENGINE_ID,
        2,
        live_tiles(2, &[(-1, -1, "q")]),
    );
    let tiles = hub.map_live_body(0)["tiles"].as_array().unwrap().clone();
    assert_eq!(tiles.len(), MAX_TILES);
    assert!(tiles.iter().any(|t| t["ix"] == -1));
}

#[tokio::test]
async fn the_map_live_route_reads_since_from_the_query() {
    let dir = tempfile::tempdir().unwrap();
    let state = test_state(dir.path());
    state.nav.ingest(
        TOPIC_OCCUPANCY,
        WORLD_ENGINE_ID,
        1,
        live_tiles(3, &[(2, -1, "t")]),
    );
    let Json(body) = get_map_live(State(state.clone()), Query(MapLiveQuery { since: 3 })).await;
    assert_eq!(body["tiles"], json!([]));
    let Json(body) = get_map_live(State(state), Query(MapLiveQuery { since: 0 })).await;
    assert_eq!(body["tiles"][0]["iy"], -1);
}

// -- auth and scope --------------------------------------------------------

#[test]
fn the_stream_admits_a_nav_ticket_and_refuses_another_scope() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("pairing.json"),
        r#"{"paired": true, "api_key": "k"}"#,
    )
    .unwrap();
    let state = test_state(dir.path());
    let offer = |scope: &str| {
        let token = WsTicketIssuer::from_api_key("k")
            .mint(scope, 30)
            .unwrap()
            .token;
        let mut h = HeaderMap::new();
        h.insert(
            "sec-websocket-protocol",
            HeaderValue::from_str(&format!("ados-ws-ticket, {token}")).unwrap(),
        );
        h
    };
    let decide =
        |h: &HeaderMap| decide_ws_auth(&state, h, SCOPE_NAV_STREAM, CallerClass::OperatorLan);
    assert!(matches!(
        decide(&offer(SCOPE_NAV_STREAM)),
        WsAuth::AcceptTicket
    ));
    assert!(matches!(
        decide(&offer("vision.detections")),
        WsAuth::Reject
    ));
    assert!(matches!(decide(&HeaderMap::new()), WsAuth::Reject));
}

/// A depth frame recorded from a live rendered run (`scale_source: lidar`).
fn recorded_depth_frame() -> Value {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/depth-frame-recorded-lidar.json"
    );
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

/// Read the HTTP head of a handshake response.
async fn read_head(conn: &mut tokio::net::TcpStream) -> String {
    use tokio::io::AsyncReadExt;
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        if conn.read(&mut byte).await.unwrap() == 0 {
            break;
        }
        head.push(byte[0]);
    }
    String::from_utf8(head).unwrap()
}

/// Read one unmasked server frame: `(opcode, payload)`.
async fn read_frame(conn: &mut tokio::net::TcpStream) -> (u8, Vec<u8>) {
    use tokio::io::AsyncReadExt;
    let mut hdr = [0u8; 2];
    conn.read_exact(&mut hdr).await.unwrap();
    let mut len = u64::from(hdr[1] & 0x7f);
    if len == 126 {
        let mut ext = [0u8; 2];
        conn.read_exact(&mut ext).await.unwrap();
        len = u64::from(u16::from_be_bytes(ext));
    }
    let mut payload = vec![0u8; usize::try_from(len).unwrap()];
    conn.read_exact(&mut payload).await.unwrap();
    (hdr[0] & 0x0f, payload)
}

/// A masked client text frame (zero mask key, so the payload goes as is).
fn client_text(payload: &str) -> Vec<u8> {
    let len = u8::try_from(payload.len()).expect("short frame");
    assert!(len < 126);
    let mut out = vec![0x81, 0x80 | len, 0, 0, 0, 0];
    out.extend_from_slice(payload.as_bytes());
    out
}

/// A browser dialling with a ticket can also carry a valid `X-ADOS-Key` (a
/// header-injecting proxy, a headless harness's extra headers). The `101` must
/// still select the `ados-ws-ticket` marker it offered, or the browser fails
/// the handshake and the layers never get a frame; then a view lease on the
/// served camera delivers the depth grid.
#[tokio::test]
async fn a_ticket_dial_carrying_the_key_header_is_answered_with_the_marker() {
    use tokio::io::AsyncWriteExt;
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("pairing.json"),
        r#"{"paired": true, "api_key": "k"}"#,
    )
    .unwrap();
    let state = test_state(dir.path());
    let frame = recorded_depth_frame();
    let ts = frame["ts_ms"].as_i64().unwrap();
    state.nav.ingest_host(NavKind::Depth, ts, frame.clone());

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = crate::routes::build_router(state.clone(), false);
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    let token = WsTicketIssuer::from_api_key("k")
        .mint(SCOPE_NAV_STREAM, 30)
        .unwrap()
        .token;
    let mut conn = tokio::net::TcpStream::connect(addr).await.unwrap();
    conn.write_all(
        format!(
            "GET /api/v1/nav/stream HTTP/1.1\r\nHost: node\r\nConnection: Upgrade\r\n\
             Upgrade: websocket\r\nSec-WebSocket-Version: 13\r\n\
             Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nX-ADOS-Key: k\r\n\
             Sec-WebSocket-Protocol: ados-ws-ticket, {token}\r\n\r\n"
        )
        .as_bytes(),
    )
    .await
    .unwrap();
    let response = read_head(&mut conn).await.to_ascii_lowercase();
    assert!(response.starts_with("http/1.1 101"), "{response}");
    assert!(
        response.contains("sec-websocket-protocol: ados-ws-ticket\r\n"),
        "the offered marker must be selected: {response}"
    );

    conn.write_all(&client_text(
        r#"{"type":"view","camera_id":"main","layers":["depth"]}"#,
    ))
    .await
    .unwrap();
    let got = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let (opcode, payload) = read_frame(&mut conn).await;
            if opcode != 0x1 {
                continue;
            }
            let v: Value = serde_json::from_slice(&payload).unwrap();
            if v["kind"] == "depth" {
                return v;
            }
        }
    })
    .await
    .expect("a depth frame after the view lease");
    assert_eq!(got["ts_ms"], json!(ts));
    assert_eq!(got["data"]["cameras"]["main"], frame["cameras"]["main"]);
}

/// A plain dial offering no subprotocol is answered with none.
#[tokio::test]
async fn a_header_dial_offering_no_subprotocol_gets_none() {
    use tokio::io::AsyncWriteExt;
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("pairing.json"),
        r#"{"paired": true, "api_key": "k"}"#,
    )
    .unwrap();
    let state = test_state(dir.path());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = crate::routes::build_router(state, false);
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    let mut conn = tokio::net::TcpStream::connect(addr).await.unwrap();
    conn.write_all(
        b"GET /api/v1/nav/stream HTTP/1.1\r\nHost: node\r\nConnection: Upgrade\r\n\
          Upgrade: websocket\r\nSec-WebSocket-Version: 13\r\n\
          Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nX-ADOS-Key: k\r\n\r\n",
    )
    .await
    .unwrap();
    let response = read_head(&mut conn).await.to_ascii_lowercase();
    assert!(response.starts_with("http/1.1 101"), "{response}");
    assert!(!response.contains("sec-websocket-protocol"), "{response}");
}

#[test]
fn a_read_token_reaches_every_nav_route() {
    for path in [
        "/api/v1/nav/status",
        "/api/v1/map/live",
        "/api/v1/nav/stream",
    ] {
        assert_eq!(
            route_scope(&Method::GET, path),
            Some(ScopeClass::Read),
            "{path}"
        );
    }
}

fn test_state(dir: &std::path::Path) -> AppState {
    let paths = crate::state::PairingPaths {
        config: dir.join("config.yaml"),
        pairing_json: dir.join("pairing.json"),
        wfb_key_dir: dir.join("wfb"),
        bind_state: dir.join("bind-state.json"),
        profile_conf: dir.join("profile.conf"),
        mesh_role: dir.join("mesh-role"),
        relay_secret: dir.join("relay-peer-secret"),
    };
    AppState::new(
        std::sync::Arc::new(crate::auth::PairingState::with_path(
            dir.join("pairing.json"),
        )),
        crate::ipc::StateIpcClient::disconnected(),
        crate::ipc::MavlinkIpcClient::new(dir.join("absent-mavlink.sock")),
        crate::ipc::LogdQueryClient::new(dir.join("absent-logd.sock")),
        dir.join("board.json"),
        paths,
        std::sync::Arc::new(crate::dashboard_pin::DashboardPin::with_path(
            dir.join("dashboard-pin.json"),
        )),
        std::sync::Arc::new(crate::mcp::McpTokenStore::with_path(
            dir.join("mcp-token.json"),
        )),
    )
}

// -- host kinds and view leases ----------------------------------------------

fn two_camera_frame() -> Value {
    json!({"v": 1, "ts_ms": 5, "approx": false, "reason": null,
           "cameras": {"main": {"items": [1]}, "down": {"items": [2]}},
           "sectors_m": [1.0]})
}

#[test]
fn a_client_leasing_main_never_receives_down() {
    let hub = NavHub::new();
    hub.ingest_host(NavKind::Ar, 5, two_camera_frame());
    hub.ingest_host(NavKind::Obstacles, 5, two_camera_frame());
    hub.ingest_host(NavKind::Pilot, 5, json!({"owner": "pilot"}));
    let lease = ViewLease::from_query("main:ar_route").unwrap();
    let mut sent = [0u64; KIND_COUNT];
    let frames = hub.frames_since(&mut sent, ClientView::Lease(Some(&lease)));
    let ar = frames.iter().find(|f| f["kind"] == "ar").unwrap();
    assert!(ar["data"]["cameras"].get("main").is_some());
    assert!(ar["data"]["cameras"].get("down").is_none());
    // Obstacles always go (sectors for the radar card) but carry no camera
    // grid for a layer this client does not draw.
    let obs = frames.iter().find(|f| f["kind"] == "obstacles").unwrap();
    assert_eq!(obs["data"]["cameras"], json!({}));
    assert_eq!(obs["data"]["sectors_m"], json!([1.0]));
    assert!(frames.iter().any(|f| f["kind"] == "pilot"));
}

#[test]
fn a_client_without_a_lease_gets_no_overlay_frames() {
    let hub = NavHub::new();
    hub.ingest_host(NavKind::Ar, 5, two_camera_frame());
    hub.ingest_host(NavKind::Depth, 5, two_camera_frame());
    hub.ingest_host(
        NavKind::Surfaces,
        5,
        json!({"camera_id": "main", "zones": []}),
    );
    let mut sent = [0u64; KIND_COUNT];
    assert!(hub
        .frames_since(&mut sent, ClientView::Lease(None))
        .is_empty());
    let lease = ViewLease::from_query("main:surfaces").unwrap();
    let mut sent = [0u64; KIND_COUNT];
    let frames = hub.frames_since(&mut sent, ClientView::Lease(Some(&lease)));
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0]["kind"], "surfaces");
}

#[test]
fn status_view_query_leases_and_reports_the_demand() {
    let hub = NavHub::new();
    let mut demand = hub.view_demand();
    assert!(demand.borrow_and_update().cameras.is_empty());
    hub.ingest_host(NavKind::Ar, 5, two_camera_frame());
    let body = hub.status_body_for(Some("down:ar_motion,obstacles"));
    assert!(body["ar"]["data"]["cameras"].get("main").is_none());
    assert!(body["ar"]["data"]["cameras"].get("down").is_some());
    assert_eq!(
        body["view_demand"],
        json!({"down": ["ar_motion", "obstacles"]})
    );
    assert!(demand.has_changed().unwrap());
    assert!(demand.borrow().wants("down", "ar_motion"));
    // Without a view the status is unfiltered.
    assert!(hub.status_body()["ar"]["data"]["cameras"]
        .get("main")
        .is_some());
}
