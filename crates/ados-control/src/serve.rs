//! The dual-listener serve loop: one axum `Router` on two edges.
//!
//! The same Router is bound on two edges, mirroring the logging store's read
//! surface:
//!
//! 1. **The trusted local Unix socket** (`0o660 root:ados-operator`, tmpfs). No
//!    auth, no rate limit: only root and members of the operator group can open
//!    it, every accept re-checks the peer's kernel credentials, and this path
//!    keeps working even if the LAN edge is gated. Plugins run outside the
//!    operator group. The GCS does not use it; the on-box CLI does.
//! 2. **A LAN TCP port.** The auth layer mirrors the agent's HTTP posture:
//!    unpaired ⇒ served by caller class (lifelines open, the operator LAN
//!    behind a dashboard PIN, remote callers refused), paired ⇒ `X-ADOS-Key`
//!    required, with on-box loopback trust and a per-caller rate limit
//!    guarding the edge. Connections are capped in total and per caller, and a
//!    connection that does not deliver a request head in time is closed.
//!
//! The one difference from the logd listener is the caller: the LAN edge
//! threads the accepted connection's [`SocketAddr`] into the request, and the
//! edge middleware classifies it once per request into a
//! [`CallerClass`] (peer address plus forwarding headers) that every gate and
//! handler then reads as a request extension. The Unix edge stamps
//! [`CallerClass::OnBox`] — its trust is the socket's group and the per-accept
//! peer check, and it never installs the auth layer.

use std::collections::HashMap;
use std::convert::Infallible;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::middleware::{self, Next};
use axum::response::Response;
use axum::Router;
use hyper::body::Incoming;
use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};
use hyper_util::server::conn::auto::Builder as ConnBuilder;
use socket2::{Domain, Protocol, Socket, Type};
use tokio::net::TcpListener;
use tokio::sync::{oneshot, OwnedSemaphorePermit, Semaphore};
use tower::{Service, ServiceBuilder};
use tower_http::cors::{AllowHeaders, AllowMethods, AllowOrigin, CorsLayer};

use crate::auth::{self, Pairing, PairingState, PeerKey, RateLimiter};
use crate::config::{ControlSecurityConfig, PairingConfig};
use crate::mcp::{route_scope, McpTokenStore, MCP_SCOPES_HEADER, MCP_TOKEN_HEADER};
use crate::proxy_auth::{BodyField, Decision, ProxiedAuth, RequestHeaders};
use crate::routes::detail;
use ados_protocol::ipc::OperatorListener;
use ados_protocol::mcp_token::scope_allows_class;
use ados_protocol::pairing_posture::{classify_caller, CallerClass};
use ados_protocol::ws_ticket::{now_unix, WsTicketIssuer};

/// The header the front stamps on a request that passes its on-box loopback
/// check, so the residual Python (which does not see the TCP peer) can honour the
/// same on-box trust the native edge applies. It is STRIPPED from every inbound
/// request first, then set only when the edge classified the caller
/// [`CallerClass::OnBox`], so a value arriving from off-box can never be spoofed
/// in. See [`tcp_edge`]. Native handlers read the [`CallerClass`] request
/// extension instead.
pub const ONBOX_HEADER: &str = "x-ados-onbox";

/// The peer address of the accepted TCP connection, attached to each LAN-edge
/// request as an extension so [`tcp_edge`] can classify the caller. Absent on
/// the Unix edge.
///
/// It cannot be forged: the accept loop inserts it from the real socket, and
/// nothing reads it from a header.
#[derive(Clone, Copy, Debug)]
pub struct PeerAddr(pub SocketAddr);

/// The local address the accepted TCP connection arrived on, attached next to
/// [`PeerAddr`]. A private-LAN peer is a first-boot lifeline only when this is
/// the agent's own AP or USB-gadget address; a LAN that merely shares that
/// numbering is not.
#[derive(Clone, Copy, Debug)]
pub struct LocalAddr(pub SocketAddr);

/// Which listener a connection arrived on, so [`serve_conn`] stamps the right
/// caller evidence on every request it carries.
#[derive(Clone, Copy, Debug)]
enum ConnEdge {
    /// The operator Unix socket: only root and operator-group peers reach it,
    /// so every request on it is [`CallerClass::OnBox`].
    Unix,
    /// The LAN TCP front: the peer and local addresses are stamped and
    /// [`tcp_edge`] classifies the caller per request (it needs the request's
    /// headers).
    Tcp {
        peer: SocketAddr,
        local: Option<SocketAddr>,
    },
}

/// Concurrent connections the TCP front holds in total. The unit runs under a
/// small memory ceiling and the default descriptor limit; past this a new
/// connection is closed on accept.
const MAX_TCP_CONNECTIONS: usize = 512;

/// Concurrent connections one caller may hold. A GCS, a dashboard and a
/// cockpit on one host, each with a few streams open, fit well under it.
const MAX_TCP_CONNECTIONS_PER_PEER: usize = 32;

/// How long a connection may take to deliver a request head, including the
/// idle wait for the next request on a kept-alive connection.
const HEADER_READ_TIMEOUT: Duration = Duration::from_secs(15);

/// The largest request body the HMAC gate buffers to verify. The signed
/// mutations are JSON commands and config writes, far below this.
const HMAC_BODY_LIMIT: usize = 1024 * 1024;

/// The largest relayed `PUT /api/config` body the edge reads to check its key.
const RELAY_CONFIG_BODY_LIMIT: usize = 64 * 1024;

/// Per-caller connection accounting for the TCP front.
#[derive(Clone, Default)]
struct ConnCounts(Arc<Mutex<HashMap<PeerKey, usize>>>);

