//! The dual-listener serve loop: one axum `Router` on two edges.
//!
//! The same Router is bound on two edges, mirroring the logging store's read
//! surface:
//!
//! 1. **The trusted local Unix socket** (`0o660`, tmpfs). No auth, no rate
//!    limit: anything on-box that can open the socket is inside the trust
//!    boundary, and this path keeps working even if the LAN edge is gated. The
//!    GCS does not use it; the on-box CLI does.
//! 2. **A LAN TCP port.** The auth layer mirrors the agent's HTTP posture:
//!    unpaired ⇒ open, paired ⇒ `X-ADOS-Key` required, with on-box loopback
//!    trust and a token-bucket rate limit guarding the edge.
//!
//! The one difference from the logd listener is the peer address: the LAN edge
//! threads the accepted connection's [`SocketAddr`] into the request as an
//! extension so the auth middleware can grant on-box loopback trust to a request
//! arriving over loopback TCP (the local CLI hitting `127.0.0.1:<port>` rather
//! than the Unix socket). The Unix edge carries no peer address — it is trusted
//! outright and never installs the auth layer.

use std::convert::Infallible;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6};
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::middleware::{self, Next};
use axum::response::Response;
use axum::Router;
use hyper::body::Incoming;
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto::Builder as ConnBuilder;
use socket2::{Domain, Protocol, Socket, Type};
use tokio::net::{TcpListener, UnixListener};
use tokio::sync::oneshot;
use tower::{Service, ServiceBuilder};
use tower_http::cors::CorsLayer;

use crate::auth::{self, Pairing, PairingState, RateLimiter};
use crate::config::{ControlSecurityConfig, PairingConfig};
use crate::mcp::{route_scope, McpTokenStore, MCP_SCOPES_HEADER, MCP_TOKEN_HEADER};
use crate::proxy_auth::{BodyField, Decision, ProxiedAuth, RequestHeaders};
use crate::routes::detail;
use ados_protocol::mcp_token::scope_allows_class;
use ados_protocol::ws_ticket::{now_unix, WsTicketIssuer};

/// The header the front stamps on a request that passes its on-box loopback
/// check, so the residual Python (which does not see the TCP peer) can honour the
/// same on-box trust the native edge applies. It is STRIPPED from every inbound
/// request first, then set only when the front's own check passes, so a value
/// arriving from off-box can never be spoofed in. See [`tcp_edge`]. Public so a
/// native handler (e.g. the dashboard-PIN set route) can read the trustworthy
/// on-box signal the edge stamped.
pub const ONBOX_HEADER: &str = "x-ados-onbox";

