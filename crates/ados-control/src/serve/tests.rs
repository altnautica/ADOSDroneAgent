use super::*;

#[tokio::test]
async fn bind_tcp_returns_real_bound_listeners() {
    // Port 0 lets the kernel pick a free port; the v4 leg binds first and the
    // v6 leg follows on the SAME port. On a dual-stack host both bind (2); on a
    // host without IPv6 only the v4 leg returns (1). Either way every returned
    // listener is a real bound socket with a resolvable local address.
    let listeners = bind_tcp(0).await.expect("v4 leg must bind");
    assert!(
        listeners.len() == 1 || listeners.len() == 2,
        "expected 1 (IPv4-only host) or 2 (dual-stack) listeners, got {}",
        listeners.len()
    );
    for l in &listeners {
        // A real listener resolves its bound address.
        let addr = l.local_addr().expect("a bound listener resolves its addr");
        assert_ne!(addr.port(), 0, "an ephemeral bind resolves to a real port");
    }
}

fn ws_headers(subprotocol: Option<&str>) -> http::HeaderMap {
    let mut h = http::HeaderMap::new();
    h.insert(http::header::CONNECTION, "upgrade".parse().unwrap());
    h.insert(http::header::UPGRADE, "websocket".parse().unwrap());
    if let Some(sp) = subprotocol {
        h.insert("sec-websocket-protocol", sp.parse().unwrap());
    }
    h
}

#[test]
fn ws_ticket_admits_authentic_upgrade_and_rejects_the_rest() {
    let key = "ados_secret";
    let paired = Pairing::Paired(key.to_string());
    let ticket = WsTicketIssuer::from_api_key(key)
        .mint("vision.detections", 30)
        .unwrap()
        .token;

    // A WS upgrade carrying an authentic ticket subprotocol is admitted.
    let good = ws_headers(Some(&format!("ados-ws-ticket, {ticket}")));
    let ws = "/api/vision/detections/ws";
    assert!(ws_upgrade_ticket_admits(&good, &paired, ws));
    // The same authentic ticket on another proxied stream's path → refused at
    // the edge (its scope is bound to the detections stream).
    assert!(!ws_upgrade_ticket_admits(
        &good,
        &paired,
        "/api/v1/setup/cloudflare/logs"
    ));
    // A path with no bound scope admits no ticket.
    assert!(!ws_upgrade_ticket_admits(&good, &paired, "/api/status"));

    // No ticket in the subprotocol list → not admitted.
    assert!(!ws_upgrade_ticket_admits(&ws_headers(None), &paired, ws));

    // A valid ticket present but the request is NOT a WS upgrade → not admitted.
    let mut plain = http::HeaderMap::new();
    plain.insert(
        "sec-websocket-protocol",
        format!("ados-ws-ticket, {ticket}").parse().unwrap(),
    );
    assert!(!ws_upgrade_ticket_admits(&plain, &paired, ws));

    // Unpaired agent → the edge handles openness elsewhere; the ticket helper
    // never admits on its own.
    assert!(!ws_upgrade_ticket_admits(&good, &Pairing::Unpaired, ws));

    // A ticket signed by a DIFFERENT pairing key → rejected.
    let forged = WsTicketIssuer::from_api_key("other-key")
        .mint("vision.detections", 30)
        .unwrap()
        .token;
    let bad = ws_headers(Some(&format!("ados-ws-ticket, {forged}")));
    assert!(!ws_upgrade_ticket_admits(&bad, &paired, ws));
}

