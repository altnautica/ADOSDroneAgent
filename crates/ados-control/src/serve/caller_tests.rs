//! The caller classification at the TCP edge, end to end: the real edge
//! middleware in front of the real pairing-claim, dashboard-PIN and WS-ticket
//! handlers, driven with the peer addresses and headers each kind of caller
//! arrives with.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::Request;
use axum::http::StatusCode;
use tower::util::ServiceExt;

use super::{now_unix_ms, tcp_edge, EdgeAuth, PeerAddr};
use crate::auth::{PairingState, RateLimiter};
use crate::dashboard_pin::DashboardPin;
use crate::ipc::{LogdQueryClient, MavlinkIpcClient, StateIpcClient};
use crate::mcp::{McpTokenStore, MintRequest, MCP_TOKEN_HEADER};
use crate::proxy_auth::ProxiedAuth;
use crate::routes;
use crate::state::{AppState, PairingPaths};

const KEY: &str = "ados_secret";

const LOOPBACK: [u8; 4] = [127, 0, 0, 1];
const LAN: [u8; 4] = [192, 168, 1, 50];
const HOTSPOT: [u8; 4] = [192, 168, 4, 20];
const WAN: [u8; 4] = [203, 0, 113, 5];

struct Harness {
    app: axum::Router,
    pin: Arc<DashboardPin>,
    mcp: Arc<McpTokenStore>,
    _dir: tempfile::TempDir,
}

