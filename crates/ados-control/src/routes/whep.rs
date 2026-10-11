//! `/whep`: the WebRTC playback exchange, relayed to mediamtx's loopback WHEP
//! server.
//!
//! mediamtx serves WHEP per published leg at `127.0.0.1:8889/<leg>/whep` and
//! binds loopback only, so this front is the way in. A browser reaches the live
//! video plane through the same host and port as the REST surface: the status
//! routes advertise the relative `/whep` (primary leg) and `/whep?camera=<id>`
//! (a secondary leg), which resolve against whatever host reached the agent.
//!
//! - `POST /whep[?camera=<leg>]` forwards the SDP offer and returns the answer.
//! - `DELETE /whep/{session_id}[?camera=<leg>]` tears the session down.
//! - `PATCH /whep/{session_id}[?camera=<leg>]` forwards a trickle-ICE fragment.
//!
//! The auth edge has already admitted the request (key, dashboard session in a
//! header or the `ados_session` query, or on-box). Every credential and front
//! trust header is stripped before the request leaves, so none reaches mediamtx
//! or its logs. The offer's `Location` (mediamtx's `/<leg>/whep/<session>`) is
//! rewritten to this front's own `/whep/<session>?camera=<leg>`, so the client's
//! DELETE and PATCH come back through here instead of dialing a loopback address
//! the browser cannot reach.

use std::time::Duration;

use axum::body::to_bytes;
use axum::extract::{Path, Request};
use axum::http::{header, HeaderMap, HeaderValue, Method, StatusCode};
use axum::response::Response;
use bytes::Bytes;
use http_body_util::Full;
use hyper_util::rt::TokioIo;
use tokio::net::TcpStream;

use crate::routes::detail;

/// The loopback port mediamtx serves WHEP on. Read from `ados-video`, which owns
/// the mediamtx config that binds it.
const WHEP_PORT: u16 = ados_video::mediamtx::DEFAULT_WEBRTC_PORT;

/// The primary published leg. A multi-leg node addresses a secondary leg by
/// `?camera=<id>`.
const DEFAULT_CAMERA: &str = "main";

/// mediamtx is on the same box: a connect that does not complete promptly means
/// it is not running.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);

/// How long mediamtx has to answer with a response head. The exchange is a few
/// KB, so a stalled mediamtx fails fast rather than holding a renegotiation.
const HEAD_TIMEOUT: Duration = Duration::from_secs(10);

/// The largest SDP offer or ICE fragment relayed. An offer is a few KB; mediamtx
/// expects a known `Content-Length`, so the body is read in full first.
const MAX_BODY: usize = 64 * 1024;

/// Request headers never forwarded: hop-by-hop headers (scoped to one TCP hop),
/// `host` and `content-length` (rewritten for the upstream), and every client
/// credential or front-set trust header. mediamtx needs none of the latter, and
/// forwarding them would land the pairing key and session tokens in its logs.
const STRIPPED_REQUEST_HEADERS: [&str; 17] = [
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
    "host",
    "content-length",
    "x-ados-key",
    "authorization",
    "cookie",
    "x-ados-session",
    "x-ados-dashboard-session",
    crate::serve::ONBOX_HEADER,
    crate::mcp::MCP_SCOPES_HEADER,
];

/// `POST /whep`: the SDP offer for the requested leg.
pub async fn post_whep(request: Request) -> Response {
    let camera = match camera_id(request.uri().query()) {
        Ok(c) => c,
        Err(refused) => return *refused,
    };
    let path = format!("/{camera}/whep");
    forward(WHEP_PORT, Method::POST, &path, request, Some(&camera)).await
}

/// `DELETE /whep/{session_id}`: tear down the requested leg's session.
pub async fn delete_whep(Path(session_id): Path<String>, request: Request) -> Response {
    session_request(Method::DELETE, &session_id, request).await
}

/// `PATCH /whep/{session_id}`: a trickle-ICE fragment (ICE restart).
pub async fn patch_whep(Path(session_id): Path<String>, request: Request) -> Response {
    session_request(Method::PATCH, &session_id, request).await
}

async fn session_request(method: Method, session_id: &str, request: Request) -> Response {
    let camera = match camera_id(request.uri().query()) {
        Ok(c) => c,
        Err(refused) => return *refused,
    };
    // The session id becomes a mediamtx path segment. axum has percent-decoded
    // it, so an encoded `/` would otherwise address another mediamtx route.
    if !is_path_safe(session_id) {
        return detail(StatusCode::BAD_REQUEST, "invalid session id");
    }
    let path = format!("/{camera}/whep/{session_id}");
    forward(WHEP_PORT, method, &path, request, None).await
}

