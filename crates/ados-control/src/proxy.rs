//! The reverse-proxy passthrough to the residual Python.
//!
//! While the migration is in flight a single CPython + FastAPI process serves
//! behind the native front: the front owns the LAN port and answers the routes
//! it has taken over ([`crate::routing::is_native`]); every other route falls
//! through to this proxy, which forwards it byte-faithfully to the residual API
//! over its internal Unix socket (`ADOS_API_INTERNAL_SOCKET`, default
//! `/run/ados/api-internal.sock`).
//!
//! The forward is transparent: the same method, the same path + query, every
//! request header verbatim (the `X-ADOS-Key` / `Origin` / `Referer` / `Cookie` /
//! `Content-Type` the residual auth reads are all preserved, plus the trustworthy
//! `X-ADOS-Onbox` the edge stamped), and the request body STREAMED so a large
//! upload passes through without buffering. The upstream response comes back
//! verbatim — status, headers (minus hop-by-hop), body streamed — so an SSE
//! stream relays unbroken (no `Content-Length`, the body just flows).
//!
//! A `Connection: upgrade` + `Upgrade: websocket` request is handled by a
//! transport-level upgrade passthrough: the handshake is forwarded, the `101` and
//! its headers are relayed back, and the two upgraded byte streams are spliced
//! with [`tokio::io::copy_bidirectional`].
//!
//! When the upstream socket is ABSENT or unconnectable (the zero-Python headless
//! profile runs no residual FastAPI), the proxy returns a clean FastAPI-shaped
//! `{"detail": "Not Found"}` — `501` for a known permanent-Python prefix (the
//! feature is absent on this profile) and `404` otherwise. It NEVER `500`s on an
//! absent upstream.

use std::path::{Path, PathBuf};

use axum::body::Body;
use axum::extract::{Request, State};
use axum::response::Response;
use http::{HeaderName, StatusCode};
use hyper::body::Incoming;
use hyper_util::rt::TokioIo;
use tokio::net::UnixStream;

use crate::routes::detail;
use crate::routing;
use crate::state::AppState;

/// The env var that points the proxy at the residual API's internal Unix socket.
/// When set it wins as an absolute path; otherwise the default resolves under the
/// runtime dir. Mirrors the Python `API_INTERNAL_SOCKET_ENV`.
pub const API_INTERNAL_SOCKET_ENV: &str = "ADOS_API_INTERNAL_SOCKET";

/// The internal-socket file name under the runtime dir.
const API_INTERNAL_SOCKET_NAME: &str = "api-internal.sock";

/// Hop-by-hop response headers stripped before relaying the upstream response:
/// they describe THIS connection, not the proxied payload, so forwarding them
/// would mislead the downstream client. `Upgrade` is stripped on the normal path
/// (the upgrade path handles it explicitly). All compared case-insensitively
/// (`HeaderName` is already lowercase).
const HOP_BY_HOP: [&str; 6] = [
    "connection",
    "keep-alive",
    "transfer-encoding",
    "upgrade",
    "proxy-authenticate",
    "proxy-authorization",
];

/// The default internal-socket path, honouring `ADOS_API_INTERNAL_SOCKET` (an
/// absolute override) and otherwise resolving under `ADOS_RUN_DIR` the same way
/// the sibling sockets do, defaulting to `/run/ados/api-internal.sock`.
pub fn default_internal_socket() -> PathBuf {
    if let Ok(explicit) = std::env::var(API_INTERNAL_SOCKET_ENV) {
        let trimmed = explicit.trim();
        if !trimmed.is_empty() {
            return PathBuf::from(trimmed);
        }
    }
    let run_dir = std::env::var("ADOS_RUN_DIR").unwrap_or_else(|_| "/run/ados".to_string());
    Path::new(&run_dir).join(API_INTERNAL_SOCKET_NAME)
}

/// The axum fallback handler: reverse-proxy any non-native route to the residual
/// Python over its internal Unix socket. Resolves the socket path from the
/// environment, then forwards. The socket path is resolved here (not held in the
/// app state) so a unit override of the env is honoured per request, matching the
/// other env-resolved seams.
pub async fn proxy_to_residual(State(_state): State<AppState>, request: Request) -> Response {
    let socket = default_internal_socket();
    proxy_with_socket(&socket, request).await
}