/// Holds one connection's slot: a share of the total, plus (for an off-box
/// caller) one of that caller's slots. Released on drop.
struct ConnSlot {
    counted: Option<(ConnCounts, PeerKey)>,
    _permit: OwnedSemaphorePermit,
}

impl ConnCounts {
    /// Take a slot for `key` if both the total and the per-caller caps allow.
    /// `None` for `key` takes a share of the total only.
    fn acquire(&self, key: Option<PeerKey>, total: &Arc<Semaphore>) -> Option<ConnSlot> {
        let permit = total.clone().try_acquire_owned().ok()?;
        let Some(key) = key else {
            return Some(ConnSlot {
                counted: None,
                _permit: permit,
            });
        };
        let mut counts = self.0.lock().unwrap_or_else(|p| p.into_inner());
        let held = counts.entry(key).or_insert(0);
        if *held >= MAX_TCP_CONNECTIONS_PER_PEER {
            return None;
        }
        *held += 1;
        Some(ConnSlot {
            counted: Some((self.clone(), key)),
            _permit: permit,
        })
    }
}

impl Drop for ConnSlot {
    fn drop(&mut self) {
        let Some((counts, key)) = &self.counted else {
            return;
        };
        let mut counts = counts.0.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(held) = counts.get_mut(key) {
            *held = held.saturating_sub(1);
            if *held == 0 {
                counts.remove(key);
            }
        }
    }
}

/// Per-edge auth state attached to the TCP layer. The Unix listener does not
/// install the layer at all, so on-box callers are never gated.
#[derive(Clone)]
struct EdgeAuth {
    pairing: Arc<PairingState>,
    rate: Arc<RateLimiter>,
    /// The proxied-route auth decision, run on every forwarded request before
    /// it reaches the residual surface (the front is the single authenticator).
    proxied: Arc<ProxiedAuth>,
    /// The dashboard-access PIN store: consulted only on a would-be-401 to accept
    /// a valid dashboard session token as an alternative data-plane credential.
    dashboard_pin: Arc<crate::dashboard_pin::DashboardPin>,
    /// The MCP-token store: consulted only on a would-be-401 for a NATIVE route,
    /// and only when the accept flag is on, to admit a scoped MCP token as a
    /// last-resort credential (with per-route scope enforcement).
    mcp_tokens: Arc<McpTokenStore>,
    /// The agent config path the edge reads the MCP accept flag + this node's
    /// device id from, on the rare would-be-401-with-MCP-token path.
    config_path: std::path::PathBuf,
}

/// The outcome of consulting a presented MCP token at the auth edge.
enum McpDecision {
    /// Admit the request; carry the comma-joined granted scope groups to stamp on
    /// the trusted `X-ADOS-MCP-Scopes` header for any downstream consumer.
    Admit(String),
    /// A token was presented and verified, but its scopes do not permit this
    /// route class (or the route is not token-reachable). Reject with `403` rather
    /// than the `401` fall-through, so the client learns it is a scope problem.
    ScopeDenied,
    /// No usable MCP token (absent header, the accept flag is off, or the token
    /// is invalid/expired/revoked). Fall through to the normal `401`.
    Fallthrough,
}

impl EdgeAuth {
    /// Consult a presented MCP token for a native route on a would-be-401. Reads
    /// the accept flag + this node's device id fresh (the rate limiter bounds this
    /// rare path) and verifies the token against the current pairing key. Returns
    /// [`McpDecision::Fallthrough`] the moment anything is missing/invalid, so an
    /// absent flag or a bad token behaves exactly as before (a normal 401).
    fn mcp_admits(
        &self,
        method: &http::Method,
        path: &str,
        headers: &http::HeaderMap,
    ) -> McpDecision {
        let Some(token) = headers.get(MCP_TOKEN_HEADER).and_then(|v| v.to_str().ok()) else {
            return McpDecision::Fallthrough;
        };
        // Opt-in: default off. An agent that has never enabled the flag never
        // honors an MCP token, so the whole path is inert until an operator opts in.
        if !ControlSecurityConfig::load_from(&self.config_path)
            .mcp
            .token_accept_enabled
        {
            return McpDecision::Fallthrough;
        }
        let device_id = PairingConfig::load_from(&self.config_path).agent.device_id;
        let pairing = self.pairing.current();
        let Some(claims) = self
            .mcp_tokens
            .verify(&pairing, token, now_unix_ms(), &device_id)
        else {
            return McpDecision::Fallthrough;
        };
        match route_scope(method, path) {
            Some(required) if scope_allows_class(required, &claims.scopes) => {
                McpDecision::Admit(claims.scopes.join(","))
            }
            _ => McpDecision::ScopeDenied,
        }
    }
}

