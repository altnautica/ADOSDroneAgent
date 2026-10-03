//! `GET /hls/*`: the HLS playback fallback, relayed to mediamtx's loopback HLS
//! server.
//!
//! The native status advertises `/hls/<leg>/index.m3u8` for browsers that cannot
//! open a WebRTC session (a remote or mixed-content viewer). mediamtx serves HLS
//! on loopback only, so this front is the way in: the auth edge has already
//! admitted the request (the media plane accepts a dashboard session in the
//! query string), and this module forwards a fresh `GET`/`HEAD` with the path,
//! the query minus that credential, and only the content-negotiation headers a
//! playlist or segment fetch needs. No `X-ADOS-Key`, `Authorization` or cookie
//! ever reaches mediamtx, and its logs never carry a credential.
//!
//! The response streams through unbuffered ([`crate::proxy::relay_response`]),
//! so a segment flows to the player as mediamtx produces it and a low-latency
//! blocking playlist reload is held open rather than cut.

use std::time::Duration;

use axum::extract::Request;
use axum::http::{header, HeaderName, Method, StatusCode};
use axum::response::Response;
use bytes::Bytes;
use http_body_util::Empty;
use hyper_util::rt::TokioIo;
use tokio::net::TcpStream;

use crate::routes::detail;
use crate::serve::MEDIA_SESSION_QUERY_KEY;

/// The loopback port mediamtx serves HLS on. Read from `ados-video`, which owns
/// the mediamtx config that binds it.
const HLS_PORT: u16 = ados_video::mediamtx::DEFAULT_HLS_PORT;

/// mediamtx is on the same box: a connect that does not complete promptly means
/// it is not running.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);

/// How long mediamtx has to answer with a response head. A low-latency HLS
/// client asks for the next playlist part before it exists and mediamtx holds
/// that request until the part is ready, so this covers a few segment
/// durations rather than a local disk read.
const HEAD_TIMEOUT: Duration = Duration::from_secs(15);

/// The longest relayed path, a bound on what a client can make this front
/// forward.
const MAX_PATH: usize = 256;

/// The request headers a playlist or segment fetch needs. Everything else,
/// credentials first of all, stays at the edge.
const FORWARDED_HEADERS: [HeaderName; 5] = [
    header::ACCEPT,
    header::ACCEPT_ENCODING,
    header::RANGE,
    header::IF_NONE_MATCH,
    header::IF_MODIFIED_SINCE,
];

/// The mediamtx path for a front `/hls/...` request path, or `None` when the
/// path is not a playlist or segment name this front will forward.
///
/// An allow-list: each segment is non-empty, not `.` or `..`, and made of
/// `[A-Za-z0-9._-]`. A stream name and the file names mediamtx generates never
/// need more, and anything that could read as a parent hop or a request-line
/// break is refused rather than escaped.
fn upstream_path(path: &str) -> Option<&str> {
    let rest = path.strip_prefix("/hls/")?;
    if rest.is_empty() || rest.len() > MAX_PATH {
        return None;
    }
    let valid = rest.split('/').all(|seg| {
        !seg.is_empty()
            && seg != "."
            && seg != ".."
            && seg
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
    });
    // Strip only the `/hls` mount, keeping the leading slash for the upstream.
    valid.then(|| &path[4..])
}

/// The query string to forward: the client's, minus the dashboard session the
/// edge already consumed. `None` when nothing is left.
fn upstream_query(query: Option<&str>) -> Option<String> {
    let kept: Vec<&str> = query?
        .split('&')
        .filter(|pair| !pair.is_empty() && pair.split('=').next() != Some(MEDIA_SESSION_QUERY_KEY))
        .collect();
    (!kept.is_empty()).then(|| kept.join("&"))
}

/// Relay one HLS request to mediamtx on loopback.
pub async fn proxy_hls(request: Request) -> Response {
    proxy_hls_to(HLS_PORT, request).await
}

