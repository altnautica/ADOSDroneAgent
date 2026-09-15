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

use std::time::{Duration, Instant};

use ados_protocol::aux_egress::AuxEgress;
use ados_protocol::aux_mux::AuxChannel;
use ados_protocol::aux_rpc::{self, ResponseSymbols, RpcMethod, RpcRequest};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::aux_rpc_dedupe::{Admit, RequestDedupe};
use crate::aux_uplink_consumer::AuxUplinkConsumerCounters;

/// The one relayed path that stays reachable before any credential exists: the
/// ground station's delivery of the per-pair secret itself.
///
/// It necessarily arrives with no ticket, because the drone cannot verify one
/// until it holds the secret the call is carrying. Admitting it is not a hole:
/// [`ados_protocol::relay_ticket::decide_accept`] is first-write-wins, so a
/// drone holding no secret accepts exactly one offer and a drone already
/// holding one refuses to be re-keyed over the air. Without this carve-out an
/// unprovisioned aircraft could never be provisioned, since the delivery rides
/// the lane it is about to gate.
const RELAY_BOOTSTRAP_PATH: &[u8] = b"/api/relay/peer-secret";

/// Whether this request is the credential delivery described on
/// [`RELAY_BOOTSTRAP_PATH`]. Method-exact, and the query string is ignored so a
/// trailing `?` cannot be used to smuggle a different path past the match.
fn is_relay_bootstrap(method: RpcMethod, path: &[u8]) -> bool {
    let bare = path.split(|&b| b == b'?').next().unwrap_or(path);
    method == RpcMethod::Post && bare == RELAY_BOOTSTRAP_PATH
}

/// Decide whether a relayed request may proceed to the local HTTP API.
///
/// Pure over its inputs so the decision can be tested without a radio, a
/// secret file or a running server -- this is the boundary the whole relay
/// credential exists to defend, and a branch buried inside the request handler
/// would only ever be exercised end to end.
///
/// `held` is the secret this drone was given, or `None` when it has none.
/// **None denies.** A relayed request reaches `ados-control` over genuine
/// loopback and is therefore treated as on-box, so a drone with no credential
/// to check would be handing its full authority to anything holding the shared
/// fleet radio key -- and a fleet shares one key by construction. The only
/// exception is [`RELAY_BOOTSTRAP_PATH`], which is how a secret gets on file in
/// the first place.
pub fn authorize(
    held: Option<&str>,
    ticket: &[u8],
    method: RpcMethod,
    path: &[u8],
    own_device_id: &str,
    now: i64,
) -> Result<(), ados_protocol::relay_ticket::RelayTicketError> {
    let Some(secret) = held else {
        if is_relay_bootstrap(method, path) {
            return Ok(());
        }
        return Err(ados_protocol::relay_ticket::RelayTicketError::NoSecret);
    };
    let issuer = ados_protocol::relay_ticket::RelayTicketIssuer::from_secret(secret.as_bytes());
    // A ticket that is not valid UTF-8 cannot be one we minted, and reads as a
    // malformed one rather than being allowed to panic a decode.
    let presented = std::str::from_utf8(ticket).unwrap_or("");
    issuer.verify(presented, own_device_id, now)
}

/// Whether the "this node holds no relay secret" warning has already been
/// emitted this boot.
static RELAY_SECRET_ABSENT_LOGGED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Say once per boot that this node holds no relay secret.
///
/// Without this the ungated state is invisible: the operator sees relayed
/// calls answered 401 and has no way to tell "the credential never arrived"
/// apart from "the ground station is presenting a bad one". Once per boot
/// rather than per request so a ground station retrying every second cannot
/// flood the Black Box store with one fact.
fn note_relay_secret_absent() {
    if RELAY_SECRET_ABSENT_LOGGED.swap(true, std::sync::atomic::Ordering::Relaxed) {
        return;
    }
    tracing::warn!(
        secret_path = ados_protocol::relay_ticket::RELAY_SECRET_PATH,
        "aux_rpc_relay_secret_absent"
    );
}

/// Wall-clock unix seconds, for the relay ticket's expiry check.
///
/// Wall clock rather than a monotonic instant because the ticket's expiry was
/// stamped by the ground station against its own wall clock; a monotonic
/// reading here would be comparing two unrelated origins.
fn now_unix_secs() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// The drone's HTTP API is always on localhost.
const HTTP_HOST: &str = "127.0.0.1";
const HTTP_PORT: u16 = 8080;

/// Bound on one proxied HTTP call. The drone's own API responds in
/// milliseconds for most endpoints; 5 seconds is enough for heavier calls
/// (config writes, param loads) while surfacing a wedge to the operator.
const HTTP_TIMEOUT: Duration = Duration::from_secs(5);

/// Headroom above the response-body ceiling for the HTTP status line and
/// headers, which are read on the same stream and stripped afterwards.
const HTTP_HEADER_ALLOWANCE: usize = 8 * 1024;