/// The TCP-edge middleware: trustworthy on-box header stamping, then (for native
/// routes) public-path bypass, on-box loopback trust, rate-limit, and auth. The
/// Unix edge does not mount this, so trusted on-box callers bypass all of it.
///
/// Two distinct posture decisions happen here:
///
/// 1. **On-box header.** Every inbound request first has any client-supplied
///    `X-ADOS-Onbox` STRIPPED, then the header is set to `1` only when the
///    front's own on-box check passes (loopback peer + no proxy-forwarding
///    header). Stripping first means a value arriving from off-box cannot be
///    spoofed in, so the residual Python can trust the header the front forwards.
///    This is done for native AND proxied requests so the forwarded value is
///    always trustworthy.
/// 2. **Auth.** A route the front serves natively keeps the full agent auth
///    posture (public bypass, on-box trust, rate-limit, `X-ADOS-Key`). A route
///    that is NOT native falls through to the reverse-proxy: the Rust auth is
///    SKIPPED and the residual FastAPI applies its own auth on the forwarded
///    request (which now carries the trustworthy `X-ADOS-Onbox`).
async fn tcp_edge(State(edge): State<EdgeAuth>, mut request: Request, next: Next) -> Response {
    // ONE normalized path, computed once, threaded through every gate below.
    //
    // Each predicate used to call `request.uri().path()` for itself. That is
    // the raw target from the request line, so `/%61pi/v1/setup/reboot` misses
    // the relay denylist, misses the public list, misses `routing::is_native`
    // — and therefore falls through to the reverse proxy, where the residual
    // FastAPI decodes it the way every ASGI server does and serves
    // `/api/v1/setup/reboot` to a caller with no credential. A refusal is the
    // only safe answer for a path this layer and the next would read
    // differently.
    let path = match auth::decision_path(request.uri().path()) {
        Ok(p) => p,
        Err(reason) => {
            tracing::warn!(
                raw = %request.uri().path(),
                ?reason,
                "non_canonical_path_refused"
            );
            return detail(StatusCode::BAD_REQUEST, auth::PathRejection::MESSAGE);
        }
    };

    // Classify the caller ONCE, from the peer address and the forwarding
    // headers, and hand the one value to every gate below and to every handler
    // (as a request extension, overwriting anything already there). A loopback
    // peer with no forwarding header is the local operator (the `ados` CLI over
    // `127.0.0.1:<port>`); any request that carries a forwarding header was
    // relayed by a proxy or tunnel and is remote, wherever its socket says it
    // came from.
    let peer_ip = request.extensions().get::<PeerAddr>().map(|p| p.0.ip());
    let local_ip = request.extensions().get::<LocalAddr>().map(|l| l.0.ip());
    let caller = classify_caller(peer_ip, local_ip, |h| request.headers().contains_key(h));
    request.extensions_mut().insert(caller);
    let on_box = caller == CallerClass::OnBox;
    let peer_key = PeerKey::of(peer_ip);

    // A request that crossed the radio relay arrives on loopback, so it is
    // on-box by the check above. That is deliberate and load-bearing — a fleet
    // shares one radio key and distributes no per-node API credential, so the
    // relay has nothing to present and refusing the posture would break the lane
    // outright. But it means radio range carries the node's full authority, and
    // a handful of paths turn that into a credential the caller keeps: unpair
    // clears the pairing, the public claim route then hands back a fresh key,
    // and the caller walks away with API access that outlives the radio link.
    //
    // Refuse those paths here, before the posture is applied. Trusting the
    // marker is safe because it can only ever REMOVE authority: a caller who
    // sets it on a local request loses access to these paths, which is no
    // attack, and a caller who omits it off-box was never on-box to begin with.
    let is_relayed = request.headers().contains_key(auth::RELAYED_HEADER);
    if is_relayed && auth::relay_forbidden(&path) {
        tracing::warn!(
            path = %path,
            method = %request.method(),
            "relay_forbidden_path_refused"
        );
        return detail(
            StatusCode::FORBIDDEN,
            "This path cannot be reached over the radio relay.",
        );
    }

    // The config write stays relay-reachable (the slot reconciler and the
    // relayed settings surface use it), but not for the keys that mint a
    // standing credential or re-route the flight link. The body is small JSON;
    // it is read here, checked, and handed on unchanged.
    if is_relayed
        && request.method() == axum::http::Method::PUT
        && path == auth::RELAY_CONFIG_WRITE_PATH
    {
        let (parts, body) = request.into_parts();
        let Ok(bytes) = axum::body::to_bytes(body, RELAY_CONFIG_BODY_LIMIT).await else {
            return detail(
                StatusCode::PAYLOAD_TOO_LARGE,
                "Config write body too large.",
            );
        };
        let key = serde_json::from_slice::<serde_json::Value>(&bytes)
            .ok()
            .and_then(|v| v.get("key").and_then(|k| k.as_str()).map(str::to_string));
        match key {
            Some(key) if !auth::relay_config_key_forbidden(&key) => {}
            other => {
                tracing::warn!(key = ?other, "relay_config_key_refused");
                return detail(
                    StatusCode::FORBIDDEN,
                    "This config key cannot be written over the radio relay.",
                );
            }
        }
        request = Request::from_parts(parts, Body::from(bytes));
    }

    // While UNPAIRED the node would answer every route to anyone, flight
    // control included, for as long as it stays unpaired. Narrow that by caller:
    // the local operator and the first-boot lifelines are served, an
    // operator-LAN browser needs a dashboard PIN session for data, and a remote
    // caller gets neither the data nor the claim that would hand it the key.
    //
    // The other public paths stay open to everyone deliberately:
    // `/api/pairing/{info,code}` is how a device is discovered over the LAN, and
    // the claim stays open to every caller on the device's own networks, which
    // is the documented local-first flow.
    //
    // Checked before the native/proxied split so it covers both surfaces, and
    // read per request through the TTL-cached pairing state, so the posture
    // widens the moment the device is paired and narrows again on unpair with no
    // restart — which a bind-time decision cannot do. See
    // `pairing_posture::CallerClass` for why this is not a bind.
    // The operator's own UI is allowed through alongside the public routes. It
    // serves no data — every `/api/*` call it then makes is refused exactly as
    // before — but withholding the shell left an unpaired node unable to show
    // the operator its own pairing code, and left a browser that already had an
    // old copy with no way to fetch a newer one.
    let unpaired = matches!(
        edge.pairing.current(),
        ados_protocol::pairing_posture::Pairing::Unpaired
    );
    match auth::unpaired_decision(&path, unpaired, caller) {
        auth::UnpairedDecision::Refuse => {
            tracing::warn!(
                path = %path,
                peer = ?peer_ip,
                ?caller,
                "unpaired_peer_refused"
            );
            return detail(
                StatusCode::FORBIDDEN,
                "This device is not paired yet. Pair it first, or reach it over its hotspot or USB connection.",
            );
        }
        auth::UnpairedDecision::RequirePin => {
            // A trusted operator-LAN browser to a DATA route on an UNPAIRED node:
            // it must hold a dashboard PIN session minted via
            // `/api/dashboard/pin/{set,verify}` (which the edge public-exempts so the
            // operator's browser can obtain one). No session -> 403 exactly as the
            // old flat refusal; with a valid session the data loads. The node is
            // unpaired here, so the session validates under the empty-key issuer
            // used when the data plane had no pairing key.
            let session_ok = presented_session(&path, &request)
                .map(|tok| edge.dashboard_pin.session_valid_unpaired(&tok))
                .unwrap_or(false);
            if !session_ok {
                tracing::warn!(
                    path = %path,
                    peer = ?peer_ip,
                    "unpaired_pin_required"
                );
                return detail(
                    StatusCode::FORBIDDEN,
                    "This device is not paired yet. Set up access with the dashboard PIN, or pair it first.",
                );
            }
        }
        auth::UnpairedDecision::Allow => {}
    }

    // Strip any client-supplied on-box header first (it cannot be trusted), then
    // set it only when the front's own check passes — for every request, native
    // or proxied, so the forwarded value is always trustworthy.
    request.headers_mut().remove(ONBOX_HEADER);
    if on_box {
        request
            .headers_mut()
            .insert(ONBOX_HEADER, axum::http::HeaderValue::from_static("1"));
    }

    // Strip any client-supplied MCP-scopes header (it cannot be trusted). It is set
    // below only when a valid MCP token is admitted — for every request, native or
    // proxied — so a value arriving from a client can never be spoofed in. Mirrors
    // the on-box header's strip-then-set discipline.
    request.headers_mut().remove(MCP_SCOPES_HEADER);

    // Every off-box request is charged to its own caller's budget, on both
    // lanes. A single shared bucket let any host on the network 429 every
    // operator; per caller, a flood exhausts only the flooder. The liveness,
    // version and pairing-handshake paths stay exempt so a watchdog or a fresh
    // GCS is never starved; the PIN login is charged, since it is the one
    // public path a guesser would loop on.
    let exempt_from_budget = on_box || (auth::is_public(&path) && !auth::is_pin_login(&path));
    if !exempt_from_budget && !edge.rate.check(peer_key) {
        return detail(
            StatusCode::TOO_MANY_REQUESTS,
            "Request budget exceeded; slow down.",
        );
    }

    // A route the front does not serve natively falls through to the reverse
    // proxy. The front runs the ported auth decision itself before forwarding,
    // so the residual surface no longer carries its own auth layers — the front
    // is the single authenticator for every route it serves or forwards.
    if !crate::routing::is_native(request.method(), &path) {
        return proxied_auth_then_forward(
            edge.proxied.clone(),
            edge.pairing.clone(),
            edge.dashboard_pin.clone(),
            on_box,
            // The SAME normalized path, not a second read of the raw target.
            // A second read is how the two halves of this edge ended up
            // authorizing different strings for one request.
            path,
            request,
            next,
        )
        .await;
    }

    // Liveness, version, and the pairing handshake are public and must always
    // answer before any gate: a fresh GCS has no key yet.
    if auth::is_public(&path) {
        return next.run(request).await;
    }

    if on_box {
        return hmac_then_run(&edge.proxied, &path, request, next).await;
    }

    let presented = request
        .headers()
        .get("X-ADOS-Key")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    if !edge.pairing.authorize(&path, presented.as_deref()) {
        // Before rejecting, accept a valid dashboard session token (minted by the
        // PIN gate) as an alternative data-plane credential. This is the ONLY
        // place the PIN record is read on the native path, and only on a
        // would-be-401 — an on-box or key-bearing request already passed above, so
        // an authenticated dashboard poll does not stat the record every request.
        // The media plane additionally accepts the session in the QUERY STRING
        // (see [`presented_session`]).
        let session_ok = presented_session(&path, &request)
            // `session_valid`, not `session_valid_for`: the latter returns false
            // whenever the node is unpaired, because an unpaired node mints its
            // sessions under a different issuer. This edge used it anyway, which
            // was survivable only while an unpaired node's data plane was open —
            // the stricter check was never reached. Gating the media plane in
            // both states made it reachable, and the result was an operator who
            // had set a PIN getting telemetry but a black video frame: the same
            // session accepted by one layer and refused by the next.
            .map(|tok| {
                edge.dashboard_pin
                    .session_valid(&edge.pairing.current(), &tok)
            })
            .unwrap_or(false);
        if !session_ok {
            // Last-resort: a scoped MCP token (behind the default-off accept flag).
            // Admitted only for a native route whose class the token's scopes cover;
            // a verified-but-wrong-scope token is a 403, an absent/invalid one falls
            // through to the same 401 as before.
            match edge.mcp_admits(request.method(), &path, request.headers()) {
                McpDecision::Admit(scopes) => {
                    // Stamp the trusted scope groups (the client value was stripped
                    // at the top) for any downstream consumer, then admit.
                    if let Ok(v) = axum::http::HeaderValue::from_str(&scopes) {
                        request.headers_mut().insert(MCP_SCOPES_HEADER, v);
                    }
                }
                McpDecision::ScopeDenied => {
                    return detail(
                        StatusCode::FORBIDDEN,
                        "The presented MCP token's scope does not permit this route.",
                    );
                }
                McpDecision::Fallthrough => {
                    // Match the FastAPI message so a GCS that surfaces the body reads
                    // the same text against either surface.
                    return detail(
                        StatusCode::UNAUTHORIZED,
                        "Missing X-ADOS-Key header. This agent is paired and requires authentication.",
                    );
                }
            }
        }
    }
    hmac_then_run(&edge.proxied, &path, request, next).await
}

