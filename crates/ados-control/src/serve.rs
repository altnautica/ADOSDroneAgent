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
use tower::Service;
use tower_http::cors::{AllowHeaders, AllowMethods, CorsLayer};

use crate::auth::{self, Pairing, PairingState, PeerKey, RateLimiter};
use crate::config::PairingConfig;
use crate::mcp::{
    route_decides_scope, route_scope, McpTokenStore, MCP_SCOPES_HEADER, MCP_TOKEN_HEADER,
};
use crate::proxy_auth::{
    is_sha256_hex, BodyField, Decision, ProxiedAuth, RequestHeaders, CONTENT_DIGEST_HEADER,
};
use crate::routes::detail;
use crate::security_state::SecurityState;
use ados_protocol::ipc::OperatorListener;
use ados_protocol::mcp_token::scope_allows_class;
use ados_protocol::pairing_posture::{classify_caller, CallerClass};
use ados_protocol::ws_ticket::{now_unix, WsTicketIssuer};

mod edge_util;

pub use edge_util::PeerKeyHint;
use edge_util::{
    canonical_uri, config_unreadable, is_mutating, keying_peer, live_cors_origins,
    proxied_ws_scope, unix_edge,
};

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

/// Concurrent connections every loopback peer may hold together: the tunnel's
/// ingress, the radio relay and the on-box CLI all arrive on loopback, and
/// before the request head arrives they cannot be told apart. Without this
/// sub-cap one tunnelled client could take the whole total.
const MAX_REMOTE_LOOPBACK_CONNS: usize = 64;

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

/// Holds one connection's slot: a share of the total, plus one of its caller's
/// slots. Released on drop.
struct ConnSlot {
    counted: (ConnCounts, PeerKey),
    _permit: OwnedSemaphorePermit,
}

impl ConnCounts {
    /// Take a slot for `key` if both the total and the caller's `cap` allow.
    fn acquire(&self, key: PeerKey, cap: usize, total: &Arc<Semaphore>) -> Option<ConnSlot> {
        let permit = total.clone().try_acquire_owned().ok()?;
        let mut counts = self.0.lock().unwrap_or_else(|p| p.into_inner());
        let held = counts.entry(key).or_insert(0);
        if *held >= cap {
            return None;
        }
        *held += 1;
        Some(ConnSlot {
            counted: (self.clone(), key),
            _permit: permit,
        })
    }
}

/// The connection-accounting key and cap for an accepted peer: every loopback
/// peer shares one key under [`MAX_REMOTE_LOOPBACK_CONNS`]; any other caller is
/// keyed by address under [`MAX_TCP_CONNECTIONS_PER_PEER`].
fn conn_key(peer: std::net::IpAddr) -> (PeerKey, usize) {
    if peer.to_canonical().is_loopback() {
        (
            PeerKey::of(Some(Ipv4Addr::LOCALHOST.into())),
            MAX_REMOTE_LOOPBACK_CONNS,
        )
    } else {
        (PeerKey::of(Some(peer)), MAX_TCP_CONNECTIONS_PER_PEER)
    }
}

