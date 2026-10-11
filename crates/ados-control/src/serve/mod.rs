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

mod edge;
mod edge_util;
mod listener;
mod ws_ticket;

#[cfg(test)]
use edge::McpDecision;
use edge::{tcp_edge, EdgeAuth};
pub use listener::{bind_tcp, bind_unix, serve_tcp, serve_unix};
pub(crate) use ws_ticket::MEDIA_SESSION_QUERY_KEY;
use ws_ticket::*;

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

#[cfg(test)]
mod caller_tests;

#[cfg(test)]
mod tests;

#[cfg(test)]
mod edge_tests;