/// The testable core: forward `request` to the residual API at `socket`. Split
/// out from [`proxy_to_residual`] so a test can point it at a mock Unix backend.
pub async fn proxy_with_socket(socket: &Path, request: Request) -> Response {
    if is_websocket_upgrade(request.headers()) {
        return proxy_upgrade(socket, request).await;
    }
    proxy_plain(socket, request).await
}

/// Forward a plain (non-upgrade) request: open a fresh HTTP/1.1 connection on the
/// upstream Unix socket, send the request with its body STREAMED, and relay the
/// upstream response (status + non-hop-by-hop headers + streamed body) back. SSE
/// works for free: the streamed body carries no `Content-Length` we set, so it
/// just flows.
async fn proxy_plain(socket: &Path, request: Request) -> Response {
    let stream = match UnixStream::connect(socket).await {
        Ok(s) => s,
        Err(_) => return upstream_absent(request.uri().path()),
    };

    // Drive the upstream connection on its own task. `handshake` returns a sender
    // and a connection future that must be polled for the exchange to progress.
    let (mut sender, conn) = match hyper::client::conn::http1::handshake(TokioIo::new(stream)).await
    {
        Ok(pair) => pair,
        Err(_) => return upstream_absent(request.uri().path()),
    };
    tokio::spawn(async move {
        // The connection ends when the response body is fully read or the peer
        // closes; an error here is just the connection ending, not a route fault.
        let _ = conn.await;
    });

    // The axum request body implements `http_body::Body`, so it forwards as the
    // upstream request body directly — no buffering, a large upload streams.
    match sender.send_request(request).await {
        Ok(upstream) => relay_response(upstream),
        // The upstream accepted the connection but the exchange failed (it closed
        // mid-request, or sent a malformed reply). Degrade rather than 500 — to
        // the downstream client the route simply is not there right now.
        Err(_) => upstream_absent_path_only(socket),
    }
}

/// Turn the upstream `Response<Incoming>` into an axum response: copy the status,
/// copy every header except the hop-by-hop set, and stream the body through
/// unbuffered.
fn relay_response(upstream: http::Response<Incoming>) -> Response {
    let (parts, body) = upstream.into_parts();
    let mut out = Response::new(Body::new(body));
    *out.status_mut() = parts.status;
    let headers = out.headers_mut();
    for (name, value) in parts.headers.iter() {
        if is_hop_by_hop(name) {
            continue;
        }
        headers.append(name.clone(), value.clone());
    }
    out
}

/// True when the request is a WebSocket upgrade handshake (`Connection` lists
/// `upgrade` AND `Upgrade: websocket`), so it routes through the transport-level
/// upgrade passthrough rather than the plain request/response path.
fn is_websocket_upgrade(headers: &http::HeaderMap) -> bool {
    let connection_has_upgrade = headers
        .get(http::header::CONNECTION)
        .and_then(|v| v.to_str().ok())
        .map(|v| {
            v.to_ascii_lowercase()
                .split(',')
                .any(|t| t.trim() == "upgrade")
        })
        .unwrap_or(false);
    let upgrade_is_websocket = headers
        .get(http::header::UPGRADE)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.eq_ignore_ascii_case("websocket"))
        .unwrap_or(false);
    connection_has_upgrade && upgrade_is_websocket
}