/// Pause between response fragments.
///
/// Back-to-back 1.2 KB datagrams into `wfb_tx`'s UDP ingress can overrun its
/// socket buffer and are dropped silently with no error to the sender, which
/// would present on the ground as a permanently `Incomplete` call. 25
/// fragments × 5 ms = 125 ms, comfortably inside the ground station's
/// 10-second call bound.
const FRAGMENT_PACING: Duration = Duration::from_millis(5);

/// One response's fragments go out as an uninterrupted paced burst. Concurrent
/// requests would otherwise each pace independently and feed `wfb_tx`'s ingress
/// at N x the intended rate, which is the overrun FRAGMENT_PACING exists to
/// avoid. Serialising whole responses also keeps a large body's fragments
/// contiguous, which shortens the ground's reassembly window.
static RESPONSE_SEND_SLOT: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(1);

/// How long a response may wait for the send slot before it is abandoned.
///
/// The slot is process-wide and held for a whole burst — about 145 ms for a
/// 29 KB answer — so concurrent requests queue linearly behind one another.
/// That wait used to count against nothing: a response could sit in the queue
/// past the ground's call bound and then transmit anyway, spending uplink
/// airtime on an answer whose caller had already given up, while this side
/// recorded a success.
///
/// Bounding the wait makes the drone agree with the ground about what
/// happened. An abandoned response is not a lost answer: the ground
/// retransmits the request, and the dedupe cache replays the already-computed
/// fragments rather than re-running the call.
///
/// Set below the ground's 10 s bound with room for the burst itself to finish.
const SEND_SLOT_WAIT_LIMIT: Duration = Duration::from_secs(6);

/// Handle one RPC request: forward to the local HTTP API and send the
/// response back over the aux egress, fragmented if it does not fit one frame.
///
/// Runs as its own task so the uplink consumer's read loop never stalls behind
/// a slow HTTP call. A failure at any stage still sends a Response with a 5xx
/// status back to the ground, so the caller's proxy times out gracefully
/// rather than waiting for the full RPC_TIMEOUT.
///
/// The ground retransmits a Request it has no answer for, so on a lossy uplink
/// the same id arrives here more than once. `dedupe` is what keeps that safe: a
/// duplicate of a running call is dropped and the original answers both, and a
/// duplicate of a finished one re-sends the cached fragments without re-running
/// the HTTP call, so a retried `PUT` can never write twice.
///
/// `own_device_id` is stamped on every fragment. A fleet shares one radio key,
/// so a broadcast request id can be answered by more than one aircraft, and the
/// ground needs to know which one this is before it feeds the symbols to a
/// decoder that cannot tell two bodies apart.
pub async fn handle(
    request: &RpcRequest<'_>,
    egress: &AuxEgress,
    dedupe: &RequestDedupe,
    counters: &AuxUplinkConsumerCounters,
    own_device_id: &str,
) {
    // The caller's clock starts HERE, not when the fragments are ready. The
    // bound below used to be measured from the start of the send, so a slow
    // HTTP call spent the caller's budget for free: a 5 s call plus a 5 s
    // queue wait is past the ground's bound, and the burst transmitted anyway.
    let started = Instant::now();
    let id = request.id;

    // Authorise BEFORE the request reaches loopback.
    //
    // Everything downstream treats a relayed call as on-box, because it
    // genuinely does arrive on 127.0.0.1 -- so by the time `ados-control` sees
    // it, the decision has already been made. This is the only place the check
    // can happen while it still means anything.
    //
    // No secret on file denies. The window in which an unprovisioned aircraft
    // served radio-range callers with its full authority is closed; the one
    // path still served is the credential delivery itself, so the aircraft can
    // still be provisioned.
    let held = ados_protocol::relay_ticket::load_secret_at(std::path::Path::new(
        ados_protocol::relay_ticket::RELAY_SECRET_PATH,
    ));
    if held.is_none() {
        note_relay_secret_absent();
    }
    if let Err(e) = authorize(
        held.as_deref(),
        request.ticket,
        request.method,
        request.path,
        own_device_id,
        now_unix_secs(),
    ) {
        counters.note_rpc_unauthorized();
        tracing::warn!(
            request_id = id,
            error = %e,
            "aux_rpc_request_unauthorized"
        );
        // Answered rather than dropped: a ground station presenting a bad
        // credential should see it said so, not sit through a call timeout
        // that reads identically to a dead radio.
        let fragments = encode_fragments(own_device_id.as_bytes(), id, 401, &[], &[]);
        if !fragments.is_empty() {
            send_fragments(id, egress, &fragments, counters, started).await;
        }
        return;
    }

    let fragments = match dedupe.admit(id) {
        Admit::Duplicate => {
            counters.note_rpc_duplicate();
            tracing::debug!(request_id = id, "aux_rpc_request_duplicate_dropped");
            return;
        }
        Admit::Replay(cached) => {
            counters.note_rpc_replayed();
            tracing::debug!(
                request_id = id,
                fragments = cached.len(),
                "aux_rpc_response_replayed"
            );
            cached
        }
        Admit::Fresh => {
            let (status, headers, body) =
                match http_call(request.method, request.path, request.body).await {
                    Ok(v) => v,
                    Err(e) => {
                        let status = e.status();
                        tracing::warn!(error = %e, request_id = id, status, "aux_rpc_http_call_failed");
                        (status, Vec::new(), Vec::new())
                    }
                };
            let fragments =
                encode_fragments(own_device_id.as_bytes(), id, status, &headers, &body);
            if fragments.is_empty() {
                // Nothing encodable to cache or to send. Reopening the id lets
                // the ground's next retransmit make a real attempt instead of
                // being dropped as a duplicate of a call that answered nothing.
                dedupe.abandon(id);
                return;
            }
            // Cache before sending, so a retransmit that overtakes the send
            // still replays. An error status that produced fragments is cached
            // like any other answer: a retry must see the same 503 rather than
            // make a second attempt at a call that may have had a side effect.
            dedupe.complete(id, &fragments);
            fragments
        }
    };

    send_fragments(id, egress, &fragments, counters, started).await;
}

