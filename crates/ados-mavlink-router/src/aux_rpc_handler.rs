//! Drone-side handler for relay-proxy HTTP requests arriving over the aux
//! uplink.
//!
//! The ground station, paired to this drone only over WFB, sends a Request
//! frame on the aux uplink (radio_id 3). The `aux_uplink_consumer` decodes it
//! and hands it here. This module forwards the request to the drone's own HTTP
//! API (ados-control, 127.0.0.1:8080), encodes the response as an RPC Response
//! frame, and radiates it back over the aux downlink (radio_id 2) via
//! [`AuxEgress`].
//!
//! ## No external HTTP client dependency
//!
//! The drone's HTTP server is on localhost:8080 — no TLS, no redirects, no
//! chunked transfer encoding needed. A minimal HTTP/1.1 client over a raw TCP
//! stream suffices: write the request line + headers + body, read the status
//! line + headers + body. Keeping this dep-free avoids pulling a full HTTP
//! client crate into the mavlink router.
//!
//! ## Bounded failure
//!
//! Every call is bounded by [`HTTP_TIMEOUT`]. A drone whose own HTTP server is
//! slow or wedged must not park the uplink consumer's read loop: the handler
//! runs in its own spawned task, and a timeout returns a 503 to the ground so
//! the operator sees the wedge rather than a silent stall.

use std::time::Duration;

use ados_protocol::aux_egress::AuxEgress;
use ados_protocol::aux_mux::AuxChannel;
use ados_protocol::aux_rpc::{self, RpcMethod, RpcRequest};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// The drone's HTTP API is always on localhost.
const HTTP_HOST: &str = "127.0.0.1";
const HTTP_PORT: u16 = 8080;

/// Bound on one proxied HTTP call. The drone's own API responds in
/// milliseconds for most endpoints; 5 seconds is enough for heavier calls
/// (config writes, param loads) while surfacing a wedge to the operator.
const HTTP_TIMEOUT: Duration = Duration::from_secs(5);

/// Handle one RPC request: forward to the local HTTP API and send the
/// response back over the aux egress.
///
/// Runs as its own task so the uplink consumer's read loop never stalls behind
/// a slow HTTP call. A failure at any stage still sends a Response with a 5xx
/// status back to the ground, so the caller's proxy times out gracefully
/// rather than waiting for the full RPC_TIMEOUT.
pub async fn handle(request: &RpcRequest<'_>, egress: &AuxEgress) {
    let id = request.id;
    let result = http_call(request.method, request.path, request.body).await;
    let (status, body) = result.unwrap_or_else(|e| {
        tracing::warn!(error = %e, request_id = id, "aux_rpc_http_call_failed");
        (503u16, Vec::new())
    });

    let response_payload = match aux_rpc::encode_response(id, status, &body) {
        Some(p) => p,
        None => {
            // Response body too large for one aux frame. Truncate is wrong —
            // the caller would decode it as 200 with garbage. Send a 413.
            tracing::warn!(
                len = body.len(),
                request_id = id,
                "aux_rpc_response_too_large"
            );
            aux_rpc::encode_response(id, 413, b"response body exceeds one aux frame")
                .unwrap_or_default()
        }
    };

    if let Err(e) = egress.send(AuxChannel::Response, &response_payload).await {
        tracing::warn!(error = %e, request_id = id, "aux_rpc_response_send_failed");
    }
}

/// A minimal HTTP/1.1 client for localhost. No TLS, no redirects, no
/// streaming — just a request/response round trip to the drone's own API.
async fn http_call(
    method: RpcMethod,
    path: &[u8],
    body: &[u8],
) -> Result<(u16, Vec<u8>), HttpError> {
    let path_str = std::str::from_utf8(path).map_err(|_| HttpError::BadPath)?;
    let method_str = method.as_http_method();

    let work = async {
        let mut stream = TcpStream::connect((HTTP_HOST, HTTP_PORT))
            .await
            .map_err(|e| HttpError::Connect(e.to_string()))?;

        // Build the HTTP/1.1 request.
        let request = format!(
            "{method_str} {path_str} HTTP/1.1\r\nHost: localhost\r\nContent-Length: {body_len}\r\nConnection: close\r\n\r\n",
            body_len = body.len(),
        );
        stream
            .write_all(request.as_bytes())
            .await
            .map_err(|e| HttpError::Write(e.to_string()))?;
        if !body.is_empty() {
            stream
                .write_all(body)
                .await
                .map_err(|e| HttpError::Write(e.to_string()))?;
        }

        // Read the full response. Connection: close means the server closes
        // after the response, so we read until EOF.
        let mut response = Vec::new();
        stream
            .read_to_end(&mut response)
            .await
            .map_err(|e| HttpError::Read(e.to_string()))?;

        parse_response(&response)
    };

    tokio::time::timeout(HTTP_TIMEOUT, work)
        .await
        .map_err(|_| HttpError::Timeout)?
}

