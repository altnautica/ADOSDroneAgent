//! `GET /api/v2/observability/{*upstream_path}`: the logging store's query API,
//! reachable through `:8080`.
//!
//! The store serves its read API on a trusted local Unix socket and on its own
//! LAN port. A client that can only reach this front still needs the data, so
//! this forwards the path tail and query string verbatim to the store's socket:
//! `/api/v2/observability/v1/query?limit=10` reaches the store as
//! `/v1/query?limit=10`. The response streams through (the live tail is SSE, the
//! export a chunked stream). The front has already authenticated the request;
//! the store's socket plane is unauthenticated, so the agent key is not
//! forwarded. A store that is not serving is a `503` with the store's error
//! envelope, so a client cascades to its next tier.

use axum::extract::Request;
use axum::http::{StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;

const PREFIX: &str = "/api/v2/observability";

/// The header carrying the agent key, never forwarded to the store.
const AGENT_KEY_HEADER: &str = "x-ados-key";

fn store_unavailable() -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(json!({"error": {
            "code": "service_unavailable",
            "message": "logging store query socket unavailable",
        }})),
    )
        .into_response()
}

/// The store-side URI for a front-side one: the prefix stripped, the query kept.
fn upstream_uri(uri: &Uri) -> Option<Uri> {
    let tail = uri.path().strip_prefix(PREFIX)?;
    let tail = if tail.is_empty() { "/" } else { tail };
    match uri.query() {
        Some(q) => format!("{tail}?{q}").parse().ok(),
        None => tail.parse().ok(),
    }
}

/// `GET /api/v2/observability/{*upstream_path}` → the store's answer.
pub async fn observability_proxy(mut request: Request) -> Response {
    let Some(uri) = upstream_uri(request.uri()) else {
        return crate::routes::detail(StatusCode::NOT_FOUND, "Not Found");
    };
    *request.uri_mut() = uri;
    request.headers_mut().remove(AGENT_KEY_HEADER);
    crate::proxy::forward_plain(
        &crate::ipc::logd_client::default_logd_socket(),
        request,
        store_unavailable,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[test]
    fn the_prefix_is_stripped_and_the_query_kept() {
        let uri: Uri = "/api/v2/observability/v1/query?limit=10&level=warn"
            .parse()
            .unwrap();
        assert_eq!(
            upstream_uri(&uri).unwrap().to_string(),
            "/v1/query?limit=10&level=warn"
        );
        let bare: Uri = "/api/v2/observability/v1/sources".parse().unwrap();
        assert_eq!(upstream_uri(&bare).unwrap().to_string(), "/v1/sources");
    }

    #[tokio::test]
    async fn the_store_sees_the_tail_without_the_agent_key() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("logd-query.sock");
        let listener = tokio::net::UnixListener::bind(&sock).unwrap();
        let server = tokio::spawn(async move {
            let (mut conn, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 4096];
            let n = conn.read(&mut buf).await.unwrap();
            let head = String::from_utf8_lossy(&buf[..n]).to_string();
            conn.write_all(
                b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 2\r\n\r\n{}",
            )
            .await
            .unwrap();
            head
        });
        let mut request = Request::builder()
            .uri("/api/v2/observability/v1/query?limit=5")
            .header(AGENT_KEY_HEADER, "secret-key")
            .body(Body::empty())
            .unwrap();
        *request.uri_mut() = upstream_uri(request.uri()).unwrap();
        request.headers_mut().remove(AGENT_KEY_HEADER);
        let resp = crate::proxy::forward_plain(&sock, request, store_unavailable).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let head = server.await.unwrap();
        assert!(head.starts_with("GET /v1/query?limit=5 HTTP/1.1"), "{head}");
        assert!(!head.to_ascii_lowercase().contains("secret-key"));

        let absent = dir.path().join("absent.sock");
        let request = Request::builder().uri("/v1/q").body(Body::empty()).unwrap();
        let resp = crate::proxy::forward_plain(&absent, request, store_unavailable).await;
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    }
}