/// The HMAC/replay gate, then the handler (or the proxy). Applied to native and
/// proxied mutations alike, so `security.hmac_enabled` means every mutation is
/// signed rather than only the ones the residual API happens to serve.
///
/// The body is buffered only when the gate is active for this method and path,
/// and only up to [`HMAC_BODY_LIMIT`]: a larger body is a `413` rather than an
/// unbounded read into a memory-capped process. Otherwise the request streams
/// through untouched, so an upload or an SSE request is not buffered.
async fn hmac_then_run(
    proxied: &ProxiedAuth,
    path: &str,
    request: Request,
    next: Next,
) -> Response {
    let method = request.method().clone();
    if !proxied.hmac_needs_body(&method, path) {
        return next.run(request).await;
    }
    let headers = collect_headers(request.headers());
    let query = request.uri().query().map(str::to_string);
    let (parts, body) = request.into_parts();
    let bytes = match axum::body::to_bytes(body, HMAC_BODY_LIMIT).await {
        Ok(b) => b,
        Err(_) => {
            return detail(
                StatusCode::PAYLOAD_TOO_LARGE,
                "Request body too large to verify its signature.",
            );
        }
    };
    if let Decision::Reject {
        status,
        field,
        message,
    } = proxied.decide_hmac(&method, path, query.as_deref(), &headers, &bytes)
    {
        return reject_response(status, field, message);
    }
    // Rebuild the request with the buffered body so it continues unchanged.
    next.run(Request::from_parts(parts, Body::from(bytes)))
        .await
}