/// A native plugin stream admits only a ticket minted for the scope its own
/// path names: the passthrough for that plugin, the progress stream for
/// that job. Another plugin's, another job's or an unrelated scope's
/// authentic ticket is refused.
#[test]
fn a_native_plugin_stream_admits_only_its_own_scoped_ticket() {
    let key = "ados_secret";
    let paired = Pairing::Paired(key.to_string());
    let issuer = WsTicketIssuer::from_api_key(key);
    let offer = |scope: &str| {
        ws_headers(Some(&format!(
            "ados-ws-ticket, {}",
            issuer.mint(scope, 30).unwrap().token
        )))
    };
    let web = "/api/plugins/com.example.web/x/live";
    assert!(native_ws_ticket_admits(
        &offer("plugins.http:com.example.web"),
        &paired,
        web
    ));
    for scope in [
        "plugins.http:com.example.other",
        "plugins.install_job:job-1",
        "vision.detections",
    ] {
        assert!(
            !native_ws_ticket_admits(&offer(scope), &paired, web),
            "{scope}"
        );
    }
    let job = "/api/plugins/jobs/job-1";
    assert!(native_ws_ticket_admits(
        &offer("plugins.install_job:job-1"),
        &paired,
        job
    ));
    assert!(!native_ws_ticket_admits(
        &offer("plugins.install_job:job-2"),
        &paired,
        job
    ));
    // A path with no bound scope admits no ticket at all.
    assert!(!native_ws_ticket_admits(
        &offer("plugins.http:com.example.web"),
        &paired,
        "/api/plugins/com.example.web"
    ));
}

/// Build an `EdgeAuth` over temp paths: a paired agent (`api_key=ados_secret`,
/// device `node-1`) with the MCP accept flag `enabled`, plus a store carrying
/// one minted `read`-scope token. Returns the edge and the token string.
fn mcp_edge(dir: &Path, accept_enabled: bool) -> (EdgeAuth, String) {
    let pairing_path = dir.join("pairing.json");
    std::fs::write(
        &pairing_path,
        r#"{"paired": true, "api_key": "ados_secret"}"#,
    )
    .unwrap();
    let config_path = dir.join("config.yaml");
    std::fs::write(
        &config_path,
        format!("mcp:\n  token_accept_enabled: {accept_enabled}\nagent:\n  device_id: node-1\n"),
    )
    .unwrap();
    let mcp_tokens = Arc::new(McpTokenStore::with_path(dir.join("mcp-token.json")));
    let scopes = ["read".to_string()];
    let token = mcp_tokens
        .mint(&crate::mcp::MintRequest {
            api_key: "ados_secret",
            label: "test",
            operator_id: "op",
            node_id: "node-1",
            scopes: &scopes,
            allowed_nodes: &[],
            ttl_ms: 3_600_000,
            now_secs: 0.0,
            now_ms: now_unix_ms(),
        })
        .unwrap();
    let edge = EdgeAuth {
        pairing: Arc::new(PairingState::with_path(pairing_path)),
        rate: Arc::new(RateLimiter::default_control()),
        proxied: Arc::new(ProxiedAuth::new(crate::config::SecuritySection::default())),
        dashboard_pin: Arc::new(crate::dashboard_pin::DashboardPin::with_path(
            dir.join("dashboard-pin.json"),
        )),
        mcp_tokens,
        security: Arc::new(SecurityState::with_ttl(
            config_path.clone(),
            std::time::Duration::ZERO,
        )),
        config_path,
    };
    (edge, token)
}

fn token_headers(token: &str) -> http::HeaderMap {
    let mut h = http::HeaderMap::new();
    h.insert(MCP_TOKEN_HEADER, token.parse().unwrap());
    h
}

#[test]
fn mcp_admits_a_read_token_on_a_read_route() {
    let dir = tempfile::tempdir().unwrap();
    let (edge, token) = mcp_edge(dir.path(), true);
    let h = token_headers(&token);
    // A read-scoped token reaches a read (GET) route.
    match edge.mcp_admits(&http::Method::GET, "/api/status", &h) {
        McpDecision::Admit(scopes) => assert_eq!(scopes, "read"),
        other => panic!("expected Admit, got {:?}", DecisionDbg(&other)),
    }
}

#[test]
fn mcp_denies_a_read_token_on_a_flight_route() {
    let dir = tempfile::tempdir().unwrap();
    let (edge, token) = mcp_edge(dir.path(), true);
    let h = token_headers(&token);
    // The command route needs the flight class; a read token is scope-denied.
    assert!(matches!(
        edge.mcp_admits(&http::Method::POST, "/api/command", &h),
        McpDecision::ScopeDenied
    ));
}

