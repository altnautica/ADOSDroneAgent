//! TCP-edge gate ordering: authentication runs before the rate limit, so an
//! unauthenticated flood is charged to per-peer buckets and can never spend the
//! read budget a keyed caller draws from.
//!
//! These drive the real `tcp_edge` middleware in-process over a stub route, with
//! the peer attached the way the TCP accept loop attaches it, and with windows
//! long enough that no refill can mask a leak between the two budgets.

use std::io::Write;
use std::net::{IpAddr, Ipv4Addr};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::StatusCode;
use axum::middleware;
use axum::routing::get;
use axum::Router;
use tower::Service;

use super::auth::{PairingState, PeerRateLimiter, RateLimiter, MAX_TRACKED_PEERS};
use super::{tcp_edge, EdgeAuth, TcpPeer};

const KEY: &str = "ados_edge_key";
/// The authorized budget for the whole test: small, and never refilled.
const AUTH_CAPACITY: u32 = 5;
/// Refused requests each peer may make before 429, never refilled.
const UNAUTH_CAPACITY: u32 = 3;
const FLOOD: u32 = 1_000;
const LONG_WINDOW: Duration = Duration::from_secs(3600);

/// A paired edge over a stub `/v1/query` route.
fn paired_edge(dir: &Path) -> Router {
    let path = dir.join("pairing.json");
    let mut f = std::fs::File::create(&path).unwrap();
    f.write_all(format!(r#"{{"paired": true, "api_key": "{KEY}"}}"#).as_bytes())
        .unwrap();
    let edge = EdgeAuth {
        pairing: Arc::new(PairingState::with_path(path)),
        rate: Arc::new(RateLimiter::new(AUTH_CAPACITY, LONG_WINDOW)),
        unauth_rate: Arc::new(PeerRateLimiter::new(
            UNAUTH_CAPACITY,
            LONG_WINDOW,
            MAX_TRACKED_PEERS,
        )),
    };
    Router::new()
        .route("/v1/query", get(|| async { "ok" }))
        .layer(middleware::from_fn_with_state(edge, tcp_edge))
}

fn peer(n: u32) -> IpAddr {
    IpAddr::V4(Ipv4Addr::from(0x0a00_0000 + n))
}

async fn send(app: &Router, from: IpAddr, key: Option<&str>) -> StatusCode {
    let mut builder = http::Request::builder().uri("/v1/query");
    if let Some(k) = key {
        builder = builder.header("X-ADOS-Key", k);
    }
    let mut req = builder.body(Body::empty()).unwrap();
    req.extensions_mut().insert(TcpPeer {
        peer: from,
        local: Some(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 10))),
    });
    let mut app = app.clone();
    app.call(req).await.unwrap().status()
}

#[tokio::test]
async fn keyed_request_200_after_unauth_flood() {
    let dir = tempfile::tempdir().unwrap();
    let app = paired_edge(dir.path());

    // Half the flood from one peer with a wrong key: its first few are 401,
    // then its own bucket is spent and it gets 429.
    let single = peer(1);
    for i in 0..FLOOD / 2 {
        let status = send(&app, single, Some("wrong")).await;
        let want = if i < UNAUTH_CAPACITY {
            StatusCode::UNAUTHORIZED
        } else {
            StatusCode::TOO_MANY_REQUESTS
        };
        assert_eq!(status, want, "single-peer flood request {i}");
    }
    // The other half from many peers with no key: each is a fresh peer (or an
    // evicted-and-readmitted one), so each is refused with 401.
    for n in 0..FLOOD / 2 {
        let status = send(&app, peer(1_000 + n), None).await;
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "many-peer flood request {n}"
        );
    }

    // The keyed caller — even from the flooding peer — still gets the whole
    // authorized budget: not one token went to the flood.
    for i in 0..AUTH_CAPACITY {
        let status = send(&app, single, Some(KEY)).await;
        assert_eq!(status, StatusCode::OK, "keyed request {i} after the flood");
    }
    // And that budget is still enforced for keyed callers.
    assert_eq!(
        send(&app, peer(2), Some(KEY)).await,
        StatusCode::TOO_MANY_REQUESTS,
        "the authorized budget still caps keyed callers"
    );
}

#[tokio::test]
async fn a_spent_authorized_budget_still_answers_bad_keys_with_401() {
    let dir = tempfile::tempdir().unwrap();
    let app = paired_edge(dir.path());
    for _ in 0..AUTH_CAPACITY {
        assert_eq!(send(&app, peer(1), Some(KEY)).await, StatusCode::OK);
    }
    assert_eq!(
        send(&app, peer(1), Some(KEY)).await,
        StatusCode::TOO_MANY_REQUESTS
    );
    // Authentication runs first: a refused request is a 401 from its own
    // bucket, whatever state the authorized budget is in.
    assert_eq!(
        send(&app, peer(3), Some("wrong")).await,
        StatusCode::UNAUTHORIZED
    );
}
