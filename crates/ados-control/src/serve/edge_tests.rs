//! The live security posture, the digest-signed uploads, the canonical request
//! target and the Unix-edge on-box stamp, driven through the real edge.

use super::*;

use hmac::{Hmac, KeyInit, Mac};
use sha2::{Digest, Sha256};
use tower::util::ServiceExt;

const KEY: &str = "ados_secret";
const SECRET: &str = "a-long-enough-secret-key";

/// A paired edge whose security posture is read live from `config` (re-checked
/// on every request) and shared with the proxied-route gate.
fn live_edge(dir: &Path, config: &str) -> EdgeAuth {
    let pairing_path = dir.join("pairing.json");
    std::fs::write(
        &pairing_path,
        format!(r#"{{"paired": true, "api_key": "{KEY}"}}"#),
    )
    .unwrap();
    let config_path = dir.join("config.yaml");
    std::fs::write(&config_path, config).unwrap();
    let security = Arc::new(SecurityState::with_ttl(config_path.clone(), Duration::ZERO));
    EdgeAuth {
        pairing: Arc::new(PairingState::with_path(pairing_path).with_security(security.clone())),
        rate: Arc::new(RateLimiter::default_control()),
        proxied: Arc::new(ProxiedAuth::live(security.clone())),
        dashboard_pin: Arc::new(crate::dashboard_pin::DashboardPin::with_path(
            dir.join("dashboard-pin.json"),
        )),
        mcp_tokens: Arc::new(McpTokenStore::with_path(dir.join("mcp-token.json"))),
        config_path,
        security,
    }
}

fn app(edge: EdgeAuth) -> Router {
    let ok = || async { "ok" };
    let routes = Router::new()
        .route("/api/status", axum::routing::get(ok))
        .route("/api/command", axum::routing::post(ok))
        .route("/api/plugins/install", axum::routing::post(ok));
    edge_router(routes, edge)
}

const LAN: ([u8; 4], u16) = ([192, 168, 1, 50], 45678);

fn lan_request(method: &str, uri: &str) -> axum::http::request::Builder {
    Request::builder()
        .method(method)
        .uri(uri)
        .header("X-ADOS-Key", KEY)
        .extension(PeerAddr(SocketAddr::from(LAN)))
}

async fn status_of(app: &Router, req: Request) -> StatusCode {
    app.clone().oneshot(req).await.unwrap().status()
}

/// Corrupt the config before it was ever read: off-box writes are refused
/// with the unreadable code, off-box reads and on-box writes still work, and
/// the Unix edge is untouched.
#[tokio::test]
async fn a_config_never_readable_refuses_off_box_writes_only() {
    let dir = tempfile::tempdir().unwrap();
    let edge = live_edge(dir.path(), "security: {hmac_enabled: [unclosed\n");
    let app = app(edge);

    let resp = app
        .clone()
        .oneshot(
            lan_request("POST", "/api/command")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    let body = axum::body::to_bytes(resp.into_body(), 4096).await.unwrap();
    let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(body["error"], "E_CONFIG_UNREADABLE");
    assert_eq!(
        body["message"],
        "config.yaml cannot be read; fix it on the node"
    );

    let read = lan_request("GET", "/api/status")
        .body(Body::empty())
        .unwrap();
    assert_eq!(status_of(&app, read).await, StatusCode::OK);

    let on_box = Request::builder()
        .method("POST")
        .uri("/api/command")
        .extension(PeerAddr(SocketAddr::from(([127, 0, 0, 1], 45678))))
        .extension(LocalAddr(SocketAddr::from(([127, 0, 0, 1], 8080))))
        .body(Body::empty())
        .unwrap();
    assert_eq!(status_of(&app, on_box).await, StatusCode::OK);

    let command = axum::routing::post(|| async { "ok" });
    let unix = unix_app(Router::new().route("/api/command", command));
    let req = Request::builder()
        .method("POST")
        .uri("/api/command")
        .extension(CallerClass::OnBox)
        .body(Body::empty())
        .unwrap();
    assert_eq!(unix.oneshot(req).await.unwrap().status(), StatusCode::OK);
}

/// Turning HMAC on in the config takes effect on the next request, with no
/// restart: an unsigned write that passed a moment ago is now refused.
#[tokio::test]
async fn enabling_hmac_in_the_config_applies_without_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let edge = live_edge(dir.path(), "security:\n  hmac_enabled: false\n");
    let config = edge.config_path.clone();
    let app = app(edge);
    let unsigned = || {
        lan_request("POST", "/api/command")
            .body(Body::empty())
            .unwrap()
    };
    assert_eq!(status_of(&app, unsigned()).await, StatusCode::OK);
    std::fs::write(
        &config,
        format!("security:\n  hmac_enabled: true\n  hmac_secret: {SECRET}\n"),
    )
    .unwrap();
    assert_eq!(status_of(&app, unsigned()).await, StatusCode::UNAUTHORIZED);
}

/// The digest-signed signature: `METHOD \n path \n nonce \n be_f64(ts) \n digest`.
fn sign_digest(path: &str, nonce: &str, ts: f64, digest: &str) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(SECRET.as_bytes()).unwrap();
    mac.update(format!("POST\n{path}\n{nonce}\n").as_bytes());
    mac.update(&ts.to_be_bytes());
    mac.update(format!("\n{digest}").as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

/// With HMAC on, a 2 MiB upload carrying a signed body digest reaches the
/// handler unbuffered; the same request signed over a different digest is a
/// 401. Without the digest header it is still the 413 a buffered body gets.
#[tokio::test]
async fn a_large_upload_signed_over_its_digest_reaches_the_handler() {
    let dir = tempfile::tempdir().unwrap();
    let edge = live_edge(
        dir.path(),
        &format!("security:\n  hmac_enabled: true\n  hmac_secret: {SECRET}\n"),
    );
    let app = app(edge);
    let body = vec![0x5au8; 2 * 1024 * 1024];
    let digest = hex::encode(Sha256::digest(&body));
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs_f64();
    let path = "/api/plugins/install";
    let upload = |nonce: &str, signed_digest: &str| {
        lan_request("POST", path)
            .header(CONTENT_DIGEST_HEADER, &digest)
            .header("x-timestamp", format!("{ts}"))
            .header("x-nonce", nonce)
            .header(
                "x-hmac-signature",
                sign_digest(path, nonce, ts, signed_digest),
            )
            .body(Body::from(body.clone()))
            .unwrap()
    };
    assert_eq!(
        status_of(&app, upload("n-up-1", &digest)).await,
        StatusCode::OK
    );
    let other = "00".repeat(32);
    assert_eq!(
        status_of(&app, upload("n-up-2", &other)).await,
        StatusCode::UNAUTHORIZED
    );
    let undeclared = lan_request("POST", path)
        .header("x-timestamp", format!("{ts}"))
        .header("x-nonce", "n-up-3")
        .header("x-hmac-signature", "00")
        .body(Body::from(body.clone()))
        .unwrap();
    assert_eq!(
        status_of(&app, undeclared).await,
        StatusCode::PAYLOAD_TOO_LARGE
    );
}

/// An accepted encoded spelling of a native path is served by the native
/// handler: the edge rewrites the target it authorized before routing.
#[tokio::test]
async fn an_encoded_native_path_reaches_the_native_handler() {
    let dir = tempfile::tempdir().unwrap();
    let app = app(live_edge(dir.path(), "agent:\n  name: test\n"));
    let req = lan_request("GET", "/%61pi/status")
        .body(Body::empty())
        .unwrap();
    assert_eq!(status_of(&app, req).await, StatusCode::OK);
}

/// The Unix edge replaces any client-supplied on-box header with `1`.
#[tokio::test]
async fn the_unix_edge_stamps_the_onbox_header() {
    let echo = |headers: axum::http::HeaderMap| async move {
        headers
            .get(ONBOX_HEADER)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string()
    };
    let unix = unix_app(Router::new().route("/api/vision/detections/ws", axum::routing::get(echo)));
    let req = Request::builder()
        .uri("/api/vision/detections/ws")
        .header(ONBOX_HEADER, "0")
        .body(Body::empty())
        .unwrap();
    let resp = unix.oneshot(req).await.unwrap();
    let body = axum::body::to_bytes(resp.into_body(), 64).await.unwrap();
    assert_eq!(&body[..], b"1");
}

/// Two tunnelled clients on loopback are charged to their own forwarded
/// addresses, so one exhausting its budget leaves the other served.
#[tokio::test]
async fn tunnelled_callers_do_not_share_a_rate_bucket() {
    let dir = tempfile::tempdir().unwrap();
    let mut edge = live_edge(dir.path(), "agent:\n  name: test\n");
    edge.rate = Arc::new(RateLimiter::new(2, Duration::from_secs(60)));
    let app = app(edge);
    let tunnelled = |client: &str| {
        Request::builder()
            .uri("/api/status")
            .header("X-ADOS-Key", KEY)
            .header("cf-connecting-ip", client)
            .extension(PeerAddr(SocketAddr::from(([127, 0, 0, 1], 45678))))
            .body(Body::empty())
            .unwrap()
    };
    for _ in 0..2 {
        assert_eq!(
            status_of(&app, tunnelled("203.0.113.7")).await,
            StatusCode::OK
        );
    }
    assert_eq!(
        status_of(&app, tunnelled("203.0.113.7")).await,
        StatusCode::TOO_MANY_REQUESTS
    );
    assert_eq!(
        status_of(&app, tunnelled("203.0.113.8")).await,
        StatusCode::OK
    );
}

/// Only a tile read that presents a valid key skips the request budget: a
/// keyless tile flood is charged and refused like any other request.
#[tokio::test]
async fn only_keyed_tile_reads_skip_the_request_budget() {
    let dir = tempfile::tempdir().unwrap();
    let mut edge = live_edge(dir.path(), "agent:\n  name: test\n");
    edge.rate = Arc::new(RateLimiter::new(2, Duration::from_secs(60)));
    let app = app(edge);
    const TILE: &str = "/api/v1/ground-station/tiles/p/1/2/3.mvt";
    let keyless = || {
        Request::builder()
            .method("GET")
            .uri(TILE)
            .extension(PeerAddr(SocketAddr::from(LAN)))
            .body(Body::empty())
            .unwrap()
    };
    status_of(&app, keyless()).await;
    status_of(&app, keyless()).await;
    assert_eq!(
        status_of(&app, keyless()).await,
        StatusCode::TOO_MANY_REQUESTS
    );

    let keyed = || {
        Request::builder()
            .method("GET")
            .uri(TILE)
            .header("X-ADOS-Key", KEY)
            .extension(PeerAddr(SocketAddr::from(([192, 168, 1, 51], 45678))))
            .body(Body::empty())
            .unwrap()
    };
    for _ in 0..3 {
        assert_ne!(
            status_of(&app, keyed()).await,
            StatusCode::TOO_MANY_REQUESTS
        );
    }
}