#[test]
fn mcp_falls_through_when_flag_off_or_no_token() {
    let dir = tempfile::tempdir().unwrap();
    // Flag OFF: even a valid token falls through to the normal 401.
    let (edge_off, token) = mcp_edge(dir.path(), false);
    assert!(matches!(
        edge_off.mcp_admits(&http::Method::GET, "/api/status", &token_headers(&token)),
        McpDecision::Fallthrough
    ));
    // Flag ON but no token header → fall through.
    let dir2 = tempfile::tempdir().unwrap();
    let (edge_on, _t) = mcp_edge(dir2.path(), true);
    assert!(matches!(
        edge_on.mcp_admits(&http::Method::GET, "/api/status", &http::HeaderMap::new()),
        McpDecision::Fallthrough
    ));
}

#[test]
fn mcp_falls_through_on_a_tampered_or_wrong_key_token() {
    let dir = tempfile::tempdir().unwrap();
    let (edge, token) = mcp_edge(dir.path(), true);
    // Flip a byte in the blob → HMAC fails → fall through (a normal 401).
    let (blob, sig) = token.rsplit_once('.').unwrap();
    let tampered = format!("{blob}x.{sig}");
    assert!(matches!(
        edge.mcp_admits(&http::Method::GET, "/api/status", &token_headers(&tampered)),
        McpDecision::Fallthrough
    ));
}

/// Build an UNPAIRED `EdgeAuth`: pairing.json with `paired:false`, a set
/// dashboard PIN (so a browser can mint a session), and a session minted the
/// way `/api/dashboard/pin/verify` does when the node has no pairing key.
fn unpaired_edge(dir: &std::path::Path) -> (EdgeAuth, crate::dashboard_pin::DashboardPin) {
    let pairing_path = dir.join("pairing.json");
    std::fs::write(&pairing_path, r#"{"paired": false, "api_key": ""}"#).unwrap();
    let dashboard_pin = Arc::new(crate::dashboard_pin::DashboardPin::with_path(
        dir.join("dashboard-pin.json"),
    ));
    let edge = EdgeAuth {
        pairing: Arc::new(PairingState::with_path(pairing_path)),
        rate: Arc::new(RateLimiter::default_control()),
        proxied: Arc::new(ProxiedAuth::new(crate::config::SecuritySection::default())),
        dashboard_pin: dashboard_pin.clone(),
        mcp_tokens: Arc::new(McpTokenStore::with_path(dir.join("mcp-token.json"))),
        config_path: dir.join("config.yaml"),
        security: Arc::new(SecurityState::new(dir.join("config.yaml"))),
    };
    (edge, dashboard_pin.as_ref().clone())
}

fn lan_app(edge: EdgeAuth) -> axum::Router {
    axum::Router::new()
        .route("/api/status", axum::routing::get(|| async { "ok" }))
        .layer(axum::middleware::from_fn_with_state(edge.clone(), tcp_edge))
        .with_state(edge)
}

/// The founder's scenario end-to-end: a private-LAN browser reaching a DATA
/// route on an UNPAIRED node must hold a dashboard PIN session. Without one it
/// is refused (403, exactly the pre-fix outcome); with a valid one it is
/// served. This is the new PIN gate, and it fails on the pre-fix behaviour
/// (which had no PIN path for a private-LAN peer at all).
#[tokio::test]
async fn unpaired_private_lan_data_route_requires_a_pin_session() {
    use tower::util::ServiceExt;
    let dir = tempfile::tempdir().unwrap();
    let (edge, dashboard_pin) = unpaired_edge(dir.path());
    dashboard_pin.set_pin("1234", 0.0).unwrap();
    let sess = dashboard_pin
        .mint_session("")
        .expect("a set PIN mints an unpaired session");
    let app = lan_app(edge);
    let peer = PeerAddr(SocketAddr::from(([192, 168, 1, 50], 45678)));

    // No session -> 403.
    let req = Request::builder()
        .uri("/api/status")
        .extension(peer)
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);

    // A wrong/tampered session is still refused.
    let req = Request::builder()
        .uri("/api/status")
        .header(crate::dashboard_pin::DASHBOARD_SESSION_HEADER, "v1|1|2|ff")
        .extension(peer)
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);

    // A valid session -> served.
    let req = Request::builder()
        .uri("/api/status")
        .header(crate::dashboard_pin::DASHBOARD_SESSION_HEADER, &sess.token)
        .extension(peer)
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