async fn proxy_hls_to(port: u16, request: Request) -> Response {
    let method = request.method().clone();
    if method != Method::GET && method != Method::HEAD {
        return detail(StatusCode::METHOD_NOT_ALLOWED, "Method Not Allowed");
    }
    let Some(path) = upstream_path(request.uri().path()) else {
        return detail(StatusCode::NOT_FOUND, "Not Found");
    };
    let target = match upstream_query(request.uri().query()) {
        Some(q) => format!("{path}?{q}"),
        None => path.to_owned(),
    };

    let mut builder = http::Request::builder()
        .method(method)
        .uri(target)
        .header(header::HOST, format!("127.0.0.1:{port}"));
    for name in &FORWARDED_HEADERS {
        if let Some(value) = request.headers().get(name) {
            builder = builder.header(name, value);
        }
    }
    let Ok(upstream_request) = builder.body(Empty::<Bytes>::new()) else {
        return detail(StatusCode::NOT_FOUND, "Not Found");
    };

    let stream = match tokio::time::timeout(
        CONNECT_TIMEOUT,
        TcpStream::connect(("127.0.0.1", port)),
    )
    .await
    {
        Ok(Ok(s)) => s,
        _ => {
            return detail(
                StatusCode::SERVICE_UNAVAILABLE,
                "upstream media endpoint unreachable",
            )
        }
    };
    let (mut sender, conn) = match tokio::time::timeout(
        HEAD_TIMEOUT,
        hyper::client::conn::http1::handshake(TokioIo::new(stream)),
    )
    .await
    {
        Ok(Ok(pair)) => pair,
        _ => {
            return detail(
                StatusCode::SERVICE_UNAVAILABLE,
                "upstream media endpoint unreachable",
            )
        }
    };
    // The connection future drives the exchange; it ends when the body has been
    // read or either side closes, neither of which is a fault.
    let conn_task = tokio::spawn(async move {
        let _ = conn.await;
    });
    match tokio::time::timeout(HEAD_TIMEOUT, sender.send_request(upstream_request)).await {
        Ok(Ok(upstream)) => crate::proxy::relay_response(upstream),
        Ok(Err(_)) => {
            conn_task.abort();
            detail(StatusCode::BAD_GATEWAY, "upstream media endpoint failed")
        }
        Err(_) => {
            conn_task.abort();
            detail(
                StatusCode::GATEWAY_TIMEOUT,
                "upstream media endpoint timed out",
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[test]
    fn only_stream_and_file_names_are_forwarded() {
        assert_eq!(
            upstream_path("/hls/main/index.m3u8"),
            Some("/main/index.m3u8")
        );
        assert_eq!(
            upstream_path("/hls/ir/abc_seg12.mp4"),
            Some("/ir/abc_seg12.mp4")
        );
        for bad in [
            "/hls/",
            "/hls/../9997/v3/paths/list",
            "/hls/main/../../x",
            "/hls/main//index.m3u8",
            "/hls/main/index.m3u8%0d%0a",
            "/hls/main/./index.m3u8",
            "/hlsx/main/index.m3u8",
        ] {
            assert_eq!(upstream_path(bad), None, "{bad} must not be forwarded");
        }
    }

    #[test]
    fn the_dashboard_session_never_leaves_the_edge() {
        assert_eq!(upstream_query(Some("ados_session=v1%7Cx")), None);
        assert_eq!(
            upstream_query(Some("_HLS_msn=4&ados_session=v1%7Cx&_HLS_part=1")),
            Some("_HLS_msn=4&_HLS_part=1".to_owned())
        );
        assert_eq!(upstream_query(None), None);
    }

    /// The whole request line and header block mediamtx receives: the path is
    /// remapped, the session query and every credential header are gone, and
    /// the response streams back with its status.
    #[tokio::test]
    async fn credentials_are_stripped_before_mediamtx_sees_the_request() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let upstream = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 4096];
            let mut seen = Vec::new();
            loop {
                let n = sock.read(&mut buf).await.unwrap();
                seen.extend_from_slice(&buf[..n]);
                if n == 0 || seen.windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
            }
            sock.write_all(
                b"HTTP/1.1 200 OK\r\ncontent-type: application/vnd.apple.mpegurl\r\ncontent-length: 7\r\n\r\n#EXTM3U",
            )
            .await
            .unwrap();
            String::from_utf8(seen).unwrap()
        });

        let request = Request::builder()
            .method(Method::GET)
            .uri("/hls/main/index.m3u8?ados_session=v1%7Csecret&_HLS_msn=3")
            .header("x-ados-key", "secret-key")
            .header(header::AUTHORIZATION, "Bearer secret")
            .header(header::COOKIE, "s=secret")
            .header(header::ACCEPT, "*/*")
            .body(Body::empty())
            .unwrap();
        let response = proxy_hls_to(port, request).await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(&body[..], b"#EXTM3U");

        let seen = upstream.await.unwrap();
        assert!(
            seen.starts_with("GET /main/index.m3u8?_HLS_msn=3 HTTP/1.1\r\n"),
            "{seen}"
        );
        let lower = seen.to_ascii_lowercase();
        assert!(
            !lower.contains("secret"),
            "a credential reached mediamtx: {seen}"
        );
        assert!(lower.contains("accept: */*"));
    }

    #[tokio::test]
    async fn an_absent_mediamtx_is_a_503_not_a_hang() {
        // Bind then drop, so the port is known to refuse.
        let port = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        };
        let request = Request::builder()
            .uri("/hls/main/index.m3u8")
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            proxy_hls_to(port, request).await.status(),
            StatusCode::SERVICE_UNAVAILABLE
        );
    }
}
