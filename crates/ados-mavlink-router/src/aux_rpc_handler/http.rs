//! The loopback HTTP leg: request build, bounded read and response parse.

use super::*;

/// The drone's HTTP API is always on localhost.
pub(super) const HTTP_HOST: &str = "127.0.0.1";

pub(super) const HTTP_PORT: u16 = 8080;

/// Bound on one proxied HTTP call. The drone's own API responds in
/// milliseconds for most endpoints; 5 seconds is enough for heavier calls
/// (config writes, param loads) while surfacing a wedge to the operator.
pub(super) const HTTP_TIMEOUT: Duration = Duration::from_secs(5);

/// Headroom above the response-body ceiling for the HTTP status line and
/// headers, which are read on the same stream and stripped afterwards.
pub(super) const HTTP_HEADER_ALLOWANCE: usize = 8 * 1024;

/// Read a response off `r` up to the ceiling the response encoder already
/// enforces, rather than to EOF.
///
/// `Connection: close` means the server closes after the response, so reading
/// to EOF terminates — but only when the far end chooses to stop, and the size
/// check ran after the body was fully buffered. With one task per relay
/// request, N concurrent calls to a large-response route each held their own
/// copy on a board with a few hundred megabytes.
///
/// The cap carries one byte past the ceiling so an over-ceiling body is still
/// detected as over-ceiling downstream, rather than being truncated to exactly
/// the limit and encoded as if it had fit.
pub(super) async fn read_response_bounded<R: tokio::io::AsyncRead + Unpin>(
    r: R,
) -> Result<Vec<u8>, HttpError> {
    let cap = ados_protocol::aux_rpc::MAX_RESPONSE_BODY + HTTP_HEADER_ALLOWANCE + 1;
    let mut buf = Vec::new();
    r.take(cap as u64)
        .read_to_end(&mut buf)
        .await
        .map_err(|e| HttpError::Read(e.to_string()))?;
    Ok(buf)
}

/// A minimal HTTP/1.1 client for localhost. No TLS, no redirects, no
/// streaming — just a request/response round trip to the drone's own API.
pub(super) async fn http_call(
    request: &RpcRequest,
) -> Result<(u16, Vec<aux_rpc::ResponseHeader>, Vec<u8>), HttpError> {
    let path_str = std::str::from_utf8(&request.path).map_err(|_| HttpError::BadPath)?;
    if !path_is_safe(path_str) {
        return Err(HttpError::BadPath);
    }
    let content_type = std::str::from_utf8(&request.content_type).unwrap_or("");
    let method_str = request.method.as_http_method();
    let body = &request.body;

    let work = async {
        let mut stream = TcpStream::connect((HTTP_HOST, HTTP_PORT))
            .await
            .map_err(|e| HttpError::Connect(e.to_string()))?;

        let head = build_request_head(method_str, path_str, content_type, body.len());
        stream
            .write_all(head.as_bytes())
            .await
            .map_err(|e| HttpError::Write(e.to_string()))?;
        if !body.is_empty() {
            stream
                .write_all(body)
                .await
                .map_err(|e| HttpError::Write(e.to_string()))?;
        }

        let response = read_response_bounded(&mut stream).await?;

        parse_response(&response)
    };

    tokio::time::timeout(HTTP_TIMEOUT, work)
        .await
        .map_err(|_| HttpError::Timeout)?
}

/// Build the HTTP/1.1 request line and headers.
///
/// The caller's `Content-Type` crosses the radio with the request, so a plugin
/// upload or a form body arrives labelled as what it is. A body sent with no
/// type is labelled JSON, which is what every agent write route reads: the
/// Rust front's axum `Json<T>` extractors reject a body with no
/// `application/json` content-type before the handler runs. A type carrying a
/// control character is dropped for the JSON default rather than interpolated,
/// since it would otherwise inject a header into an on-box request.
///
/// `X-ADOS-Relayed: 1` marks the call as having crossed the radio. The front
/// uses it to refuse the trust-root routes over the relay; it is deliberately
/// NOT in `pairing_posture::FORWARDED_HEADERS`, because the ticket checked
/// above is what authenticates the call and it must keep the on-box posture
/// it reaches the local API with.
pub(super) fn build_request_head(
    method: &str,
    path: &str,
    content_type: &str,
    body_len: usize,
) -> String {
    let content_type = if body_len == 0 {
        String::new()
    } else {
        let declared = content_type.trim();
        let declared = if declared.is_empty() || !path_is_safe(&declared.replace(' ', "")) {
            "application/json"
        } else {
            declared
        };
        format!("Content-Type: {declared}\r\n")
    };
    format!(
        "{method} {path} HTTP/1.1\r\nHost: localhost\r\nX-ADOS-Relayed: 1\r\n{content_type}Content-Length: {body_len}\r\nConnection: close\r\n\r\n"
    )
}

/// Whether a relayed path may be interpolated into an HTTP request line.
///
/// The ground forwards the caller's path still percent-encoded, so a raw
/// control character or space here would end or split the request line and
/// let the caller inject arbitrary headers into the request the drone makes to
/// its own API. The ground rejects this too; the drone re-checks because the
/// radio is the trust boundary, and this side must not depend on the other
/// side's validation.
pub(super) fn path_is_safe(path: &str) -> bool {
    !path.bytes().any(|b| b <= 0x20 || b == 0x7F)
}