/// A first-boot lifeline peer keeps unrestricted unpaired data access (no PIN
/// required) — the provisioning surface is where the PIN is first created.
#[tokio::test]
async fn unpaired_lifeline_peer_served_data_without_a_pin() {
    use tower::util::ServiceExt;
    let dir = tempfile::tempdir().unwrap();
    let (edge, _set_pin_unused) = unpaired_edge(dir.path());
    let app = lan_app(edge);
    // Each peer reached the agent's own address on its subnet.
    for (ip, local) in [
        ("192.168.4.37", "192.168.4.1"),
        ("192.168.7.2", "192.168.7.1"),
        ("127.0.0.1", "127.0.0.1"),
    ] {
        let ip: std::net::IpAddr = ip.parse().unwrap();
        let local: std::net::IpAddr = local.parse().unwrap();
        let req = Request::builder()
            .uri("/api/status")
            .extension(PeerAddr(SocketAddr::from((ip, 45678))))
            .extension(LocalAddr(SocketAddr::from((local, 8080))))
            .body(Body::empty())
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "{ip} is a lifeline");
    }
    // A peer on a LAN merely numbered like the AP subnet, reaching the
    // node's own lease rather than the AP address, is not a lifeline.
    let req = Request::builder()
        .uri("/api/status")
        .extension(PeerAddr(SocketAddr::from(([192, 168, 4, 37], 45678))))
        .extension(LocalAddr(SocketAddr::from(([192, 168, 4, 12], 8080))))
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN, "not a lifeline");
    // A public-WAN peer stays refused on a data route.
    let req = Request::builder()
        .uri("/api/status")
        .extension(PeerAddr(SocketAddr::from(([8, 8, 8, 8], 45678))))
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

// --- the media-plane session, in the query string ------------------------
//
// A `<video>` element cannot attach a header to the requests it makes for a
// playlist or its segments, so a header-only credential is unreachable for
// element-driven playback and the operator gets a black frame with no way to
// authenticate it. These pin the narrow shape of the alternative.

#[test]
fn a_session_travels_in_the_query_string() {
    assert_eq!(
        session_token_from_query(Some("ados_session=abc123")),
        Some("abc123".to_string())
    );
    assert_eq!(
        session_token_from_query(Some("foo=1&ados_session=abc123&bar=2")),
        Some("abc123".to_string()),
        "the parameter need not be first"
    );
    // `encodeURIComponent` sends the token's `|` separators as `%7C`.
    assert_eq!(
        session_token_from_query(Some("ados_session=v1%7C1%7C2%7Cff")),
        Some("v1|1|2|ff".to_string())
    );
    assert_eq!(session_token_from_query(Some("ados_session=v1%7")), None);
}

#[test]
fn an_empty_session_parameter_is_absent_not_a_credential() {
    // `?ados_session=` must not read as "a session was presented". An empty
    // string that reached the validator would be one typo away from a
    // credential-shaped hole.
    assert_eq!(session_token_from_query(Some("ados_session=")), None);
    assert_eq!(session_token_from_query(Some("")), None);
    assert_eq!(session_token_from_query(None), None);
}

#[test]
fn a_similarly_named_parameter_is_not_the_session() {
    // Exact key match. A prefix or contains check here would accept
    // `not_ados_session` or `ados_session_id` as the real thing.
    assert_eq!(session_token_from_query(Some("not_ados_session=x")), None);
    assert_eq!(session_token_from_query(Some("ados_session_id=x")), None);
    assert_eq!(session_token_from_query(Some("session=x")), None);
}