/// Emit one response's fragments as a single paced burst.
///
/// Holds [`RESPONSE_SEND_SLOT`] for the whole burst, so concurrent responses
/// queue behind one another instead of interleaving at N x the paced rate.
///
/// The wait for that slot is bounded. Beyond the bound the ground has given up
/// on this call, and transmitting anyway would spend uplink airtime on an
/// answer nobody is waiting for while this side logged a success. The ground's
/// retransmission plus the dedupe cache is what makes abandoning safe.
async fn send_fragments(
    id: u32,
    egress: &AuxEgress,
    fragments: &[Vec<u8>],
    counters: &AuxUplinkConsumerCounters,
    started: Instant,
) {
    // `acquire` fails only on a closed semaphore and nothing closes a static
    // one; `.ok()` holds the permit for this scope with no panic path.
    // Whatever is left of the caller's budget, not a fresh full allowance.
    let remaining = SEND_SLOT_WAIT_LIMIT.saturating_sub(started.elapsed());
    let slot = tokio::time::timeout(remaining, RESPONSE_SEND_SLOT.acquire()).await;
    let _slot = match slot {
        Ok(permit) => permit.ok(),
        Err(_) => {
            counters.note_rpc_response_abandoned();
            tracing::warn!(
                request_id = id,
                fragments = fragments.len(),
                elapsed_ms = started.elapsed().as_millis() as u64,
                "aux_rpc_response_abandoned_send_queue_too_deep"
            );
            return;
        }
    };
    let last = fragments.len().saturating_sub(1);
    for (index, payload) in fragments.iter().enumerate() {
        if let Err(e) = egress.send(AuxChannel::Response, payload).await {
            tracing::warn!(error = %e, request_id = id, index, "aux_rpc_response_send_failed");
            return;
        }
        if index < last {
            tokio::time::sleep(FRAGMENT_PACING).await;
        }
    }
}

/// Encode a response body plus its allow-listed headers as the aux payloads
/// that will carry them.
///
/// The headers are folded into the encoded OBJECT by
/// [`aux_rpc::pack_response`], not into each fragment, so they cost one
/// trailer per response rather than one copy per symbol and the fragment
/// geometry is untouched. With no headers the object is the body verbatim and
/// the emitted bytes are identical to a build without header support, which is
/// what keeps a mixed-version fleet's JSON reads working.
///
/// A body past the ground station's reassembly ceiling becomes a single 413
/// answer: truncating is wrong, because the ground would reassemble garbage
/// and report it as the drone's answer. The ceiling is checked against the
/// packed object, since the trailer is bytes on the air too.
fn encode_fragments(
    sender: &[u8],
    id: u32,
    status: u16,
    headers: &[aux_rpc::ResponseHeader],
    body: &[u8],
) -> Vec<Vec<u8>> {
    const OVERSIZED: &[u8] = b"response exceeds the relay reassembly ceiling";
    let (wire_status, object) = aux_rpc::pack_response(status, body, headers);
    let (status, symbols) = match aux_rpc::split_response(&object) {
        Some(s) => (wire_status, s),
        None => {
            tracing::warn!(
                len = object.len(),
                request_id = id,
                "aux_rpc_response_too_large"
            );
            // The refusal is 45 bytes, so this arm always encodes; the fallback
            // exists only because `split_response` is fallible in principle.
            // It carries no headers, so no flag rides with it.
            match aux_rpc::split_response(OVERSIZED) {
                Some(s) => (413u16, s),
                None => return Vec::new(),
            }
        }
    };

    encode_all(sender, id, status, &symbols)
}