/// Run the ported proxied-route auth decision, then forward to the proxy on an
/// accept. The on-box header has already been stamped on `request` by the
/// caller, so the residual still sees the trustworthy on-box signal. The HMAC
/// gate then runs through [`hmac_then_run`], the same one the native lane uses.
async fn proxied_auth_then_forward(
    proxied: Arc<ProxiedAuth>,
    pairing_state: Arc<PairingState>,
    dashboard_pin: Arc<crate::dashboard_pin::DashboardPin>,
    on_box: bool,
    // `path` is the normalized decision path from `tcp_edge`. Taken as an
    // argument rather than re-read from the request: a second read of
    // `request.uri().path()` gives the RAW target, so this half of the edge
    // would authorize a different string than the half that already ran —
    // which is precisely the percent-encoding bypass.
    path: String,
    request: Request,
    next: Next,
) -> Response {
    let method = request.method().clone();
    let headers = collect_headers(request.headers());
    // The pairing posture comes from the SAME short-TTL-cached reader the native
    // edge uses, so the gate and every other surface agree on one posture.
    let pairing = pairing_state.current();

    // The API-key gate first (the same order the Python middleware stack runs:
    // ApiKeyAuthMiddleware sits outside SecurityMiddleware). A rejection is
    // reversed only by a valid dashboard session token — the SPA also hits
    // proxied routes (setup status, WHEP, etc.), so the session must be an
    // alternative credential here too, symmetric with the native edge.
    if let Decision::Reject {
        status,
        field,
        message,
    } = proxied.decide_api_key(&method, &path, &headers, on_box, &pairing)
    {
        let session_ok = presented_session(&path, &request)
            .map(|tok| dashboard_pin.session_valid(&pairing, &tok))
            .unwrap_or(false);
        // A browser cannot set `X-ADOS-Key` on a WebSocket handshake, so a
        // proxied WS route (e.g. the vision-detections stream) authenticates via
        // a one-shot HMAC ticket in the `Sec-WebSocket-Protocol` list. Admit at
        // the edge when the ticket is authentic + unexpired; the proxied Python
        // route re-verifies the ticket AND enforces the exact route scope, so a
        // wrong-scope ticket is still rejected there.
        let ws_ticket_ok = ws_upgrade_ticket_admits(request.headers(), &pairing);
        if !session_ok && !ws_ticket_ok {
            return reject_response(status, field, message);
        }
    }

    hmac_then_run(&proxied, &path, request, next).await
}

/// True when a WebSocket-upgrade request to a proxied route carries an authentic,
/// unexpired one-shot HMAC ticket in its `Sec-WebSocket-Protocol` list
/// (`["ados-ws-ticket", "<token>"]`). A browser cannot set an `X-ADOS-Key` header
/// on a WS handshake, so the ticket subprotocol is the only data-plane credential
/// it can present for a proxied stream (e.g. `/api/vision/detections/ws`). The
/// front admits an authentic ticket at the edge; the proxied Python route
/// re-verifies the ticket AND enforces the exact route scope via
/// `authenticate_websocket`, so the front verifies against the scope encoded in
/// the token and the route stays the authority on scope. Mirrors the native
/// ground-station WS ticket check in `routes::gs_ws`.
fn ws_upgrade_ticket_admits(headers: &http::HeaderMap, pairing: &Pairing) -> bool {
    if !crate::proxy::is_websocket_upgrade(headers) {
        return false;
    }
    let Pairing::Paired(key) = pairing else {
        return false;
    };
    // Flatten the offered subprotocols (comma-joined within one header and/or
    // split across several); the ticket itself carries no comma so it survives.
    let offered: Vec<String> = headers
        .get_all("sec-websocket-protocol")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|raw| raw.split(','))
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .collect();
    let Some(pos) = offered.iter().position(|p| p == "ados-ws-ticket") else {
        return false;
    };
    let Some(token) = offered.get(pos + 1) else {
        return false;
    };
    // The scope is the token's 2nd `|`-field (`v1|<scope>|<issued>|<expires>|<mac>`).
    // Verify authenticity for that scope; the Python route independently enforces
    // the exact scope it expects, so a wrong-scope ticket is still rejected there.
    let Some(scope) = token.split('|').nth(1).filter(|s| !s.is_empty()) else {
        return false;
    };
    WsTicketIssuer::from_api_key(key)
        .verify(token, scope, now_unix())
        .is_ok()
}