#[test]
fn only_the_media_plane_may_carry_a_session_in_the_url() {
    // The whole justification for a URL-borne credential is that the video
    // element cannot send a header. Every `/api/*` caller is code that can,
    // so widening this beyond the media plane would put a credential in
    // access logs and browser history for no reason.
    for p in ["/whep", "/whep/main", "/hls", "/hls/main/index.m3u8"] {
        assert!(crate::proxy_auth::is_media_plane(p), "{p} is media");
    }
    for p in [
        "/api/status",
        "/api/config",
        "/api/command",
        "/cockpit/",
        "/whepinar",
    ] {
        assert!(!crate::proxy_auth::is_media_plane(p), "{p} is NOT media");
    }
}

// --- the path-normalization bypass table ---------------------------------
//
// Every gate on this edge used to call `request.uri().path()` for itself.
// That is the RAW target from the request line, so `%61pi` is six
// characters matching no literal in the relay denylist, the public list
// or `routing::is_native`. The request therefore missed every gate AND
// missed the native router, fell through to the reverse proxy, and the
// residual FastAPI decoded it the way every ASGI server does — serving
// `/api/v1/setup/reboot` to a caller holding no credential.
//
// This is the test that would have caught the whole class.

/// The edge harness for a PAIRED node, where every non-public route needs
/// `X-ADOS-Key`.
fn paired_edge(dir: &std::path::Path) -> EdgeAuth {
    let pairing_path = dir.join("pairing.json");
    std::fs::write(
        &pairing_path,
        r#"{"paired": true, "api_key": "ados_secret"}"#,
    )
    .unwrap();
    EdgeAuth {
        pairing: Arc::new(PairingState::with_path(pairing_path)),
        rate: Arc::new(RateLimiter::default_control()),
        proxied: Arc::new(ProxiedAuth::new(crate::config::SecuritySection::default())),
        dashboard_pin: Arc::new(crate::dashboard_pin::DashboardPin::with_path(
            dir.join("dashboard-pin.json"),
        )),
        mcp_tokens: Arc::new(McpTokenStore::with_path(dir.join("mcp-token.json"))),
        config_path: dir.join("config.yaml"),
        security: Arc::new(SecurityState::new(dir.join("config.yaml"))),
    }
}

/// Every spelling of a privileged path that is not its canonical form is
/// refused BEFORE any gate reads it, on a paired node, with no credential.
#[tokio::test]
async fn encoded_paths_never_reach_a_handler_without_a_credential() {
    use tower::util::ServiceExt;
    let dir = tempfile::tempdir().unwrap();
    let app = lan_app(paired_edge(dir.path()));

    // An off-box peer, so on-box loopback trust cannot mask the result.
    let peer = PeerAddr(SocketAddr::from(([192, 168, 1, 50], 45678)));

    for uri in [
        // The percent-encoded first letter: the original bypass.
        "/%61pi/v1/setup/reboot",
        // The media plane, which is deliberately NOT exempt.
        "/%77hep",
        // An encoded dot segment, which a downstream router may collapse.
        "/api/%2e%2e/v1/setup/reboot",
        // An empty segment, which different routers collapse differently.
        "/api//v1/setup/reboot",
        // A literal dot segment.
        "/api/../api/v1/setup/reboot",
        // Double encoding: `%2561pi` decodes once to `%61pi`.
        "/%2561pi/v1/setup/reboot",
    ] {
        let req = Request::builder()
            .uri(uri)
            .extension(peer)
            .body(Body::empty())
            .unwrap();
        let status = app.clone().oneshot(req).await.unwrap().status();
        assert!(
            status == StatusCode::BAD_REQUEST || status == StatusCode::UNAUTHORIZED,
            "{uri} must be refused before any handler, got {status}",
        );
    }
}

/// The canonical spelling of the same route still behaves normally: the
/// normalizer must not have turned the gate into a blanket refusal.
#[tokio::test]
async fn the_canonical_path_still_reaches_the_gate() {
    use tower::util::ServiceExt;
    let dir = tempfile::tempdir().unwrap();
    let app = lan_app(paired_edge(dir.path()));
    let peer = PeerAddr(SocketAddr::from(([192, 168, 1, 50], 45678)));

    // No key: the ordinary 401, not the 400 the normalizer emits.
    let req = Request::builder()
        .uri("/api/status")
        .extension(peer)
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        app.clone().oneshot(req).await.unwrap().status(),
        StatusCode::UNAUTHORIZED,
    );

    // With the key: served.
    let req = Request::builder()
        .uri("/api/status")
        .header("X-ADOS-Key", "ados_secret")
        .extension(peer)
        .body(Body::empty())
        .unwrap();
    assert_eq!(app.oneshot(req).await.unwrap().status(), StatusCode::OK);
}