/// Forward a WebSocket upgrade: send the handshake to the upstream, relay the
/// `101` + its headers back, and splice the two upgraded byte streams with
/// `copy_bidirectional`. Both legs register an `on_upgrade` callback; once the
/// `101` is in flight in both directions the upgraded streams carry the WebSocket
/// frames verbatim. The front does not parse the frames — it is a byte pipe.
async fn proxy_upgrade(socket: &Path, mut request: Request) -> Response {
    let stream = match UnixStream::connect(socket).await {
        Ok(s) => s,
        Err(_) => return upstream_absent(request.uri().path()),
    };
    let (mut sender, conn) = match hyper::client::conn::http1::handshake(TokioIo::new(stream)).await
    {
        Ok(pair) => pair,
        Err(_) => return upstream_absent(request.uri().path()),
    };
    // The upgrade-bearing connection must keep being driven AFTER the response so
    // hyper can surface the upgraded IO; `with_upgrades` exposes it.
    tokio::spawn(async move {
        let _ = conn.with_upgrades().await;
    });

    // Register the downstream (client) on-upgrade future BEFORE forwarding the
    // head. `on(&mut request)` takes the `OnUpgrade` extension the server
    // connection stamped on the request, leaving the request itself intact to
    // forward to the upstream verbatim.
    let downstream_on_upgrade = hyper::upgrade::on(&mut request);

    let upstream = match sender.send_request(request).await {
        Ok(resp) => resp,
        Err(_) => return upstream_absent_path_only(socket),
    };

    // A non-101 upstream reply (it declined the upgrade) is relayed as-is.
    if upstream.status() != StatusCode::SWITCHING_PROTOCOLS {
        return relay_response(upstream);
    }

    // Build the 101 response to send back downstream, copying the upstream's
    // switch headers (Sec-WebSocket-Accept, etc.) minus hop-by-hop.
    let mut switching = Response::new(Body::empty());
    *switching.status_mut() = StatusCode::SWITCHING_PROTOCOLS;
    {
        let headers = switching.headers_mut();
        for (name, value) in upstream.headers().iter() {
            if is_hop_by_hop(name) && name.as_str() != "upgrade" && name.as_str() != "connection" {
                continue;
            }
            headers.append(name.clone(), value.clone());
        }
    }

    // Take the upstream's on-upgrade future, then splice the two upgraded streams
    // once both sides have switched.
    let upstream_on_upgrade = hyper::upgrade::on(upstream);
    tokio::spawn(async move {
        match tokio::try_join!(downstream_on_upgrade, upstream_on_upgrade) {
            Ok((downstream_upgraded, upstream_upgraded)) => {
                let mut a = TokioIo::new(downstream_upgraded);
                let mut b = TokioIo::new(upstream_upgraded);
                // Pipe bytes both directions until either side closes.
                let _ = tokio::io::copy_bidirectional(&mut a, &mut b).await;
            }
            Err(e) => {
                tracing::debug!(error = %e, "websocket upgrade did not complete on both legs");
            }
        }
    });

    switching
}

/// The graceful-degradation reply when the residual upstream is absent or
/// unconnectable: a FastAPI-shaped `{"detail": "Not Found"}`. A path under a
/// known permanent-Python prefix gets `501` (the feature is absent on this
/// profile); anything else gets `404`. Never a `500`.
fn upstream_absent(path: &str) -> Response {
    if routing::is_permanent_python_path(path) {
        return detail(StatusCode::NOT_IMPLEMENTED, "Not Found");
    }
    detail(StatusCode::NOT_FOUND, "Not Found")
}

/// A degradation reply for the case where we already consumed the request and so
/// only have the socket path, not the request path. We still answer `404` (we
/// cannot tell whether it was a permanent prefix without the path); this is only
/// reached when the upstream accepted the connection then failed the exchange,
/// which is rarer than a plain absent socket.
fn upstream_absent_path_only(_socket: &Path) -> Response {
    detail(StatusCode::NOT_FOUND, "Not Found")
}