/// The dashboard session a request presents: the `X-ADOS-Dashboard-Session`
/// header, or, on the media plane only, the `ados_session` query parameter.
///
/// The query form is not a convenience. A plain `<video>` element cannot attach
/// a custom header to the requests it makes for a playlist or its segments (the
/// element does the fetching, and there is no hook), so a header-only credential
/// is unreachable for element-driven playback and the operator gets a black
/// frame with no way to authenticate it.
///
/// It is confined to `/whep` and `/hls`. A credential in a URL lands in access
/// logs, browser history and `Referer`, which is why it is not accepted on
/// `/api/*`: those callers are all code that can set a header. Every branch of
/// the edge (unpaired PIN gate, native routes, proxied routes) reads the session
/// through this one function, because `/whep` and `/hls` are proxied and a
/// check that only the native branch applied never ran for them.
fn presented_session(path: &str, request: &Request) -> Option<String> {
    request
        .headers()
        .get(crate::dashboard_pin::DASHBOARD_SESSION_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
        .or_else(|| {
            crate::proxy_auth::is_media_plane(path)
                .then(|| session_token_from_query(request.uri().query()))
                .flatten()
        })
}

/// The query-string parameter carrying a dashboard session on the media plane.
pub(crate) const MEDIA_SESSION_QUERY_KEY: &str = "ados_session";

/// Pull a dashboard session token out of a query string, if one is there.
///
/// Hand-parsed rather than pulled through a URL crate: the input is a raw query
/// fragment, the only key that matters is one exact name, and an empty value is
/// treated as absent so `?ados_session=` cannot read as a credential. The value
/// is percent-decoded once: the token's field separator is `|`, which the
/// clients' `encodeURIComponent` sends as `%7C`. A malformed escape is absent.
pub(crate) fn session_token_from_query(query: Option<&str>) -> Option<String> {
    query?.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        if k != MEDIA_SESSION_QUERY_KEY || v.is_empty() {
            return None;
        }
        percent_decode_once(v)
    })
}