/// The response headers that cross the relay.
///
/// An allow-list, not the whole header map: the lane's per-response budget is
/// [`aux_rpc::MAX_RESPONSE_HEADER_BLOCK`] and every byte here is a byte of body
/// that cannot travel, so only the headers a consumer genuinely branches on
/// ride along.
///
/// - `content-type` — without it a relayed body arrives untyped: a browser or
///   a strict server-side consumer refuses it or renders it as a download.
/// - `content-disposition` — the filename of a log artifact or an export.
/// - `location` — the WHEP proxy's whole value is rewriting this so session
///   teardown routes back through it; that rewrite could never survive the
///   relay while headers were dropped.
/// - `cache-control` — a relayed read that a caching layer then serves stale
///   is a status surface reporting a value nothing measured.
///
/// `content-length` is deliberately absent: the ground rebuilds the body and
/// axum sets the real length, and forwarding a stale one would describe a body
/// that is not the one being sent.
pub(super) const FORWARDED_RESPONSE_HEADERS: &[&str] = &[
    "content-type",
    "content-disposition",
    "location",
    "cache-control",
];

/// Parse a raw HTTP/1.1 response into (status, allow-listed headers, body).
/// Extracts the status code from the first line, the forwarded headers, and
/// the body after the header/body separator.
///
/// `Content-Length` is honoured when the server sends one: the read runs to
/// EOF, so anything past the declared length is not part of this body.
/// `Transfer-Encoding: chunked` is refused rather than forwarded — the body
/// would still carry chunk-size framing lines, and handing those to the ground
/// as the answer produces unparseable JSON with no hint as to why.
pub(super) fn parse_response(
    raw: &[u8],
) -> Result<(u16, Vec<aux_rpc::ResponseHeader>, Vec<u8>), HttpError> {
    // Find the header/body boundary: \r\n\r\n.
    let boundary = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or(HttpError::NoHeaderBoundary)?;

    let headers = &raw[..boundary];
    let body = &raw[boundary + 4..];

    // Parse the status line: "HTTP/1.1 200 OK\r\n..."
    let headers_str = std::str::from_utf8(headers).map_err(|_| HttpError::MalformedStatus)?;
    let mut lines = headers_str.lines();
    let first_line = lines.next().ok_or(HttpError::MalformedStatus)?;
    let mut parts = first_line.split_whitespace();
    let _version = parts.next().ok_or(HttpError::MalformedStatus)?;
    let status_str = parts.next().ok_or(HttpError::MalformedStatus)?;
    let status: u16 = status_str.parse().map_err(|_| HttpError::MalformedStatus)?;

    let mut content_length: Option<usize> = None;
    let mut forwarded: Vec<aux_rpc::ResponseHeader> = Vec::new();
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let (name, value) = (name.trim(), value.trim());
        if name.eq_ignore_ascii_case("transfer-encoding")
            && value
                .split(',')
                .any(|token| token.trim().eq_ignore_ascii_case("chunked"))
        {
            return Err(HttpError::Chunked);
        }
        if name.eq_ignore_ascii_case("content-length") {
            content_length = value.parse().ok();
            continue;
        }
        // Lowercased on the way out so the ground rebuilds one canonical
        // spelling regardless of how the upstream cased it.
        if let Some(canonical) = FORWARDED_RESPONSE_HEADERS
            .iter()
            .find(|h| name.eq_ignore_ascii_case(h))
        {
            forwarded.push((canonical.to_string(), value.to_string()));
        }
    }

    // A body shorter than the declared length means the server closed early;
    // forward what did arrive rather than inventing a failure the ground
    // cannot act on.
    let body = match content_length {
        Some(len) if len <= body.len() => &body[..len],
        _ => body,
    };

    Ok((status, forwarded, body.to_vec()))
}

#[derive(Debug)]
pub(super) enum HttpError {
    BadPath,
    Connect(String),
    Write(String),
    Read(String),
    Timeout,
    NoHeaderBoundary,
    MalformedStatus,
    /// The local API answered with a chunked body, which this minimal client
    /// does not de-frame.
    Chunked,
}

impl HttpError {
    /// The status the ground sees for this failure.
    ///
    /// Everything is a 503 — the drone's own API did not answer — except a
    /// response that did arrive and cannot be forwarded intact, which is a
    /// genuine bad gateway and must not read as a wedged API.
    pub(super) fn status(&self) -> u16 {
        match self {
            Self::Chunked => 502,
            _ => 503,
        }
    }
}

impl std::fmt::Display for HttpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BadPath => write!(f, "request path is not UTF-8 or has control characters"),
            Self::Connect(e) => write!(f, "connect failed: {e}"),
            Self::Write(e) => write!(f, "write failed: {e}"),
            Self::Read(e) => write!(f, "read failed: {e}"),
            Self::Timeout => write!(f, "HTTP call timed out"),
            Self::NoHeaderBoundary => write!(f, "response has no header/body boundary"),
            Self::MalformedStatus => write!(f, "malformed status line"),
            Self::Chunked => write!(f, "response uses chunked transfer-encoding"),
        }
    }
}

impl std::error::Error for HttpError {}