/// A node (paired under [`KEY`] or unpaired) with the MCP accept flag on,
/// serving the claim, PIN-set and ticket routes plus a stand-in data route
/// behind the real edge.
fn harness(paired: bool) -> Harness {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    let pairing_json = d.join("pairing.json");
    let pairing_body = if paired {
        format!(r#"{{"paired": true, "api_key": "{KEY}"}}"#)
    } else {
        r#"{"paired": false}"#.to_string()
    };
    std::fs::write(&pairing_json, pairing_body).unwrap();
    let config = d.join("config.yaml");
    std::fs::write(
        &config,
        "mcp:\n  token_accept_enabled: true\nagent:\n  device_id: node-1\n",
    )
    .unwrap();

    let pairing = Arc::new(PairingState::with_path(pairing_json.clone()));
    let pin = Arc::new(DashboardPin::with_path(d.join("dashboard-pin.json")));
    let mcp = Arc::new(McpTokenStore::with_path(d.join("mcp-token.json")));
    let edge = EdgeAuth {
        pairing: pairing.clone(),
        rate: Arc::new(RateLimiter::default_control()),
        proxied: Arc::new(ProxiedAuth::new(crate::config::SecuritySection::default())),
        dashboard_pin: pin.clone(),
        mcp_tokens: mcp.clone(),
        config_path: config.clone(),
    };
    let state = AppState::new(
        pairing,
        StateIpcClient::disconnected(),
        MavlinkIpcClient::new(d.join("absent-mavlink.sock")),
        LogdQueryClient::new(d.join("absent-logd.sock")),
        d.join("board.json"),
        PairingPaths {
            config,
            pairing_json,
            wfb_key_dir: d.join("wfb"),
            bind_state: d.join("bind-state.json"),
            profile_conf: d.join("profile.conf"),
            mesh_role: d.join("mesh-role"),
            relay_secret: d.join("relay-peer-secret"),
        },
        pin.clone(),
        mcp.clone(),
    );
    let app = axum::Router::new()
        .route("/api/status", axum::routing::get(|| async { "ok" }))
        .route(
            "/api/pairing/claim",
            axum::routing::post(routes::pairing::claim_pairing),
        )
        .route(
            "/api/dashboard/pin/set",
            axum::routing::post(routes::dashboard_pin::set_pin),
        )
        .route(
            "/api/_ws/ticket",
            axum::routing::post(routes::ws_ticket::mint_ws_ticket),
        )
        .with_state(state)
        .layer(axum::middleware::from_fn_with_state(edge, tcp_edge));
    Harness {
        app,
        pin,
        mcp,
        _dir: dir,
    }
}

fn request(
    method: &str,
    uri: &str,
    peer: [u8; 4],
    headers: &[(&str, &str)],
    body: &str,
) -> Request<Body> {
    let mut builder = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .extension(PeerAddr(SocketAddr::from((peer, 45678))));
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    builder.body(Body::from(body.to_string())).unwrap()
}

async fn status_of(h: &Harness, req: Request<Body>) -> StatusCode {
    h.app.clone().oneshot(req).await.unwrap().status()
}

const SET_PIN: &str = r#"{"pin": "1234"}"#;
const CLAIM: &str = r#"{"user_id": "operator"}"#;

/// A tunnel terminating on this host delivers every internet request from
/// 127.0.0.1 with a forwarding header. On an unpaired node that caller used to
/// read as a first-boot lifeline: served every data route, allowed to claim the
/// node's key, and allowed to set the first dashboard PIN.
#[tokio::test]
async fn a_tunnelled_loopback_caller_gets_nothing_from_an_unpaired_node() {
    let h = harness(false);
    for header in ados_protocol::pairing_posture::FORWARDED_HEADERS {
        let tunnelled = [(header, "203.0.113.5")];
        assert_eq!(
            status_of(&h, request("GET", "/api/status", LOOPBACK, &tunnelled, "")).await,
            StatusCode::FORBIDDEN,
            "loopback + {header} must not reach an unpaired node's data"
        );
        assert_eq!(
            status_of(
                &h,
                request(
                    "POST",
                    "/api/dashboard/pin/set",
                    LOOPBACK,
                    &tunnelled,
                    SET_PIN
                )
            )
            .await,
            StatusCode::FORBIDDEN,
            "loopback + {header} must not set the first PIN"
        );
        assert_eq!(
            status_of(
                &h,
                request("POST", "/api/pairing/claim", LOOPBACK, &tunnelled, CLAIM)
            )
            .await,
            StatusCode::FORBIDDEN,
            "loopback + {header} must not claim the node"
        );
    }
    assert!(!h.pin.is_set());

    // The local operator, with no relay header, is still served.
    assert_eq!(
        status_of(&h, request("GET", "/api/status", LOOPBACK, &[], "")).await,
        StatusCode::OK
    );
}

/// A public-WAN peer cannot claim an unpaired node; a caller on its own LAN
/// still can, which is the documented local-first pairing flow.
#[tokio::test]
async fn an_unpaired_node_is_claimed_from_its_own_networks_only() {
    let h = harness(false);
    assert_eq!(
        status_of(&h, request("POST", "/api/pairing/claim", WAN, &[], CLAIM)).await,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        status_of(&h, request("POST", "/api/pairing/claim", LAN, &[], CLAIM)).await,
        StatusCode::OK
    );
}

/// On an unpaired node the first PIN is still claimed from the device's own
/// networks (trust-on-first-use).
#[tokio::test]
async fn an_unpaired_nodes_first_pin_is_set_from_its_own_lan() {
    let h = harness(false);
    assert_eq!(
        status_of(
            &h,
            request("POST", "/api/dashboard/pin/set", LAN, &[], SET_PIN)
        )
        .await,
        StatusCode::OK
    );
    assert!(h.pin.is_set());
}

/// A paired node's pairing key protected nothing while its first PIN was
/// unset: any host on the LAN could set one and receive a session the edge
/// accepts in place of the key. The first PIN on a paired node now needs the
/// key (or on-box access).
#[tokio::test]
async fn a_paired_nodes_first_pin_needs_the_key() {
    let h = harness(true);
    for peer in [LAN, HOTSPOT, WAN] {
        assert_eq!(
            status_of(
                &h,
                request("POST", "/api/dashboard/pin/set", peer, &[], SET_PIN)
            )
            .await,
            StatusCode::FORBIDDEN,
            "{peer:?} has no key and must not set the first PIN"
        );
    }
    assert!(!h.pin.is_set());

    // The key holder (Mission Control) sets it.
    assert_eq!(
        status_of(
            &h,
            request(
                "POST",
                "/api/dashboard/pin/set",
                LAN,
                &[("x-ados-key", KEY)],
                SET_PIN
            )
        )
        .await,
        StatusCode::OK
    );
    assert!(h.pin.is_set());
}

#[tokio::test]
async fn an_on_box_caller_sets_a_paired_nodes_first_pin() {
    let h = harness(true);
    assert_eq!(
        status_of(
            &h,
            request("POST", "/api/dashboard/pin/set", LOOPBACK, &[], SET_PIN)
        )
        .await,
        StatusCode::OK
    );
    assert!(h.pin.is_set());
}

/// Mint a scoped MCP token for the harness's paired node.
fn mint_token(h: &Harness, scopes: &[&str]) -> String {
    let scopes: Vec<String> = scopes.iter().map(|s| s.to_string()).collect();
    h.mcp
        .mint(&MintRequest {
            api_key: KEY,
            label: "test",
            operator_id: "op",
            node_id: "node-1",
            scopes: &scopes,
            allowed_nodes: &[],
            ttl_ms: 3_600_000,
            now_secs: 0.0,
            now_ms: now_unix_ms(),
        })
        .unwrap()
}

fn ticket_body(scope: &str) -> String {
    format!(r#"{{"scope": "{scope}"}}"#)
}

/// An operator who gives an AI client `read,admin` and deliberately withholds
/// `flight` must not see it buy a MAVLink WebSocket ticket, which would carry
/// arm and takeoff commands to the flight controller.
#[tokio::test]
async fn an_admin_only_mcp_token_cannot_mint_a_mavlink_ticket() {
    let h = harness(true);
    let admin = mint_token(&h, &["read", "admin"]);
    let with_admin = [(MCP_TOKEN_HEADER, admin.as_str())];
    assert_eq!(
        status_of(
            &h,
            request(
                "POST",
                "/api/_ws/ticket",
                LAN,
                &with_admin,
                &ticket_body("gs.mavlink_ws")
            )
        )
        .await,
        StatusCode::FORBIDDEN
    );
    // The same token still buys a read-only stream ticket.
    assert_eq!(
        status_of(
            &h,
            request(
                "POST",
                "/api/_ws/ticket",
                LAN,
                &with_admin,
                &ticket_body("vision.detections")
            )
        )
        .await,
        StatusCode::OK
    );

    // A token that holds `flight` gets the MAVLink ticket.
    let flight = mint_token(&h, &["admin", "flight"]);
    assert_eq!(
        status_of(
            &h,
            request(
                "POST",
                "/api/_ws/ticket",
                LAN,
                &[(MCP_TOKEN_HEADER, flight.as_str())],
                &ticket_body("gs.mavlink_ws")
            )
        )
        .await,
        StatusCode::OK
    );
}