/// Decode `%XX` escapes once. `None` on a malformed escape or non-UTF-8 result.
fn percent_decode_once(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hi = (*bytes.get(i + 1)? as char).to_digit(16)?;
            let lo = (*bytes.get(i + 2)? as char).to_digit(16)?;
            out.push((hi * 16 + lo) as u8);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// Wall-clock unix milliseconds, matching the MCP token's millisecond expiry.
fn now_unix_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Pull the headers the proxied-auth decision reads into the typed struct, so
/// the decision is a pure function of strings (decoupled from the live
/// `HeaderMap`). A non-UTF-8 header value is treated as absent.
fn collect_headers(map: &axum::http::HeaderMap) -> RequestHeaders {
    let get = |name: &str| {
        map.get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
    };
    RequestHeaders {
        origin: get("origin"),
        referer: get("referer"),
        host: get("host"),
        x_ados_key: get("x-ados-key"),
        x_ados_setup_token: get("x-ados-setup-token"),
        x_ados_dashboard_session: get(crate::dashboard_pin::DASHBOARD_SESSION_HEADER),
        x_timestamp: get("x-timestamp"),
        x_nonce: get("x-nonce"),
        x_hmac_signature: get("x-hmac-signature"),
    }
}

/// Turn a `Reject` into the FastAPI-shaped JSON response, rendering the message
/// under `detail` (the API-key middleware) or `error` (the HMAC middleware) so
/// the body matches the Python byte-for-byte.
fn reject_response(status: StatusCode, field: BodyField, message: &str) -> Response {
    match field {
        BodyField::Detail => detail(status, message),
        BodyField::Error => {
            use axum::response::IntoResponse;
            (status, axum::Json(serde_json::json!({ "error": message }))).into_response()
        }
    }
}

/// Build the Unix-edge app: the bare Router, no auth (the socket is the trust
/// boundary).
pub fn unix_app(router: Router) -> Router {
    router
}

/// Build the LAN-edge app: the same Router wrapped with the rate-limit + auth
/// layer keyed on the shared pairing reader. `proxied` carries the ported
/// proxied-route auth decision the front runs on every forwarded request;
/// `dashboard_pin` lets the edge accept a valid dashboard session token as an
/// alternative to `X-ADOS-Key`.
pub fn tcp_app(
    router: Router,
    pairing: Arc<PairingState>,
    proxied: Arc<ProxiedAuth>,
    dashboard_pin: Arc<crate::dashboard_pin::DashboardPin>,
    mcp_tokens: Arc<McpTokenStore>,
    config_path: std::path::PathBuf,
    security: &ControlSecurityConfig,
) -> Router {
    let edge = EdgeAuth {
        pairing,
        rate: Arc::new(RateLimiter::default_control()),
        proxied,
        dashboard_pin,
        mcp_tokens,
        config_path,
    };
    // CORS wraps OUTSIDE the auth layer (ServiceBuilder applies the first
    // layer outermost). A browser cross-origin call to this LAN edge sends a
    // custom `X-ADOS-Key` header, which forces a preflight `OPTIONS` that
    // carries no key — the CORS layer must answer it before `tcp_edge` can
    // 401 it, and it stamps `Access-Control-Allow-Origin` onto every response
    // (including auth rejections) so the GCS reads the real status instead of
    // a CORS error.
    //
    // An ALLOW-LIST, not `CorsLayer::permissive()`. The old comment argued
    // CORS is not a security boundary here because auth is the `X-ADOS-Key` —
    // true for the routes that need a key, and false for the ones that do
    // not. `/api/pairing/{info,code}` are public by necessity, so
    // `Access-Control-Allow-Origin: *` let ANY page the operator happened to
    // visit walk the local network, read an unclaimed agent's pairing code
    // and claim the aircraft from the browser. The allow-list is the same
    // one the residual Python half enforces, read from the same config keys,
    // so the two surfaces cannot disagree about who may call this agent.
    //
    // `expose_headers(Any)` is also gone: nothing on this edge needs a
    // non-simple response header readable cross-origin.
    let origins: Vec<axum::http::HeaderValue> = security
        .security
        .api
        .effective_cors_origins()
        .iter()
        .filter_map(|o| axum::http::HeaderValue::from_str(o).ok())
        .collect();
    let cors = CorsLayer::new()
        .allow_origin(AllowOrigin::list(origins))
        .allow_methods(AllowMethods::mirror_request())
        .allow_headers(AllowHeaders::mirror_request());
    router.layer(
        ServiceBuilder::new()
            .layer(cors)
            .layer(middleware::from_fn_with_state(edge, tcp_edge)),
    )
}

/// Bind the Unix listener through the shared command-plane helper: stale socket
/// removed, mode `0o660`, group `ados-operator`, and a peer-credential check on
/// every accept, so only root and the operator group reach the trusted plane.
pub fn bind_unix(path: &Path) -> std::io::Result<OperatorListener> {
    ados_protocol::ipc::bind_command_socket(path, 0o660)
}

/// Bind the LAN TCP front on the given port across BOTH address families: one
/// AF_INET listener on `0.0.0.0` and one AF_INET6 listener on `::` with
/// `IPV6_V6ONLY` set, so the two sockets do not contend for IPv4-mapped traffic.
/// Returns both listeners; the caller serves the same Router on each.
///
/// A browser resolving a `*.local` host with both A and AAAA records often tries
/// IPv4 first, so a v6-only listener leaves those clients with a TCP RST and a
/// "failed to fetch" in the GCS even though IPv6 link-local works. Binding an
/// explicit pair sidesteps the kernel/dual-stack uncertainty.
///
/// The AF_INET leg is mandatory — its bind error propagates (a port collision is
/// the first thing the inert dual-run must rule out). The AF_INET6 leg is
/// best-effort: on a kernel built without IPv6, or one that rejects the `::`
/// bind, the v6 socket is dropped and the function returns the v4 listener alone,
/// so the front still serves IPv4 clients. Mirrors the Python
/// `make_dual_stack_sockets` helper.
pub async fn bind_tcp(port: u16) -> Result<Vec<TcpListener>> {
    let v4 = bind_one(Domain::IPV4, port, false)
        .with_context(|| format!("bind control TCP port {port} (IPv4)"))?;
    let mut listeners = vec![v4];
    // The IPv6 leg is best-effort: a kernel without IPv6 or a restricted bind
    // leaves the v4 listener serving alone rather than failing bring-up.
    match bind_one(Domain::IPV6, port, true) {
        Ok(v6) => listeners.push(v6),
        Err(e) => {
            tracing::debug!(error = %e, port, "IPv6 control listener unavailable; serving IPv4 only");
        }
    }
    Ok(listeners)
}

/// Bind one address-family listener on the wildcard address for its family.
/// `v6only` forces `IPV6_V6ONLY` on the AF_INET6 socket so the v6 leg never
/// claims IPv4-mapped traffic the v4 leg owns. `SO_REUSEADDR` mirrors the Python
/// helper so a quick restart does not trip `EADDRINUSE` on the TIME_WAIT window.
/// The socket is set non-blocking and handed to tokio as a [`TcpListener`].
fn bind_one(domain: Domain, port: u16, v6only: bool) -> std::io::Result<TcpListener> {
    let socket = Socket::new(domain, Type::STREAM, Some(Protocol::TCP))?;
    socket.set_reuse_address(true)?;
    if v6only {
        socket.set_only_v6(true)?;
    }
    let addr: SocketAddr = if domain == Domain::IPV6 {
        SocketAddr::V6(SocketAddrV6::new(Ipv6Addr::UNSPECIFIED, port, 0, 0))
    } else {
        SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, port))
    };
    socket.bind(&addr.into())?;
    // The same backlog depth the Python helper uses; comfortably above the burst
    // a fresh GCS opens while it walks the pairing handshake.
    socket.listen(2048)?;
    socket.set_nonblocking(true)?;
    TcpListener::from_std(socket.into())
}

