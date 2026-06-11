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
use std::net::SocketAddr;
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
use tokio::net::{TcpListener, UnixListener};
use tokio::sync::oneshot;
use tower::Service;

use crate::auth::{self, PairingState, RateLimiter};
use crate::routes::detail;

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
}

/// The TCP-edge middleware: public-path bypass, then on-box loopback trust, then
/// rate-limit, then auth. The Unix edge does not mount this, so trusted on-box
/// callers bypass all of it.
async fn tcp_edge(State(edge): State<EdgeAuth>, request: Request, next: Next) -> Response {
    let path = request.uri().path().to_string();

    // Liveness, version, and the pairing handshake are public and must always
    // answer before any gate: a fresh GCS has no key yet, and a watchdog hitting
    // `/healthz` must never be starved by a request flood, so the public paths
    // skip the rate limiter and the auth check.
    if auth::is_public(&path) {
        return next.run(request).await;
    }

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
    if auth::is_on_box(peer_is_loopback, has_forwarding_header) {
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
        // Match the FastAPI message so a GCS that surfaces the body reads the
        // same text against either surface.
        return detail(
            StatusCode::UNAUTHORIZED,
            "Missing X-ADOS-Key header. This agent is paired and requires authentication.",
        );
    }
    next.run(request).await
}

/// Build the Unix-edge app: the bare Router, no auth (the socket is the trust
/// boundary).
pub fn unix_app(router: Router) -> Router {
    router
}

/// Build the LAN-edge app: the same Router wrapped with the rate-limit + auth
/// layer keyed on the shared pairing reader.
pub fn tcp_app(router: Router, pairing: Arc<PairingState>) -> Router {
    let edge = EdgeAuth {
        pairing,
        rate: Arc::new(RateLimiter::default_control()),
    };
    router.layer(middleware::from_fn_with_state(edge, tcp_edge))
}

/// Bind the Unix listener, removing a stale socket and tightening the mode to
/// `0o660` on Linux so only the agent group can reach the trusted plane.
pub fn bind_unix(path: &Path) -> std::io::Result<UnixListener> {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::remove_file(path);
    let listener = UnixListener::bind(path)?;
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::fs::PermissionsExt;
        // Group-own to `ados` first, then set the mode: the 0o660 grant only
        // reaches a non-root operator once the group owns the socket.
        crate::set_ados_group(path);
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o660));
    }
    Ok(listener)
}

/// Bind the LAN TCP listener on the given port across all interfaces.
pub async fn bind_tcp(port: u16) -> Result<TcpListener> {
    TcpListener::bind(("0.0.0.0", port))
        .await
        .with_context(|| format!("bind control TCP port {port}"))
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
