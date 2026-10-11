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
use ados_protocol::aux_rpc::{
    self, ReassemblyOutcome, RequestFragment, RequestReassembler, ResponseSymbols, RpcMethod,
    RpcRequest,
};
use ados_protocol::nonce_cache::NonceCache;
use ados_protocol::relay_ticket::{RelayTicketError, RequestBinding, VerifiedRelayTicket};
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
///
/// The ticket must be bound to this exact request (method, path with query,
/// body). `Ok(Some(ticket))` carries the nonce the caller spends once it knows
/// the request is not a retransmit; `Ok(None)` is the unticketed bootstrap.
pub fn authorize(
    held: Option<&str>,
    request: &RpcRequest,
    own_device_id: &str,
    now: i64,
) -> Result<Option<VerifiedRelayTicket>, RelayTicketError> {
    let Some(secret) = held else {
        if is_relay_bootstrap(request.method, &request.path) {
            return Ok(None);
        }
        return Err(RelayTicketError::NoSecret);
    };
    let issuer = ados_protocol::relay_ticket::RelayTicketIssuer::from_secret(secret.as_bytes());
    // A ticket or path that is not valid UTF-8 cannot be one we minted, and
    // reads as a malformed one rather than being allowed to panic a decode.
    let presented =
        std::str::from_utf8(&request.ticket).map_err(|_| RelayTicketError::Malformed)?;
    let path = std::str::from_utf8(&request.path).map_err(|_| RelayTicketError::Malformed)?;
    let binding = RequestBinding::new(request.method.as_http_method(), path, &request.body);
    issuer
        .verify(presented, own_device_id, &binding, now)
        .map(Some)
}

/// Every relay ticket this drone has accepted, so each is honoured once.
static SPENT_TICKETS: NonceCache = ados_protocol::relay_ticket::new_replay_cache();

/// Request fragments waiting for the rest of their request.
static REASSEMBLER: std::sync::LazyLock<std::sync::Mutex<RequestReassembler>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(RequestReassembler::new()));

/// Feed one request fragment to the drone's reassembler. `Some` once the
/// request it belongs to is whole.
pub fn accept_fragment(fragment: &RequestFragment<'_>) -> Option<RpcRequest> {
    let mut reassembler = REASSEMBLER.lock().unwrap_or_else(|p| p.into_inner());
    match reassembler.push(fragment, Instant::now()) {
        ReassemblyOutcome::Complete(request) => Some(request),
        ReassemblyOutcome::Pending | ReassemblyOutcome::Duplicate => None,
        ReassemblyOutcome::Rejected => {
            tracing::warn!(
                request_id = fragment.id,
                seq = fragment.seq,
                total = fragment.total,
                "aux_rpc_request_fragment_rejected"
            );
            None
        }
    }
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
    request: &RpcRequest,
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
    let now = now_unix_secs();

    // Authorise BEFORE the request reaches loopback.
    //
    // Everything downstream treats a relayed call as on-box, because it
    // genuinely does arrive on 127.0.0.1 -- so by the time `ados-control` sees
    // it, the decision has already been made. This is the only place the check
    // can happen while it still means anything.
    //
    // No secret on file denies. The one path still served is the credential
    // delivery itself, so the aircraft can still be provisioned.
    let held = ados_protocol::relay_ticket::load_secret_at(std::path::Path::new(
        ados_protocol::relay_ticket::RELAY_SECRET_PATH,
    ));
    if held.is_none() {
        note_relay_secret_absent();
    }
    let ticket = match authorize(held.as_deref(), request, own_device_id, now) {
        Ok(ticket) => ticket,
        Err(e) => {
            refuse(id, e, egress, counters, own_device_id, started).await;
            return;
        }
    };

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
            // Spend the ticket only now. A retransmit of the same request id
            // carries the same ticket and is answered from the dedupe cache
            // above; anything reaching here with a spent nonce is the same
            // ticket replayed under a new id, which must not run again.
            if let Some(ticket) = &ticket {
                if let Err(e) = ticket.spend(&SPENT_TICKETS, now) {
                    dedupe.abandon(id);
                    refuse(id, e, egress, counters, own_device_id, started).await;
                    return;
                }
            }
            let (status, headers, body) = match http_call(request).await {
                Ok(v) => v,
                Err(e) => {
                    let status = e.status();
                    tracing::warn!(error = %e, request_id = id, status, "aux_rpc_http_call_failed");
                    // Said in the body, so a caller can tell the drone's own
                    // API failing from the ground station's lane failing: both
                    // are a 5xx on the relay route.
                    let body = serde_json::to_vec(&serde_json::json!({
                        "error": "E_RELAY_PEER_API",
                        "detail": e.to_string(),
                    }))
                    .unwrap_or_default();
                    let headers =
                        vec![("content-type".to_string(), "application/json".to_string())];
                    (status, headers, body)
                }
            };
            let fragments = encode_fragments(own_device_id.as_bytes(), id, status, &headers, &body);
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

/// Answer a refused relayed request with 401 and a body that says why.
///
/// Answered rather than dropped: a ground station presenting a bad credential
/// should see it said so, not sit through a call timeout that reads
/// identically to a dead radio. The reason is what lets an operator tell a
/// drone whose clock is wrong from one that holds no secret.
async fn refuse(
    id: u32,
    error: RelayTicketError,
    egress: &AuxEgress,
    counters: &AuxUplinkConsumerCounters,
    own_device_id: &str,
    started: Instant,
) {
    counters.note_rpc_unauthorized();
    tracing::warn!(
        request_id = id,
        error = %error,
        reason = error.reason(),
        "aux_rpc_request_unauthorized"
    );
    let body = ados_protocol::relay_ticket::refusal_body(error.reason());
    let headers = vec![("content-type".to_string(), "application/json".to_string())];
    let fragments = encode_fragments(own_device_id.as_bytes(), id, 401, &headers, &body);
    if !fragments.is_empty() {
        send_fragments(id, egress, &fragments, counters, started).await;
    }
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

mod http;
use http::*;

#[cfg(test)]
mod tests;
