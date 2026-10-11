use super::*;
use axum::body::Body;
use axum::routing::{delete, post};
use axum::Router;

/// A stand-in for mediamtx's WHEP server on an ephemeral loopback port.
async fn stub_upstream(router: Router) -> u16 {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    port
}

async fn body_text(resp: Response) -> String {
    let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    String::from_utf8_lossy(&bytes).into_owned()
}

fn offer(uri: &str) -> Request {
    Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/sdp")
        .body(Body::from("v=0\r\n"))
        .unwrap()
}

#[tokio::test]
async fn whep_rejects_bad_camera() {
    for query in [
        "camera=",
        "camera=..",
        "camera=a%2Fb",
        "camera=a/b",
        "camera",
    ] {
        let resp = post_whep(offer(&format!("/whep?{query}"))).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "{query}");
        assert!(body_text(resp).await.contains("invalid camera id"));
    }
    assert_eq!(camera_id(None).unwrap(), "main");
    assert_eq!(
        camera_id(Some("ados_session=t0k&camera=eo_wide-2")).unwrap(),
        "eo_wide-2"
    );
}

#[tokio::test]
async fn whep_rejects_an_encoded_session_id() {
    let req = Request::builder()
        .method("DELETE")
        .uri("/whep/x")
        .body(Body::empty())
        .unwrap();
    let resp = session_request(Method::DELETE, "a/../b", req).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn whep_strips_session_headers() {
    // The stub echoes every header name it received, one per line.
    let echo = Router::new().route(
        "/main/whep",
        post(|headers: HeaderMap| async move {
            headers
                .keys()
                .map(|k| k.as_str().to_owned())
                .collect::<Vec<_>>()
                .join("\n")
        }),
    );
    let port = stub_upstream(echo).await;
    let mut req = offer("/whep");
    let h = req.headers_mut();
    for name in [
        "x-ados-key",
        "authorization",
        "cookie",
        "x-ados-session",
        "x-ados-dashboard-session",
        "x-ados-onbox",
        "x-ados-mcp-scopes",
    ] {
        h.insert(
            http::HeaderName::from_static(name),
            HeaderValue::from_static("secret"),
        );
    }
    let resp = forward(port, Method::POST, "/main/whep", req, Some("main")).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let seen = body_text(resp).await;
    let seen: Vec<&str> = seen.lines().collect();
    for name in STRIPPED_REQUEST_HEADERS {
        if name == "host" || name == "content-length" {
            continue;
        }
        assert!(
            !seen.contains(&name),
            "{name} reached the upstream: {seen:?}"
        );
    }
    assert!(seen.contains(&"content-type"), "the SDP type is forwarded");
    assert!(seen.contains(&"host"), "the upstream host is set");
}

#[tokio::test]
async fn whep_location_passthrough() {
    // mediamtx answers the offer with its own resource path; the client must get
    // this front's path back so its DELETE and PATCH return through it.
    let upstream = Router::new()
        .route(
            "/ir/whep",
            post(|| async {
                (
                    StatusCode::CREATED,
                    [
                        ("content-type", "application/sdp"),
                        ("location", "/ir/whep/abc-123"),
                    ],
                    "v=0 answer",
                )
            }),
        )
        .route("/ir/whep/{session}", delete(|| async { StatusCode::OK }));
    let port = stub_upstream(upstream).await;

    let resp = forward(
        port,
        Method::POST,
        "/ir/whep",
        offer("/whep?camera=ir"),
        Some("ir"),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    assert_eq!(
        resp.headers().get(header::LOCATION).unwrap(),
        "/whep/abc-123?camera=ir"
    );
    assert_eq!(
        resp.headers().get(header::CONTENT_TYPE).unwrap(),
        "application/sdp"
    );
    assert_eq!(body_text(resp).await, "v=0 answer");

    let teardown = Request::builder()
        .method("DELETE")
        .uri("/whep/abc-123?camera=ir")
        .body(Body::empty())
        .unwrap();
    let resp = forward(port, Method::DELETE, "/ir/whep/abc-123", teardown, None).await;
    assert_eq!(resp.status(), StatusCode::OK);
}

#[test]
fn an_unexpected_location_is_left_alone() {
    let mut headers = HeaderMap::new();
    headers.insert(header::LOCATION, HeaderValue::from_static("/other/abc"));
    rewrite_location(&mut headers, "main");
    assert_eq!(headers.get(header::LOCATION).unwrap(), "/other/abc");
}

#[tokio::test]
async fn an_absent_mediamtx_is_a_503() {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let resp = forward(
        port,
        Method::POST,
        "/main/whep",
        offer("/whep"),
        Some("main"),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
}