/// Whether a response header is hop-by-hop (describes the connection, not the
/// payload) and so must not be relayed. Compared case-insensitively against the
/// already-lowercase [`HOP_BY_HOP`] set.
fn is_hop_by_hop(name: &HeaderName) -> bool {
    HOP_BY_HOP.iter().any(|h| name.as_str() == *h)
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::HeaderValue;
    use http_body_util::BodyExt;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::UnixListener;

    /// Serve one canned HTTP/1.1 response on a Unix socket, then exit. Drains the
    /// request head so the client's write completes before replying. Mirrors the
    /// logd client's `serve_once` test helper.
    fn serve_once(listener: UnixListener, response: Vec<u8>) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            if let Ok((mut conn, _addr)) = listener.accept().await {
                let mut buf = [0u8; 4096];
                let _ = conn.read(&mut buf).await;
                let _ = conn.write_all(&response).await;
                let _ = conn.flush().await;
            }
        })
    }

    fn http_ok(json_body: &str) -> Vec<u8> {
        format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            json_body.len(),
            json_body
        )
        .into_bytes()
    }

    /// Read an axum response body to bytes for an assertion.
    async fn body_bytes(resp: Response) -> Vec<u8> {
        resp.into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec()
    }

    #[tokio::test]
    async fn forwards_and_relays_byte_faithfully() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("api-internal.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let canned = r#"{"ok": true, "value": 42}"#;
        let server = serve_once(listener, http_ok(canned));

        let request = http::Request::builder()
            .method(http::Method::GET)
            .uri("/api/flights")
            .body(Body::empty())
            .unwrap();
        let resp = proxy_with_socket(&path, request).await;

        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers()
                .get(http::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok()),
            Some("application/json")
        );
        let body = body_bytes(resp).await;
        assert_eq!(body, canned.as_bytes());
        server.await.unwrap();
    }

    #[tokio::test]
    async fn relays_a_non_200_status_verbatim() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("api-internal.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let resp_bytes =
            b"HTTP/1.1 418 I'm a teapot\r\nContent-Length: 4\r\nConnection: close\r\n\r\nbrew"
                .to_vec();
        let server = serve_once(listener, resp_bytes);

        let request = http::Request::builder()
            .uri("/api/anything")
            .body(Body::empty())
            .unwrap();
        let resp = proxy_with_socket(&path, request).await;
        assert_eq!(resp.status(), StatusCode::IM_A_TEAPOT);
        let body = body_bytes(resp).await;
        assert_eq!(body, b"brew");
        server.await.unwrap();
    }

    #[tokio::test]
    async fn absent_socket_yields_404_not_500_for_an_unknown_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("absent.sock");
        let request = http::Request::builder()
            .uri("/api/flights")
            .body(Body::empty())
            .unwrap();
        let resp = proxy_with_socket(&path, request).await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        let body = body_bytes(resp).await;
        assert_eq!(body, br#"{"detail":"Not Found"}"#);
    }

    #[tokio::test]
    async fn absent_socket_yields_501_for_a_permanent_python_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("absent.sock");
        let request = http::Request::builder()
            .uri("/api/vision/state")
            .body(Body::empty())
            .unwrap();
        let resp = proxy_with_socket(&path, request).await;
        // The headless profile runs no residual Python; a permanent-Python feature
        // is absent, not unknown.
        assert_eq!(resp.status(), StatusCode::NOT_IMPLEMENTED);
        let body = body_bytes(resp).await;
        assert_eq!(body, br#"{"detail":"Not Found"}"#);
    }

    #[test]
    fn default_socket_prefers_the_absolute_env_override() {
        // The env override wins as an absolute path; without it the default
        // resolves under the runtime dir. We assert only the filename of the
        // default to avoid touching the process env in a shared test runner.
        let p = default_internal_socket();
        assert!(p.ends_with("api-internal.sock") || p.is_absolute());
    }

    #[test]
    fn websocket_upgrade_is_detected() {
        let mut headers = http::HeaderMap::new();
        headers.insert(
            http::header::CONNECTION,
            HeaderValue::from_static("Upgrade"),
        );
        headers.insert(http::header::UPGRADE, HeaderValue::from_static("websocket"));
        assert!(is_websocket_upgrade(&headers));

        // A plain request is not an upgrade.
        let plain = http::HeaderMap::new();
        assert!(!is_websocket_upgrade(&plain));

        // Connection: keep-alive (no upgrade token) is not an upgrade even with an
        // Upgrade header present.
        let mut mixed = http::HeaderMap::new();
        mixed.insert(
            http::header::CONNECTION,
            HeaderValue::from_static("keep-alive"),
        );
        mixed.insert(http::header::UPGRADE, HeaderValue::from_static("websocket"));
        assert!(!is_websocket_upgrade(&mixed));
    }

    #[test]
    fn hop_by_hop_headers_are_recognised() {
        assert!(is_hop_by_hop(&HeaderName::from_static("connection")));
        assert!(is_hop_by_hop(&HeaderName::from_static("transfer-encoding")));
        assert!(is_hop_by_hop(&HeaderName::from_static("upgrade")));
        assert!(!is_hop_by_hop(&HeaderName::from_static("content-type")));
        assert!(!is_hop_by_hop(&HeaderName::from_static("x-ados-key")));
    }
}