/// Serve the Router on the Unix listener: accept connections and hand each to
/// hyper with the axum service, until the stop signal fires. Each connection is
/// driven on its own task so one slow client cannot stall the accept loop. The
/// listener admits only root and operator-group peers; the Unix edge carries no
/// peer address.
pub async fn serve_unix(listener: OperatorListener, app: Router, stop: oneshot::Receiver<()>) {
    tokio::pin!(stop);
    loop {
        tokio::select! {
            _ = &mut stop => break,
            accepted = listener.accept() => {
                match accepted {
                    Ok((stream, _addr)) => {
                        let app = app.clone();
                        tokio::spawn(serve_conn(TokioIo::new(stream), app, ConnEdge::Unix));
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "control unix accept failed");
                        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                    }
                }
            }
        }
    }
}

/// Serve the Router on the TCP listener, mirroring the unix accept loop. Unlike
/// the logd listener, the accepted peer and local addresses are threaded into
/// each connection so the edge middleware can classify the caller.
///
/// Connections are capped: [`MAX_TCP_CONNECTIONS`] in total and
/// [`MAX_TCP_CONNECTIONS_PER_PEER`] per off-box caller. One host opening
/// connections and trickling header bytes used to exhaust the process's
/// descriptors and cut every client off; past a cap the new connection is
/// closed on accept. Loopback callers (the relay, a tunnel's ingress, the CLI)
/// share one address, so they count against the total only.
pub async fn serve_tcp(listener: TcpListener, app: Router, stop: oneshot::Receiver<()>) {
    tokio::pin!(stop);
    let total = Arc::new(Semaphore::new(MAX_TCP_CONNECTIONS));
    let counts = ConnCounts::default();
    loop {
        tokio::select! {
            _ = &mut stop => break,
            accepted = listener.accept() => {
                match accepted {
                    Ok((stream, peer)) => {
                        let key = (!peer.ip().to_canonical().is_loopback())
                            .then(|| PeerKey::of(Some(peer.ip())));
                        let Some(slot) = counts.acquire(key, &total) else {
                            tracing::debug!(peer = %peer, "control tcp connection cap reached");
                            continue;
                        };
                        let local = stream.local_addr().ok();
                        let app = app.clone();
                        tokio::spawn(async move {
                            serve_conn(TokioIo::new(stream), app, ConnEdge::Tcp { peer, local }).await;
                            drop(slot);
                        });
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "control tcp accept failed");
                        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                    }
                }
            }
        }
    }
}

/// Drive one accepted connection through hyper with the axum service. Generic
/// over the IO so the same code serves a Unix stream and a TCP stream. The TCP
/// edge stamps the peer and local addresses for [`tcp_edge`] to classify; the
/// Unix edge stamps [`CallerClass::OnBox`] directly, since its listener already
/// admitted only root and operator-group peers.
///
/// HTTP/1 only (nothing on this edge speaks cleartext HTTP/2), with a request
/// head deadline of [`HEADER_READ_TIMEOUT`] that also bounds the idle wait
/// between requests on a kept-alive connection.
async fn serve_conn<I>(io: TokioIo<I>, app: Router, edge: ConnEdge)
where
    I: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    // Bridge the axum Router (a tower Service over axum's Request) to hyper's
    // service over `Incoming` request bodies, stamping the caller evidence on
    // every request.
    let svc = hyper::service::service_fn(move |req: hyper::Request<Incoming>| {
        let mut app = app.clone();
        async move {
            let mut req = req.map(Body::new);
            match edge {
                ConnEdge::Unix => {
                    req.extensions_mut().insert(CallerClass::OnBox);
                }
                ConnEdge::Tcp { peer, local } => {
                    req.extensions_mut().insert(PeerAddr(peer));
                    if let Some(local) = local {
                        req.extensions_mut().insert(LocalAddr(local));
                    }
                }
            }
            // Router implements Service<Request<Body>>; readiness is immediate.
            let response = app.call(req).await?;
            Ok::<_, Infallible>(response)
        }
    });
    let mut builder = ConnBuilder::new(TokioExecutor::new()).http1_only();
    builder
        .http1()
        .timer(TokioTimer::new())
        .header_read_timeout(HEADER_READ_TIMEOUT);
    if let Err(e) = builder.serve_connection_with_upgrades(io, svc).await {
        tracing::debug!(error = %e, "control connection ended");
    }
}

#[cfg(test)]
mod caller_tests;

#[cfg(test)]
mod tests {
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
            .token;

        // A WS upgrade carrying an authentic ticket subprotocol is admitted.
        let good = ws_headers(Some(&format!("ados-ws-ticket, {ticket}")));
        assert!(ws_upgrade_ticket_admits(&good, &paired));

        // No ticket in the subprotocol list → not admitted.
        assert!(!ws_upgrade_ticket_admits(&ws_headers(None), &paired));

        // A valid ticket present but the request is NOT a WS upgrade → not admitted.
        let mut plain = http::HeaderMap::new();
        plain.insert(
            "sec-websocket-protocol",
            format!("ados-ws-ticket, {ticket}").parse().unwrap(),
        );
        assert!(!ws_upgrade_ticket_admits(&plain, &paired));

        // Unpaired agent → the edge handles openness elsewhere; the ticket helper
        // never admits on its own.
        assert!(!ws_upgrade_ticket_admits(&good, &Pairing::Unpaired));

        // A ticket signed by a DIFFERENT pairing key → rejected.
        let forged = WsTicketIssuer::from_api_key("other-key")
            .mint("vision.detections", 30)
            .token;
        let bad = ws_headers(Some(&format!("ados-ws-ticket, {forged}")));
        assert!(!ws_upgrade_ticket_admits(&bad, &paired));
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
            format!(
                "mcp:\n  token_accept_enabled: {accept_enabled}\nagent:\n  device_id: node-1\n"
            ),
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
            "/api/plugins/install/resume",
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
}