impl Drop for ConnSlot {
    fn drop(&mut self) {
        let (counts, key) = &self.counted;
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
    /// The agent config path the edge reads this node's device id from, on the
    /// rare would-be-401-with-MCP-token path.
    config_path: std::path::PathBuf,
    /// The live `security:` / `mcp:` posture: the MCP accept flag, and whether
    /// the config is unreadable (off-box mutations are then refused).
    security: Arc<SecurityState>,
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
    /// the accept flag + this node's device id from the cached config snapshot
    /// and verifies the token against the current pairing key. Returns
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
        if !self.security.config_or_default().mcp.token_accept_enabled {
            return McpDecision::Fallthrough;
        }
        let device_id = PairingConfig::cached(&self.config_path)
            .agent
            .device_id
            .clone();
        let pairing = self.pairing.current();
        let Some(claims) = self
            .mcp_tokens
            .verify(&pairing, token, now_unix_ms(), &device_id)
        else {
            return McpDecision::Fallthrough;
        };
        // A route that classifies the request from what it reads (a plugin
        // tool's declared safety class) is admitted here and enforces the
        // class itself against the scopes stamped on the request.
        if route_decides_scope(method, path) && !claims.scopes.is_empty() {
            return McpDecision::Admit(claims.scopes.join(","));
        }
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
    // The router behind this edge, the fallback proxy and the residual all
    // read the request target, so they must read the string this edge just
    // authorized: an accepted encoded spelling is rewritten to the canonical
    // encoding of the decoded path before anything downstream sees it.
    if path != request.uri().path() {
        match canonical_uri(request.uri(), &path) {
            Some(uri) => *request.uri_mut() = uri,
            None => return detail(StatusCode::BAD_REQUEST, auth::PathRejection::MESSAGE),
        }
    }

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
    // Who the request is charged to (rate limit, PIN lockout): a tunnelled
    // caller on loopback by its forwarded address, anyone else by its peer.
    let peer_key = keying_peer(peer_ip, request.headers());
    request.extensions_mut().insert(PeerKeyHint(peer_key));

    // A config that has never parsed since this process started leaves the
    // security posture unknown. Failing open would mean "HMAC off, no
    // configured key"; instead every off-box write is refused until the file
    // is repaired on the node (the on-box operator and the Unix socket stay
    // open for exactly that).
    if !on_box && is_mutating(request.method()) && edge.security.is_unreadable() {
        tracing::warn!(path = %path, "config_unreadable_mutation_refused");
        return config_unreadable();
    }

    // A request that crossed the radio relay arrives on loopback, so it is
    // on-box by the check above. The drone's relay handler admitted it only
    // after verifying a per-pair ticket bound to this exact request, so it
    // carries the linked ground station's authority. What it must not do is
    // change who the node trusts: pairing, credentials, cloud posture. Those
    // paths are refused here, before the posture is applied. Trusting the
    // marker is safe because it can only ever REMOVE authority: a caller who
    // sets it on a local request loses access to these paths, which is no
    // attack, and a caller who omits it off-box was never on-box to begin with.
    let is_relayed = request.headers().contains_key(auth::RELAYED_HEADER);
    // The relay ticket already authenticated this exact request (method, path
    // and body), so the HMAC gate is not asked to authenticate it a second
    // time with a secret the relay does not hold. Only an on-box caller can be
    // relayed: the marker on an off-box request changes nothing here.
    let hmac_exempt = on_box && is_relayed;
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
    // relayed settings surface use it), but not for the trust-root keys. The
    // body is small JSON; it is read here, checked, and handed on unchanged.
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
    match auth::unpaired_decision(request.method(), &path, unpaired, caller) {
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
            &edge,
            on_box,
            hmac_exempt,
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

    if hmac_exempt {
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
        // A browser cannot set `X-ADOS-Key` on a WebSocket handshake. A native
        // stream whose ticket scope names the path's own id (an install job's
        // progress, a plugin's HTTP passthrough) is admitted on a ticket minted
        // for exactly that scope; any other ticket opens nothing here.
        let ticket_ok = !session_ok
            && native_ws_ticket_admits(request.headers(), &edge.pairing.current(), &path);
        if !session_ok && !ticket_ok {
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
///
/// A streaming upload (plugin archive, vision model) that declares its body
/// digest in `X-Content-SHA256` is verified over that digest instead and
/// forwarded with the header and its body unread; the route hashes the body as
/// it streams and rejects a mismatch.
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
    let digest = request
        .headers()
        .get(CONTENT_DIGEST_HEADER)
        .and_then(|v| v.to_str().ok())
        .filter(|d| is_sha256_hex(d))
        .map(str::to_ascii_lowercase);
    if let Some(digest) = digest.filter(|_| ProxiedAuth::is_digest_signed_route(&method, path)) {
        if let Decision::Reject {
            status,
            field,
            message,
        } = proxied.decide_hmac_digest(&method, path, query.as_deref(), &headers, &digest)
        {
            return reject_response(status, field, message);
        }
        return next.run(request).await;
    }
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
    edge: &EdgeAuth,
    on_box: bool,
    hmac_exempt: bool,
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
    let pairing = edge.pairing.current();

    // The API-key gate first (the same order the Python middleware stack runs:
    // ApiKeyAuthMiddleware sits outside SecurityMiddleware). A rejection is
    // reversed only by a valid dashboard session token — the SPA also hits
    // proxied routes (setup status, WHEP, etc.), so the session must be an
    // alternative credential here too, symmetric with the native edge.
    if let Decision::Reject {
        status,
        field,
        message,
    } = edge
        .proxied
        .decide_api_key(&method, &path, &headers, on_box, &pairing)
    {
        // A dashboard session is a browser login, not a credential that may
        // re-point the node's cloud posture: those routes demand the key or
        // the setup token, and a session never stands in for them.
        let session_ok = !ProxiedAuth::is_cloud_posture_path(&path)
            && presented_session(&path, &request)
                .map(|tok| edge.dashboard_pin.session_valid(&pairing, &tok))
                .unwrap_or(false);
        // A browser cannot set `X-ADOS-Key` on a WebSocket handshake, so a
        // proxied WS route (e.g. the vision-detections stream) authenticates via
        // a one-shot HMAC ticket in the `Sec-WebSocket-Protocol` list, minted
        // for the scope bound to this path. The proxied Python route
        // re-verifies the ticket and its scope as well.
        let ws_ticket_ok = ws_upgrade_ticket_admits(request.headers(), &pairing, &path);
        if !session_ok && !ws_ticket_ok {
            return reject_response(status, field, message);
        }
    }

    if hmac_exempt {
        return next.run(request).await;
    }
    hmac_then_run(&edge.proxied, &path, request, next).await
}

/// True when a WebSocket-upgrade request to a proxied route carries an authentic,
/// unexpired one-shot HMAC ticket in its `Sec-WebSocket-Protocol` list
/// (`["ados-ws-ticket", "<token>"]`) minted for the scope bound to `path` (see
/// [`edge_util::PROXIED_WS_SCOPES`]). A browser cannot set an `X-ADOS-Key`
/// header on a WS handshake, so the ticket subprotocol is the only data-plane
/// credential it can present for a proxied stream. A ticket for any other
/// scope, or an upgrade to a path with no bound scope, is refused here rather
/// than left to the residual route. Parallels [`native_ws_ticket_admits`].
fn ws_upgrade_ticket_admits(headers: &http::HeaderMap, pairing: &Pairing, path: &str) -> bool {
    if !crate::proxy::is_websocket_upgrade(headers) {
        return false;
    }
    let Pairing::Paired(key) = pairing else {
        return false;
    };
    let (Some(scope), Some(token)) = (proxied_ws_scope(path), offered_ticket(headers)) else {
        return false;
    };
    WsTicketIssuer::from_api_key(key)
        .verify_once(&token, scope, now_unix())
        .is_ok()
}

/// True when a WebSocket upgrade to a native `path` carries a ticket minted for
/// the scope bound to that path (see
/// [`crate::routes::ws_ticket::path_bound_scope`]). The route itself therefore
/// needs no ticket check: only the one scope it serves gets through.
fn native_ws_ticket_admits(headers: &http::HeaderMap, pairing: &Pairing, path: &str) -> bool {
    if !crate::proxy::is_websocket_upgrade(headers) {
        return false;
    }
    let Pairing::Paired(key) = pairing else {
        return false;
    };
    let (Some(scope), Some(token)) = (
        crate::routes::ws_ticket::path_bound_scope(path),
        offered_ticket(headers),
    ) else {
        return false;
    };
    WsTicketIssuer::from_api_key(key)
        .verify_once(&token, &scope, now_unix())
        .is_ok()
}

/// The ticket a WebSocket handshake offers: the entry after the
/// `ados-ws-ticket` marker in its `Sec-WebSocket-Protocol` list (comma-joined
/// within one header and/or split across several; a ticket has no comma).
fn offered_ticket(headers: &http::HeaderMap) -> Option<String> {
    let offered: Vec<String> = headers
        .get_all("sec-websocket-protocol")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|raw| raw.split(','))
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .collect();
    let pos = offered.iter().position(|p| p == "ados-ws-ticket")?;
    offered.get(pos + 1).cloned()
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
/// boundary). Every caller here is on-box, so the residual sees
/// `x-ados-onbox: 1` on every request, whatever the client sent.
pub fn unix_app(router: Router) -> Router {
    router.layer(middleware::from_fn(unix_edge))
}

/// The router behind the LAN edge: `router` mounted as the fallback of an
/// outer router that carries [`tcp_edge`]. A layer on a router runs after that
/// router has matched a route, so the edge has to sit outside the router it
/// guards for its canonical request target to be the one that is routed.
fn edge_router(router: Router, edge: EdgeAuth) -> Router {
    Router::new()
        .fallback_service(router)
        .layer(middleware::from_fn_with_state(edge, tcp_edge))
}

/// Build the LAN-edge app: the same Router wrapped with the rate-limit + auth
/// layer keyed on the shared pairing reader. `proxied` carries the ported
/// proxied-route auth decision the front runs on every forwarded request;
/// `dashboard_pin` lets the edge accept a valid dashboard session token as an
/// alternative to `X-ADOS-Key`; `security` is the live `security:` posture the
/// CORS allow-list and the unreadable-config refusal read per request.
pub fn tcp_app(
    router: Router,
    pairing: Arc<PairingState>,
    proxied: Arc<ProxiedAuth>,
    dashboard_pin: Arc<crate::dashboard_pin::DashboardPin>,
    mcp_tokens: Arc<McpTokenStore>,
    config_path: std::path::PathBuf,
    security: Arc<SecurityState>,
) -> Router {
    let edge = EdgeAuth {
        pairing,
        rate: Arc::new(RateLimiter::default_control()),
        proxied,
        dashboard_pin,
        mcp_tokens,
        config_path,
        security: Arc::clone(&security),
    };
    // CORS wraps OUTSIDE the auth layer (the layer added last is outermost).
    // A browser cross-origin call to this LAN edge sends a
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
    // one the residual Python half enforces, read from the same config keys
    // (per request, so an edit applies without a restart), so the two
    // surfaces cannot disagree about who may call this agent.
    //
    // `expose_headers(Any)` is also gone: nothing on this edge needs a
    // non-simple response header readable cross-origin.
    let cors = CorsLayer::new()
        .allow_origin(live_cors_origins(security))
        .allow_methods(AllowMethods::mirror_request())
        .allow_headers(AllowHeaders::mirror_request());
    edge_router(router, edge).layer(cors)
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
/// so the front still serves IPv4 clients.
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
/// Connections are capped: [`MAX_TCP_CONNECTIONS`] in total,
/// [`MAX_TCP_CONNECTIONS_PER_PEER`] per off-box caller, and
/// [`MAX_REMOTE_LOOPBACK_CONNS`] across every loopback peer together (the
/// relay, a tunnel's ingress and the CLI share one address). One host opening
/// connections and trickling header bytes used to exhaust the process's
/// descriptors and cut every client off; past a cap the new connection is
/// closed on accept.
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
                        let (key, cap) = conn_key(peer.ip());
                        let Some(slot) = counts.acquire(key, cap, &total) else {
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
mod tests;

#[cfg(test)]
mod edge_tests;