/// Parse a raw HTTP/1.1 response into (status, body). Extracts the status
/// code from the first line and the body after the header/body separator.
fn parse_response(raw: &[u8]) -> Result<(u16, Vec<u8>), HttpError> {
    // Find the header/body boundary: \r\n\r\n.
    let boundary = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or(HttpError::NoHeaderBoundary)?;

    let headers = &raw[..boundary];
    let body = &raw[boundary + 4..];

    // Parse the status line: "HTTP/1.1 200 OK\r\n..."
    let headers_str = std::str::from_utf8(headers).map_err(|_| HttpError::MalformedStatus)?;
    let first_line = headers_str
        .lines()
        .next()
        .ok_or(HttpError::MalformedStatus)?;
    let mut parts = first_line.split_whitespace();
    let _version = parts.next().ok_or(HttpError::MalformedStatus)?;
    let status_str = parts.next().ok_or(HttpError::MalformedStatus)?;
    let status: u16 = status_str.parse().map_err(|_| HttpError::MalformedStatus)?;

    Ok((status, body.to_vec()))
}

#[derive(Debug)]
enum HttpError {
    BadPath,
    Connect(String),
    Write(String),
    Read(String),
    Timeout,
    NoHeaderBoundary,
    MalformedStatus,
}

impl std::fmt::Display for HttpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BadPath => write!(f, "request path is not valid UTF-8"),
            Self::Connect(e) => write!(f, "connect failed: {e}"),
            Self::Write(e) => write!(f, "write failed: {e}"),
            Self::Read(e) => write!(f, "read failed: {e}"),
            Self::Timeout => write!(f, "HTTP call timed out"),
            Self::NoHeaderBoundary => write!(f, "response has no header/body boundary"),
            Self::MalformedStatus => write!(f, "malformed status line"),
        }
    }
}

impl std::error::Error for HttpError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_200_response_with_a_body() {
        let raw =
            b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 5\r\n\r\nhello";
        let (status, body) = parse_response(raw).unwrap();
        assert_eq!(status, 200);
        assert_eq!(body, b"hello");
    }

    #[test]
    fn parses_a_404_response_with_an_empty_body() {
        let raw = b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n";
        let (status, body) = parse_response(raw).unwrap();
        assert_eq!(status, 404);
        assert!(body.is_empty());
    }

    #[test]
    fn parses_a_500_response_with_a_long_body() {
        let body = vec![b'x'; 500];
        let raw = format!(
            "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 500\r\n\r\n{}",
            String::from_utf8(body.clone()).unwrap()
        );
        let (status, parsed_body) = parse_response(raw.as_bytes()).unwrap();
        assert_eq!(status, 500);
        assert_eq!(parsed_body, body);
    }

    #[test]
    fn rejects_a_response_with_no_header_boundary() {
        let raw = b"HTTP/1.1 200 OK";
        assert!(parse_response(raw).is_err());
    }

    #[test]
    fn rejects_a_malformed_status_line() {
        let raw = b"NOT HTTP\r\n\r\nbody";
        assert!(parse_response(raw).is_err());
    }

    #[test]
    fn handles_a_body_that_contains_the_boundary_pattern() {
        // The body itself contains \r\n\r\n — only the FIRST boundary splits.
        let raw = b"HTTP/1.1 200 OK\r\nContent-Length: 8\r\n\r\na\r\n\r\nb";
        let (status, body) = parse_response(raw).unwrap();
        assert_eq!(status, 200);
        assert_eq!(body, b"a\r\n\r\nb");
    }
}