/// The peer address of the accepted connection, attached to each LAN-edge
/// request as an extension so the auth middleware can apply on-box loopback
/// trust. Absent on the Unix edge (which is trusted outright).
#[derive(Clone, Copy, Debug)]
struct PeerAddr(SocketAddr);

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
    let path = request.uri().path().to_string();

    // On-box loopback trust: a request whose peer is loopback and that carries no
    // proxy-forwarding header is the local operator (the `ados` CLI over
    // `127.0.0.1:<port>`), who already holds shell-level privilege that exceeds
    // API auth. A tunnel terminating on loopback is excluded by the
    // forwarding-header check. This mirrors the FastAPI `_is_on_box` contract.
    let peer_is_loopback = request
        .extensions()
        .get::<PeerAddr>()
        .map(|p| p.0.ip().is_loopback())
        .unwrap_or(false);
    let has_forwarding_header = auth::FORWARDED_HEADERS
        .iter()
        .any(|h| request.headers().contains_key(*h));
    let on_box = auth::is_on_box(peer_is_loopback, has_forwarding_header);

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
            request,
            next,
        )
        .await;
    }

    // Liveness, version, and the pairing handshake are public and must always
    // answer before any gate: a fresh GCS has no key yet, and a watchdog hitting
    // `/healthz` must never be starved by a request flood, so the public paths
    // skip the rate limiter and the auth check.
    if auth::is_public(&path) {
        return next.run(request).await;
    }

    if on_box {
        return next.run(request).await;
    }

    // Rate limit before the pairing read so a flood does not even reach it.
    if !edge.rate.check() {
        return detail(
            StatusCode::TOO_MANY_REQUESTS,
            "Request budget exceeded; slow down.",
        );
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
        let session_ok = request
            .headers()
            .get(crate::dashboard_pin::DASHBOARD_SESSION_HEADER)
            .and_then(|v| v.to_str().ok())
            .map(|tok| {
                edge.dashboard_pin
                    .session_valid_for(&edge.pairing.current(), tok)
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
    next.run(request).await
}

/// Run the ported proxied-route auth decision, then forward to the proxy on an
/// accept. The on-box header has already been stamped on `request` by the
/// caller, so the residual still sees the trustworthy on-box signal. The body
/// is buffered ONLY when the HMAC gate needs it (a mutating, non-exempt method
/// while HMAC is active); otherwise it streams through untouched, so a large
/// upload or an SSE request is not buffered.
async fn proxied_auth_then_forward(
    proxied: Arc<ProxiedAuth>,
    pairing_state: Arc<PairingState>,
    dashboard_pin: Arc<crate::dashboard_pin::DashboardPin>,
    on_box: bool,
    request: Request,
    next: Next,
) -> Response {
    let method = request.method().clone();
    let path = request.uri().path().to_string();
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
        let session_ok = headers
            .x_ados_dashboard_session
            .as_deref()
            .map(|tok| dashboard_pin.session_valid_for(&pairing, tok))
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

    // The HMAC gate. Only here do we touch the body, and only when the gate is
    // active for this method+path; otherwise forward the original request with
    // its body still streaming.
    if proxied.hmac_needs_body(&method, &path) {
        let (parts, body) = request.into_parts();
        let bytes = match axum::body::to_bytes(body, usize::MAX).await {
            Ok(b) => b,
            Err(_) => {
                // A body we cannot read cannot be HMAC-verified; reject with the
                // same shape an invalid signature would (the request never
                // reaches the upstream).
                return reject_response(
                    StatusCode::UNAUTHORIZED,
                    BodyField::Error,
                    "Invalid HMAC signature",
                );
            }
        };
        if let Decision::Reject {
            status,
            field,
            message,
        } = proxied.decide_hmac(&method, &path, &headers, &bytes)
        {
            return reject_response(status, field, message);
        }
        // Rebuild the request with the buffered body so the proxy still streams
        // it downstream unchanged.
        let rebuilt = Request::from_parts(parts, Body::from(bytes));
        return next.run(rebuilt).await;
    }

    next.run(request).await
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
) -> Router {
    let edge = EdgeAuth {
        pairing,
        rate: Arc::new(RateLimiter::default_control()),
        proxied,
        dashboard_pin,
        mcp_tokens,
        config_path,
    };
    // CORS wraps OUTSIDE the auth layer (ServiceBuilder applies the first layer
    // outermost). A browser cross-origin call to this LAN edge sends a custom
    // `X-ADOS-Key` header, which forces a preflight `OPTIONS` that carries no
    // key — the CORS layer must answer it before `tcp_edge` can 401 it, and it
    // stamps `Access-Control-Allow-Origin` onto every response (incl. auth
    // rejections) so the GCS reads the real status instead of a CORS error.
    // Auth stays the X-ADOS-Key (CORS is not a security boundary here), so any
    // GCS origin is allowed. Restores the CORS the FastAPI front carried before
    // the native front took over :8080.
    router.layer(
        ServiceBuilder::new()
            .layer(CorsLayer::permissive())
            .layer(middleware::from_fn_with_state(edge, tcp_edge)),
    )
}

/// Bind the Unix listener, removing a stale socket and tightening the mode to
/// `0o660` on Linux so only the agent group can reach the trusted plane.
pub fn bind_unix(path: &Path) -> std::io::Result<UnixListener> {
    // The shared helper owns the create-dir / remove-stale / bind / chmod
    // (0o660) hygiene; group-owning to `ados` afterward keeps the mode's
    // group-rw grant reaching a non-root operator (a chown does not clear the rw
    // bits, so the final owner+group+mode state is unchanged).
    let listener = ados_protocol::ipc::bind_command_socket(path, 0o660)?;
    #[cfg(target_os = "linux")]
    crate::set_ados_group(path);
    Ok(listener)
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
/// Unix edge carries no peer address (it is trusted outright).
pub async fn serve_unix(listener: UnixListener, app: Router, stop: oneshot::Receiver<()>) {
    tokio::pin!(stop);
    loop {
        tokio::select! {
            _ = &mut stop => break,
            accepted = listener.accept() => {
                match accepted {
                    Ok((stream, _addr)) => {
                        let app = app.clone();
                        tokio::spawn(serve_conn(TokioIo::new(stream), app, None));
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
/// the logd listener, the accepted peer address is threaded into each connection
/// so the auth middleware can grant on-box loopback trust to a request arriving
/// over loopback TCP.
pub async fn serve_tcp(listener: TcpListener, app: Router, stop: oneshot::Receiver<()>) {
    tokio::pin!(stop);
    loop {
        tokio::select! {
            _ = &mut stop => break,
            accepted = listener.accept() => {
                match accepted {
                    Ok((stream, peer)) => {
                        let app = app.clone();
                        tokio::spawn(serve_conn(TokioIo::new(stream), app, Some(peer)));
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
/// over the IO so the same code serves a Unix stream and a TCP stream. When a
/// `peer` is given (the TCP edge), it is inserted as a request extension so the
/// auth middleware can read it; the Unix edge passes `None`.
async fn serve_conn<I>(io: TokioIo<I>, app: Router, peer: Option<SocketAddr>)
where
    I: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    // Bridge the axum Router (a tower Service over axum's Request) to hyper's
    // service over `Incoming` request bodies, stamping the peer address on the
    // request so the LAN-edge middleware can apply loopback trust.
    let svc = hyper::service::service_fn(move |req: hyper::Request<Incoming>| {
        let mut app = app.clone();
        async move {
            let mut req = req.map(Body::new);
            if let Some(addr) = peer {
                req.extensions_mut().insert(PeerAddr(addr));
            }
            // Router implements Service<Request<Body>>; readiness is immediate.
            let response = app.call(req).await?;
            Ok::<_, Infallible>(response)
        }
    });
    if let Err(e) = ConnBuilder::new(TokioExecutor::new())
        .serve_connection_with_upgrades(io, svc)
        .await
    {
        tracing::debug!(error = %e, "control connection ended");
    }
}

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