/// The media plane through the WHOLE edge on a paired node. `/whep` and
/// `/hls` are proxied routes, so this exercises the proxied lane, where the
/// query-string session used to be ignored: a `<video>` element holding a
/// valid PIN session got 401 and a black frame.
#[tokio::test]
async fn the_proxied_media_plane_accepts_a_query_session_and_refuses_no_credential() {
    use tower::util::ServiceExt;
    let dir = tempfile::tempdir().unwrap();
    let edge = paired_edge(dir.path());
    edge.dashboard_pin.set_pin("1234", 0.0).unwrap();
    let sess = edge
        .dashboard_pin
        .mint_session("ados_secret")
        .expect("a set PIN mints a session");
    // What a browser's `encodeURIComponent` puts on the wire.
    let sess_q = sess.token.replace('|', "%7C");
    let app = axum::Router::new()
        .route(
            "/hls/main/index.m3u8",
            axum::routing::get(|| async { "ok" }),
        )
        .route("/whep", axum::routing::post(|| async { "ok" }))
        .route("/api/status", axum::routing::get(|| async { "ok" }))
        .layer(axum::middleware::from_fn_with_state(edge.clone(), tcp_edge))
        .with_state(edge);
    let peer = PeerAddr(SocketAddr::from(([192, 168, 1, 50], 45678)));
    let status = |method: &str, uri: String, key: Option<&str>| {
        let app = app.clone();
        let mut b = Request::builder().method(method).uri(uri).extension(peer);
        if let Some(k) = key {
            b = b.header("X-ADOS-Key", k);
        }
        async move {
            app.oneshot(b.body(Body::empty()).unwrap())
                .await
                .unwrap()
                .status()
        }
    };

    // No credential: the credentialed proxy refuses.
    assert_eq!(
        status("GET", "/hls/main/index.m3u8".into(), None).await,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        status("POST", "/whep".into(), None).await,
        StatusCode::UNAUTHORIZED
    );
    // A forged session in the query is refused.
    assert_eq!(
        status(
            "GET",
            "/hls/main/index.m3u8?ados_session=v1%7C1%7C2%7Cff".into(),
            None
        )
        .await,
        StatusCode::UNAUTHORIZED
    );
    // A valid session in the query is served, on both media routes.
    assert_eq!(
        status(
            "GET",
            format!("/hls/main/index.m3u8?ados_session={sess_q}"),
            None
        )
        .await,
        StatusCode::OK
    );
    assert_eq!(
        status("POST", format!("/whep?ados_session={sess_q}"), None).await,
        StatusCode::OK
    );
    // The pairing key still works.
    assert_eq!(
        status("GET", "/hls/main/index.m3u8".into(), Some("ados_secret")).await,
        StatusCode::OK
    );
    // Off the media plane the URL never carries a credential.
    assert_eq!(
        status("GET", format!("/api/status?ados_session={sess_q}"), None).await,
        StatusCode::UNAUTHORIZED
    );
}

/// The same table against the RADIO RELAY denylist. A relayed caller is
/// on-box by construction (the relay lands on loopback), so an encoded
/// path that slips the denylist is served with full node authority.
#[tokio::test]
async fn encoded_paths_cannot_slip_the_relay_denylist() {
    use tower::util::ServiceExt;
    let dir = tempfile::tempdir().unwrap();
    let app = lan_app(paired_edge(dir.path()));

    for uri in [
        "/api/pairing/%75npair",
        "/%61pi/pairing/unpair",
        "/api//pairing/unpair",
        "/api/pairing/%2e/unpair",
        // Prefix coverage: anything beneath a denied route is denied too.
        "/api/pairing/claim/resume",
    ] {
        let req = Request::builder()
            .uri(uri)
            .header(auth::RELAYED_HEADER, "1")
            .extension(PeerAddr(SocketAddr::from(([127, 0, 0, 1], 45678))))
            .body(Body::empty())
            .unwrap();
        let status = app.clone().oneshot(req).await.unwrap().status();
        assert!(
            status == StatusCode::BAD_REQUEST || status == StatusCode::FORBIDDEN,
            "{uri} must not be reachable over the relay, got {status}",
        );
    }
}