/// Encode every symbol, or none of them.
///
/// A fragment that failed to encode used to be skipped, which sent a short
/// response whose surviving fragments still advertise the original `total`: the
/// ground then waits out its entire call bound for a fragment that is never
/// coming. Sending nothing instead lets its retransmit try again immediately.
fn encode_all(sender: &[u8], id: u32, status: u16, symbols: &ResponseSymbols) -> Vec<Vec<u8>> {
    let total = symbols.symbols.len() as u16;
    let mut out = Vec::with_capacity(symbols.symbols.len());
    for (index, symbol) in symbols.symbols.iter().enumerate() {
        match aux_rpc::encode_response_fragment(
            sender,
            id,
            status,
            index as u16,
            total,
            symbols.oti,
            symbol,
        ) {
            Some(payload) => out.push(payload),
            None => {
                tracing::error!(
                    request_id = id,
                    index,
                    total,
                    len = symbol.len(),
                    "aux_rpc_fragment_encode_failed"
                );
                return Vec::new();
            }
        }
    }
    out
}

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
async fn read_response_bounded<R: tokio::io::AsyncRead + Unpin>(
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
async fn http_call(
    method: RpcMethod,
    path: &[u8],
    body: &[u8],
) -> Result<(u16, Vec<aux_rpc::ResponseHeader>, Vec<u8>), HttpError> {
    let path_str = std::str::from_utf8(path).map_err(|_| HttpError::BadPath)?;
    if !path_is_safe(path_str) {
        return Err(HttpError::BadPath);
    }
    let method_str = method.as_http_method();

    let work = async {
        let mut stream = TcpStream::connect((HTTP_HOST, HTTP_PORT))
            .await
            .map_err(|e| HttpError::Connect(e.to_string()))?;

        let request = build_request_head(method_str, path_str, body.len());
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

        let response = read_response_bounded(&mut stream).await?;

        parse_response(&response)
    };

    tokio::time::timeout(HTTP_TIMEOUT, work)
        .await
        .map_err(|_| HttpError::Timeout)?
}

/// Build the HTTP/1.1 request line and headers.
///
/// The wire frame carries no headers, so a body's content-type is synthesised
/// rather than forwarded: the lane only ever carries JSON, and every GCS write
/// already sets `Content-Type: application/json` on its own transport. The
/// Rust front's axum `Json<T>` extractors reject a body with no
/// `application/json` content-type before the handler runs, so a relayed write
/// would 415 without this.
///
/// `X-ADOS-Relayed: 1` marks the call as having crossed the radio. It is for
/// logging and attribution ONLY, and is deliberately NOT in
/// `pairing_posture::FORWARDED_HEADERS`: a header in that set flips the request
/// out of the on-box trust posture it reaches the local API with, which would
/// put every relayed call behind `X-ADOS-Key`, pairing, and the rate limiter and
/// break the lane outright. The trust boundary here is the WFB pairing plus the
/// key-gated ground route, not this header — do not read it as an auth control.
fn build_request_head(method: &str, path: &str, body_len: usize) -> String {
    let content_type = if body_len == 0 {
        ""
    } else {
        "Content-Type: application/json\r\n"
    };
    format!(
        "{method} {path} HTTP/1.1\r\nHost: localhost\r\nX-ADOS-Relayed: 1\r\n{content_type}Content-Length: {body_len}\r\nConnection: close\r\n\r\n"
    )
}

/// Whether a relayed path may be interpolated into an HTTP request line.
///
/// axum percent-decodes the ground route's wildcard capture, so `%0D%0A` in the
/// caller's URL arrives here as a literal CRLF that would end the request line
/// and let the caller inject arbitrary headers into the request the drone makes
/// to its own API. The ground rejects this too; the drone re-checks because the
/// radio is the trust boundary, and this side must not depend on the other
/// side's validation.
fn path_is_safe(path: &str) -> bool {
    !path.bytes().any(|b| b < 0x20 || b == 0x7F)
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
const FORWARDED_RESPONSE_HEADERS: &[&str] = &[
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
fn parse_response(raw: &[u8]) -> Result<(u16, Vec<aux_rpc::ResponseHeader>, Vec<u8>), HttpError> {
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
enum HttpError {
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
    fn status(&self) -> u16 {
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The measured `/api/services` size on a live drone — the endpoint whose
    /// 413 was the reason fragmentation exists.
    const MEASURED_SERVICES_BYTES: usize = 2631;

    /// This drone's device id, stamped on every fragment it emits.
    const OWN_ID: &[u8] = b"77735cd38937";

    /// Rebuild a response from the fragments the handler produced, the way the
    /// ground station does — including splitting the encoded object back into
    /// its status, body and headers.
    fn reassemble_full(
        frags: &[Vec<u8>],
        skip: &[usize],
    ) -> Option<(u16, Vec<aux_rpc::ResponseHeader>, Vec<u8>)> {
        let mut decoder: Option<aux_rpc::ResponseDecoder> = None;
        for (i, payload) in frags.iter().enumerate() {
            if skip.contains(&i) {
                continue;
            }
            let dec = aux_rpc::decode_response(payload).unwrap();
            assert_eq!(
                dec.sender, OWN_ID,
                "every fragment names the answering drone"
            );
            assert_eq!(dec.index, i as u16);
            assert_eq!(dec.total, frags.len() as u16);
            let d = decoder.get_or_insert_with(|| aux_rpc::ResponseDecoder::new(dec.oti).unwrap());
            if let aux_rpc::FragmentOutcome::Complete(object) = d.push(dec.index, dec.body) {
                let (status, body, headers) = aux_rpc::unpack_response(dec.status, object);
                return Some((status, headers, body));
            }
        }
        None
    }

    /// The common case: status + body, with the headers asserted separately by
    /// the tests that care about them.
    fn reassemble(frags: &[Vec<u8>], skip: &[usize]) -> Option<(u16, Vec<u8>)> {
        reassemble_full(frags, skip).map(|(status, _, body)| (status, body))
    }

    /// `start_paused` lets the bounded wait expire instantly instead of the
    /// test sitting through the real limit.

    #[tokio::test]
    async fn a_response_larger_than_the_ceiling_stops_being_read_at_the_ceiling() {
        // The read used to run to EOF, bounded only by a 5 s timeout against
        // loopback, and the size check happened after the body was fully
        // buffered. With one task per relay request, N concurrent calls to a
        // large-response route each held their own copy.
        let (mut near, far) = tokio::io::duplex(64 * 1024);
        let ceiling = ados_protocol::aux_rpc::MAX_RESPONSE_BODY;
        // Far more than the ceiling, written for as long as anyone reads.
        tokio::spawn(async move {
            let chunk = vec![b'x'; 32 * 1024];
            loop {
                if near.write_all(&chunk).await.is_err() {
                    break;
                }
            }
        });

        let got = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            read_response_bounded(far),
        )
        .await
        .expect("the read never terminated; it is still bounded only by a timeout")
        .expect("read failed");

        assert!(
            got.len() <= ceiling + HTTP_HEADER_ALLOWANCE + 1,
            "buffered {} bytes for a ceiling of {}",
            got.len(),
            ceiling
        );
    }

    #[tokio::test]
    async fn a_response_under_the_ceiling_is_read_whole() {
        // The bound must not truncate the ordinary case.
        let (mut near, far) = tokio::io::duplex(64 * 1024);
        let body = b"HTTP/1.1 200 OK\r\n\r\n{\"ok\":true}".to_vec();
        let expected = body.clone();
        tokio::spawn(async move {
            let _ = near.write_all(&body).await;
        });
        let got = read_response_bounded(far).await.expect("read failed");
        assert_eq!(got, expected);
    }

    #[tokio::test(start_paused = true)]
    async fn a_response_is_abandoned_rather_than_sent_after_the_caller_gave_up() {
        // The send slot is process-wide and held for a whole burst, so
        // concurrent responses queue linearly. That wait used to count against
        // nothing: a response could sit past the ground's call bound and
        // transmit anyway, spending uplink airtime on an answer nobody was
        // waiting for while this side recorded a success.
        let counters = AuxUplinkConsumerCounters::new();
        let sock = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let target = sock.local_addr().unwrap();
        sock.connect(target).await.unwrap();
        let egress = AuxEgress::connected_for_test(sock);
        let fragments = encode_fragments(OWN_ID, 1, 200, &[], br#"{"ok":true}"#);

        // Hold the slot for longer than a caller would wait.
        let held = RESPONSE_SEND_SLOT.acquire().await.expect("slot");
        send_fragments(1, &egress, &fragments, &counters, Instant::now()).await;
        drop(held);

        assert_eq!(
            counters.snapshot().rpc_response_abandoned,
            1,
            "a response that waited past the caller's bound must be abandoned and counted"
        );
    }

    #[test]
    fn the_send_slot_wait_stays_inside_the_grounds_call_bound() {
        // Waiting longer than the caller does turns a queued response into
        // wasted airtime: the ground has already timed out and retransmitted.
        assert!(
            SEND_SLOT_WAIT_LIMIT < ados_protocol::aux_rpc_proxy::RPC_DEFAULT_TIMEOUT,
            "the send-slot wait must expire before the ground gives up"
        );
        // And it must leave room for the burst itself to finish afterwards.
        assert!(
            SEND_SLOT_WAIT_LIMIT + Duration::from_millis(500)
                < ados_protocol::aux_rpc_proxy::RPC_DEFAULT_TIMEOUT,
            "the wait must leave the burst time to transmit"
        );
    }

    #[test]
    fn a_small_body_travels_as_one_symbol_plus_its_repair_set() {
        let frags = encode_fragments(OWN_ID, 7, 200, &[], br#"{"ok":true}"#);
        assert_eq!(frags.len(), 1 + aux_rpc::RPC_REPAIR_SYMBOLS as usize);
        assert_eq!(
            reassemble(&frags, &[]),
            Some((200, br#"{"ok":true}"#.to_vec()))
        );
    }

    #[test]
    fn a_services_sized_body_fragments_and_reassembles() {
        let body: Vec<u8> = (0..MEASURED_SERVICES_BYTES)
            .map(|i| (i % 251) as u8)
            .collect();
        let frags = encode_fragments(OWN_ID, 7, 200, &[], &body);
        let systematic = MEASURED_SERVICES_BYTES.div_ceil(aux_rpc::MAX_RESPONSE_FRAGMENT);
        assert_eq!(
            frags.len(),
            systematic + aux_rpc::RPC_REPAIR_SYMBOLS as usize
        );
        assert_eq!(reassemble(&frags, &[]), Some((200, body.clone())));
        // The repair symbols are the point: losing any two still answers.
        assert_eq!(reassemble(&frags, &[0, 2]), Some((200, body)));
    }

    #[test]
    fn an_empty_body_still_travels_as_one_fragment() {
        let frags = encode_fragments(OWN_ID, 7, 204, &[], b"");
        assert_eq!(frags.len(), 1, "a 204 has no symbols to protect");
        let dec = aux_rpc::decode_response(&frags[0]).unwrap();
        assert_eq!(dec.status, 204);
        assert_eq!(dec.sender, OWN_ID);
        assert!(dec.body.is_empty());
        assert_eq!(reassemble(&frags, &[]), Some((204, Vec::new())));
    }

    #[test]
    fn a_body_past_the_ceiling_becomes_a_413() {
        let body = vec![b'x'; 90_000];
        let frags = encode_fragments(OWN_ID, 7, 200, &[], &body);
        assert_eq!(
            reassemble(&frags, &[]),
            Some((
                413,
                b"response exceeds the relay reassembly ceiling".to_vec()
            )),
            "never truncate a body into a fake 200"
        );
    }

    #[test]
    fn a_request_with_a_body_declares_json_and_one_without_does_not() {
        let write = build_request_head("PUT", "/api/config", 40);
        assert!(
            write.contains("Content-Type: application/json\r\n"),
            "axum Json<T> rejects a body with no content-type before the handler runs"
        );
        assert!(write.starts_with("PUT /api/config HTTP/1.1\r\n"));
        assert!(write.contains("Content-Length: 40\r\n"));

        let read = build_request_head("GET", "/api/logs?limit=5", 0);
        assert!(!read.contains("Content-Type"));
        assert!(read.starts_with("GET /api/logs?limit=5 HTTP/1.1\r\n"));
    }

    #[test]
    fn the_allow_listed_response_headers_are_collected_and_the_rest_are_not() {
        let raw = b"HTTP/1.1 200 OK\r\n\
Content-Type: text/csv; charset=utf-8\r\n\
Content-Disposition: attachment; filename=\"log.csv\"\r\n\
Location: /whep/session/abc\r\n\
Cache-Control: no-store\r\n\
Server: hyper\r\n\
Set-Cookie: sid=secret\r\n\
Content-Length: 3\r\n\r\nabc";
        let (status, headers, body) = parse_response(raw).unwrap();
        assert_eq!(status, 200);
        assert_eq!(body, b"abc");
        assert_eq!(
            headers,
            vec![
                ("content-type".to_string(), "text/csv; charset=utf-8".to_string()),
                (
                    "content-disposition".to_string(),
                    "attachment; filename=\"log.csv\"".to_string()
                ),
                ("location".to_string(), "/whep/session/abc".to_string()),
                ("cache-control".to_string(), "no-store".to_string()),
            ],
            "only the allow-list crosses, lowercased, in wire order"
        );
        // Every byte of header is a byte of body that cannot travel, and a
        // cookie is a credential with no business on a shared radio.
        assert!(headers.iter().all(|(n, _)| n != "set-cookie" && n != "server"));
        // Rebuilt by the ground, never forwarded: the body it reassembles is
        // the one whose length counts.
        assert!(headers.iter().all(|(n, _)| n != "content-length"));
    }

    #[test]
    fn a_content_type_reaches_the_ground_through_the_fragment_codec() {
        // The end-to-end property GA-01 is about: an artifact body that leaves
        // the drone typed arrives at the ground still typed.
        let body: Vec<u8> = (0..5000u32).map(|i| (i % 251) as u8).collect();
        let headers = vec![(
            "content-type".to_string(),
            "application/octet-stream".to_string(),
        )];
        let frags = encode_fragments(OWN_ID, 7, 200, &headers, &body);
        let (status, got_headers, got_body) =
            reassemble_full(&frags, &[1]).expect("repair symbols cover one loss");
        assert_eq!(status, 200, "the header flag never leaks into the status");
        assert_eq!(got_body, body);
        assert_eq!(got_headers, headers);
    }

    #[test]
    fn a_headerless_answer_puts_the_same_bytes_on_the_air_as_before() {
        // The mixed-fleet guarantee: a drone on this build answering a relayed
        // JSON read emits exactly what a drone without header support emits,
        // so a ground station that has not upgraded keeps working.
        let body = br#"{"ok":true}"#;
        let with_support = encode_fragments(OWN_ID, 7, 200, &[], body);
        let dec = aux_rpc::decode_response(&with_support[0]).unwrap();
        assert_eq!(
            dec.status & aux_rpc::RESPONSE_HEADERS_FLAG,
            0,
            "no flag set when nothing is carried"
        );
        assert_eq!(reassemble(&with_support, &[]), Some((200, body.to_vec())));
    }

    #[test]
    fn parses_a_200_response_with_a_body() {
        let raw =
            b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 5\r\n\r\nhello";
        let (status, _headers, body) = parse_response(raw).unwrap();
        assert_eq!(status, 200);
        assert_eq!(body, b"hello");
    }

    #[test]
    fn parses_a_404_response_with_an_empty_body() {
        let raw = b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n";
        let (status, _headers, body) = parse_response(raw).unwrap();
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
        let (status, _headers, parsed_body) = parse_response(raw.as_bytes()).unwrap();
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
        let (status, _headers, body) = parse_response(raw).unwrap();
        assert_eq!(status, 200);
        assert_eq!(body, b"a\r\n\r\nb");
    }

    #[test]
    fn the_relayed_marker_rides_every_request_head() {
        let head = build_request_head("GET", "/api/version", 0);
        assert!(
            head.contains("X-ADOS-Relayed: 1\r\n"),
            "attribution only: the key-gated ground route is the auth control, not this header"
        );
    }

    #[test]
    fn a_path_carrying_control_characters_is_refused() {
        assert!(path_is_safe("/api/logs?limit=5&since=2026-01-01T00:00:00Z"));
        assert!(
            !path_is_safe("/api/version\r\nX-Injected: 1"),
            "a decoded %0D%0A would end the request line and inject a header"
        );
        assert!(!path_is_safe("/api/version\nX-Injected: 1"));
        assert!(!path_is_safe("/api/version\u{7f}"));
    }

    #[test]
    fn a_chunked_response_is_refused_as_a_bad_gateway() {
        let raw = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n0\r\n\r\n";
        let err = parse_response(raw).unwrap_err();
        assert!(matches!(err, HttpError::Chunked));
        assert_eq!(
            err.status(),
            502,
            "chunk framing forwarded as a body reaches the GCS as unparseable JSON"
        );
        assert_eq!(
            HttpError::Timeout.status(),
            503,
            "a wedged local API stays a 503"
        );
    }

    #[test]
    fn a_declared_content_length_bounds_the_body() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello-and-then-some";
        let (status, _headers, body) = parse_response(raw).unwrap();
        assert_eq!(status, 200);
        assert_eq!(
            body, b"hello",
            "bytes past the declared length are not part of this body"
        );

        let short = b"HTTP/1.1 200 OK\r\nContent-Length: 99\r\n\r\nhello";
        assert_eq!(
            parse_response(short).unwrap().2,
            b"hello",
            "a server that closed early still yields what it did send"
        );
    }

    #[test]
    fn one_unencodable_fragment_cancels_the_whole_response() {
        // MAX_RESPONSE_FRAGMENT budgets a worst-case device id, so a symbol one
        // byte past it is the first that cannot be framed for a sender whose id
        // is actually that long. A shorter id leaves slack and would still fit.
        let worst_case_sender = vec![b'a'; ados_protocol::node_status::MAX_DEVICE_ID];
        let oversized = aux_rpc::ResponseSymbols {
            oti: 8,
            symbols: vec![
                b"first".to_vec(),
                vec![0u8; aux_rpc::MAX_RESPONSE_FRAGMENT + 1],
            ],
        };
        assert_eq!(
            encode_all(&worst_case_sender, 7, 200, &oversized),
            Vec::<Vec<u8>>::new(),
            "a short send whose total still says 2 makes the ground wait out its call bound"
        );
        let fits = aux_rpc::ResponseSymbols {
            oti: 11,
            symbols: vec![b"first!".to_vec(), b"second".to_vec()],
        };
        assert_eq!(
            encode_all(OWN_ID, 7, 200, &fits).len(),
            2,
            "the all-or-nothing guard must not reject an encodable response"
        );
    }

    mod relay_authorization {
        use super::super::authorize;
        use ados_protocol::aux_rpc::RpcMethod;
        use ados_protocol::relay_ticket::{
            RelayTicketError, RelayTicketIssuer, DEFAULT_TTL_SECONDS,
        };

        const SECRET: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        const ME: &str = "77735cd38937";
        const NOW: i64 = 1_800_000_000;
        /// A path with no special standing, used wherever the decision must not
        /// depend on which route was called.
        const ORDINARY: &[u8] = b"/api/config";
        const BOOTSTRAP: &[u8] = b"/api/relay/peer-secret";

        fn ticket_for(target: &str) -> String {
            RelayTicketIssuer::from_secret(SECRET.as_bytes()).mint_at(
                target,
                DEFAULT_TTL_SECONDS,
                NOW,
            )
        }

        #[test]
        fn a_drone_with_no_secret_refuses_every_relayed_call() {
            // The close. A relayed request lands on loopback and is treated as
            // on-box, so an aircraft with no credential to check must refuse
            // rather than hand radio range its full authority.
            for method in [
                RpcMethod::Get,
                RpcMethod::Post,
                RpcMethod::Put,
                RpcMethod::Delete,
            ] {
                assert_eq!(
                    authorize(None, b"", method, ORDINARY, ME, NOW),
                    Err(RelayTicketError::NoSecret)
                );
                assert_eq!(
                    authorize(None, b"any old rubbish", method, ORDINARY, ME, NOW),
                    Err(RelayTicketError::NoSecret)
                );
            }
        }

        #[test]
        fn a_ticket_cannot_open_a_drone_that_holds_no_secret() {
            // There is nothing to verify against, so a well-formed ticket is
            // worth no more than an absent one.
            let t = ticket_for(ME);
            assert_eq!(
                authorize(None, t.as_bytes(), RpcMethod::Put, ORDINARY, ME, NOW),
                Err(RelayTicketError::NoSecret)
            );
        }

        #[test]
        fn the_secret_delivery_stays_reachable_before_a_secret_exists() {
            // The one carve-out, and the reason denying everything else does
            // not brick provisioning: the delivery cannot carry a ticket the
            // drone could check, and first-write-wins bounds it.
            assert!(authorize(None, b"", RpcMethod::Post, BOOTSTRAP, ME, NOW).is_ok());
            assert!(authorize(
                None,
                b"",
                RpcMethod::Post,
                b"/api/relay/peer-secret?whatever=1",
                ME,
                NOW
            )
            .is_ok());
        }

        #[test]
        fn the_carve_out_is_that_one_method_and_that_one_path() {
            // A read of the delivery route, a near-miss path and a prefix
            // extension all stay refused, so the exception cannot be widened
            // into a general bypass.
            for (method, path) in [
                (RpcMethod::Get, BOOTSTRAP),
                (RpcMethod::Put, BOOTSTRAP),
                (RpcMethod::Delete, BOOTSTRAP),
                (
                    RpcMethod::Post,
                    b"/api/relay/peer-secret/../config" as &[u8],
                ),
                (RpcMethod::Post, b"/api/relay/peer-secretx"),
                (RpcMethod::Post, b"/api/relay/peer-secre"),
            ] {
                assert_eq!(
                    authorize(None, b"", method, path, ME, NOW),
                    Err(RelayTicketError::NoSecret),
                    "{method:?} {}",
                    String::from_utf8_lossy(path)
                );
            }
        }

        #[test]
        fn a_valid_ticket_is_admitted() {
            let t = ticket_for(ME);
            assert!(authorize(
                Some(SECRET),
                t.as_bytes(),
                RpcMethod::Put,
                ORDINARY,
                ME,
                NOW
            )
            .is_ok());
        }

        #[test]
        fn a_drone_holding_a_secret_refuses_a_call_carrying_none() {
            // Once the credential exists, an unaccompanied relayed request no
            // longer inherits on-box authority -- including on the delivery
            // route, which a re-key attempt would have to use.
            assert!(authorize(Some(SECRET), b"", RpcMethod::Put, ORDINARY, ME, NOW).is_err());
            assert!(authorize(Some(SECRET), b"", RpcMethod::Post, BOOTSTRAP, ME, NOW).is_err());
        }

        #[test]
        fn a_ticket_minted_for_another_drone_is_refused() {
            // Load-bearing because the uplink is a broadcast: every drone in
            // the fleet hears every ticket, so one addressed elsewhere must not
            // open this one.
            let t = ticket_for("some-other-drone");
            assert!(authorize(
                Some(SECRET),
                t.as_bytes(),
                RpcMethod::Put,
                ORDINARY,
                ME,
                NOW
            )
            .is_err());
        }

        #[test]
        fn a_ticket_from_a_different_pair_secret_is_refused() {
            // A second ground station holding the shared fleet radio key still
            // cannot mint one this drone accepts.
            let other = "f".repeat(64);
            let t = RelayTicketIssuer::from_secret(other.as_bytes()).mint_at(
                ME,
                DEFAULT_TTL_SECONDS,
                NOW,
            );
            assert!(authorize(
                Some(SECRET),
                t.as_bytes(),
                RpcMethod::Put,
                ORDINARY,
                ME,
                NOW
            )
            .is_err());
        }

        #[test]
        fn an_expired_ticket_is_refused() {
            let t = ticket_for(ME);
            assert!(authorize(
                Some(SECRET),
                t.as_bytes(),
                RpcMethod::Put,
                ORDINARY,
                ME,
                NOW + DEFAULT_TTL_SECONDS + 1
            )
            .is_err());
        }

        #[test]
        fn a_ticket_that_is_not_utf8_is_refused_rather_than_panicking() {
            assert!(authorize(
                Some(SECRET),
                &[0xFF, 0xFE, 0xFD],
                RpcMethod::Put,
                ORDINARY,
                ME,
                NOW
            )
            .is_err());
        }
    }
}