/// `[A-Za-z0-9_-]+`: the roster's own leg-id rule, and what a mediamtx session
/// id is made of.
fn is_path_safe(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
}

/// The requested leg from `?camera=`, defaulting to the primary `main`. A value
/// outside `[A-Za-z0-9_-]+` (empty, encoded, a traversal) is a 400, never a path
/// into another mediamtx route.
pub(crate) fn camera_id(query: Option<&str>) -> Result<String, Box<Response>> {
    let requested = query.and_then(|q| {
        q.split('&')
            .filter_map(|pair| pair.split_once('=').or(Some((pair, ""))))
            .find(|(k, _)| *k == "camera")
            .map(|(_, v)| v)
    });
    match requested {
        None => Ok(DEFAULT_CAMERA.to_owned()),
        Some(v) if is_path_safe(v) => Ok(v.to_owned()),
        Some(_) => Err(Box::new(detail(
            StatusCode::BAD_REQUEST,
            "invalid camera id",
        ))),
    }
}

/// The request headers forwarded upstream: everything but the stripped set.
pub(crate) fn upstream_headers(incoming: &HeaderMap) -> HeaderMap {
    let mut out = HeaderMap::with_capacity(incoming.len());
    for (name, value) in incoming {
        if STRIPPED_REQUEST_HEADERS.contains(&name.as_str()) {
            continue;
        }
        out.append(name.clone(), value.clone());
    }
    out
}

/// Rewrite a mediamtx WHEP `Location` (`/<camera>/whep/<session>`) to this
/// front's `/whep/<session>?camera=<camera>`. An absent or unexpected
/// `Location` is left untouched: the exchange still succeeds, only explicit
/// teardown is affected.
fn rewrite_location(headers: &mut HeaderMap, camera: &str) {
    let Some(loc) = headers.get(header::LOCATION).and_then(|v| v.to_str().ok()) else {
        return;
    };
    let prefix = format!("/{camera}/whep");
    let Some(rest) = loc.strip_prefix(prefix.as_str()) else {
        return;
    };
    if let Ok(value) = HeaderValue::from_str(&format!("/whep{rest}?camera={camera}")) {
        headers.insert(header::LOCATION, value);
    }
}

/// Relay one request to mediamtx's WHEP server on `127.0.0.1:<port>`. The
/// response streams back unbuffered; `rewrite_for` names the leg whose offer
/// `Location` is rewritten.
pub(crate) async fn forward(
    port: u16,
    method: Method,
    path: &str,
    request: Request,
    rewrite_for: Option<&str>,
) -> Response {
    let (parts, body) = request.into_parts();
    let Ok(body) = to_bytes(body, MAX_BODY).await else {
        return detail(StatusCode::PAYLOAD_TOO_LARGE, "request body too large");
    };
    let mut builder = http::Request::builder()
        .method(method)
        .uri(path)
        .header(header::HOST, format!("127.0.0.1:{port}"));
    if let Some(headers) = builder.headers_mut() {
        headers.extend(upstream_headers(&parts.headers));
    }
    let Ok(upstream_request) = builder.body(Full::<Bytes>::new(body)) else {
        return detail(StatusCode::BAD_REQUEST, "invalid request");
    };

    let unreachable = || {
        detail(
            StatusCode::SERVICE_UNAVAILABLE,
            "upstream media endpoint unreachable",
        )
    };
    let stream = match tokio::time::timeout(
        CONNECT_TIMEOUT,
        TcpStream::connect(("127.0.0.1", port)),
    )
    .await
    {
        Ok(Ok(s)) => s,
        _ => return unreachable(),
    };
    let (mut sender, conn) = match tokio::time::timeout(
        HEAD_TIMEOUT,
        hyper::client::conn::http1::handshake(TokioIo::new(stream)),
    )
    .await
    {
        Ok(Ok(pair)) => pair,
        _ => return unreachable(),
    };
    // The connection future drives the exchange; it ends when the body has been
    // read or either side closes, neither of which is a fault.
    let conn_task = tokio::spawn(async move {
        let _ = conn.await;
    });
    match tokio::time::timeout(HEAD_TIMEOUT, sender.send_request(upstream_request)).await {
        Ok(Ok(upstream)) => {
            let mut response = crate::proxy::relay_response(upstream);
            if let Some(camera) = rewrite_for {
                rewrite_location(response.headers_mut(), camera);
            }
            response
        }
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
#[path = "whep_tests.rs"]
mod tests;