/// The relay carries the linked ground station's authority over the
/// operating surface but never a trust-root change, with HMAC enabled or
/// not: the ticket the drone verified is the request's authentication.
#[tokio::test]
async fn the_relay_reaches_operations_but_not_trust_roots() {
    use tower::util::ServiceExt;
    let dir = tempfile::tempdir().unwrap();
    let mut edge = paired_edge(dir.path());
    // HMAC on: a relayed write must still go through, unsigned.
    edge.proxied = Arc::new(ProxiedAuth::new(crate::config::SecuritySection {
        hmac_enabled: true,
        hmac_secret: "a-long-enough-secret-key".to_string(),
        ..Default::default()
    }));
    let ok = || async { "ok" };
    let app = axum::Router::new()
        .route("/api/pairing/claim", axum::routing::post(ok))
        .route("/api/v1/setup/apply", axum::routing::post(ok))
        .route("/api/plugins/{id}/enable", axum::routing::post(ok))
        .layer(axum::middleware::from_fn_with_state(edge.clone(), tcp_edge))
        .with_state(edge);
    let relayed = |uri: &str| {
        Request::builder()
            .method("POST")
            .uri(uri)
            .header(auth::RELAYED_HEADER, "1")
            .extension(PeerAddr(SocketAddr::from(([127, 0, 0, 1], 45678))))
            .body(Body::empty())
            .unwrap()
    };
    for uri in ["/api/pairing/claim", "/api/v1/setup/apply"] {
        let status = app.clone().oneshot(relayed(uri)).await.unwrap().status();
        assert_eq!(status, StatusCode::FORBIDDEN, "{uri}");
    }
    let status = app
        .clone()
        .oneshot(relayed("/api/plugins/com.example.tool/enable"))
        .await
        .unwrap()
        .status();
    assert_eq!(status, StatusCode::OK, "a ticketed plugin enable crosses");
}

/// The normalizer's own contract, unit-level, so a failure names the rule
/// rather than a status code.
#[test]
fn decision_path_decodes_once_and_refuses_ambiguity() {
    use crate::auth::{decision_path, PathRejection};

    assert_eq!(decision_path("/api/status").unwrap(), "/api/status");
    assert_eq!(decision_path("/%61pi/status").unwrap(), "/api/status");
    // A trailing slash is the one legitimate empty segment.
    assert_eq!(decision_path("/api/status/").unwrap(), "/api/status/");

    assert_eq!(
        decision_path("/%2561pi/status"),
        Err(PathRejection::DoubleEncoded),
    );
    assert_eq!(
        decision_path("/api//status"),
        Err(PathRejection::EmptySegment)
    );
    assert_eq!(
        decision_path("/api/../status"),
        Err(PathRejection::DotSegment)
    );
    assert_eq!(
        decision_path("/api/%2e%2e/status"),
        Err(PathRejection::DotSegment),
    );
    assert_eq!(decision_path("/api/%00"), Err(PathRejection::ControlByte));
    assert_eq!(
        decision_path("/api/%zz"),
        Err(PathRejection::MalformedEscape)
    );
    assert_eq!(
        decision_path("/api/%a"),
        Err(PathRejection::MalformedEscape)
    );
}

/// A tiny Debug shim so a failing Admit assertion can print the variant.
struct DecisionDbg<'a>(&'a McpDecision);
impl std::fmt::Debug for DecisionDbg<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.0 {
            McpDecision::Admit(s) => write!(f, "Admit({s})"),
            McpDecision::ScopeDenied => write!(f, "ScopeDenied"),
            McpDecision::Fallthrough => write!(f, "Fallthrough"),
        }
    }
}
