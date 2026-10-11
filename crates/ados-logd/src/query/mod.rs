//! The read surface: one axum `Router` served on two listeners.
//!
//! The store is queryable over a stable HTTP `/v1` API. The exact same Router
//! is bound on two edges:
//!
//! 1. **`/run/ados/logd-query.sock`** — the trusted local Unix socket
//!    (`0o660 root:ados-operator`, tmpfs). No auth, no rate limit: only root
//!    and the operator group can open it, and every accept re-checks the
//!    peer's credentials. This path keeps working even if the Python API is
//!    down (the diagnostics tool must not share a failure domain with the thing
//!    it diagnoses).
//! 2. **TCP `:8090`** — the LAN edge. The auth layer follows the agent's
//!    data-plane posture (see [`auth`]): unpaired ⇒ first-boot reach only,
//!    paired ⇒ `X-ADOS-Key` required and an exact match, an unreadable pairing
//!    file ⇒ refused. Token-bucket rate limits guard the edge: authorized
//!    requests share one read budget, refused ones are charged per peer.
//!
//! Auth and rate limiting are a per-edge middleware: a no-op on the Unix
//! listener, enforcing on the TCP listener. The two public endpoints
//! (`/v1/healthz`, `/v1/openapi.json`) are open on both edges.
//!
//! All reads are read-only WAL connections; the live tail is fed by the
//! writer's broadcast channel, never a DB poll. Neither path can block the
//! single writer.

pub mod aggregate;
pub mod auth;
pub mod export;
pub mod openapi;
pub mod pagination;
pub mod params;
pub mod pool;
pub mod routes;
pub mod rows;
pub mod sse;
pub mod stats;

use std::convert::Infallible;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use hyper::body::Incoming;
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto::Builder as ConnBuilder;
use tokio::net::TcpListener;
use tokio::sync::{broadcast, mpsc, oneshot};
use tower::Service;

use ados_protocol::ipc::OperatorListener;
use ados_protocol::logd::IngestFrame;
use ados_protocol::pairing_posture::{classify_caller, CallerClass};

use crate::writer::ControlMsg;

use self::auth::{PairingState, PeerRateLimiter, RateLimiter};
use self::routes::AppState;
use self::sse::{ExportSlots, TailSlots};
use crate::ingest::IngestStats;

/// The mark-synced path. A single const shared by the router registration and
/// the TCP-edge gate that forbids it off the trusted socket.
pub const SYNCED_PATH: &str = "/v1/synced";

/// Per-edge auth state attached to the TCP layer. The Unix listener does not
/// install the layer at all, so on-box callers are never gated.
#[derive(Clone)]
struct EdgeAuth {
    pairing: Arc<PairingState>,
    /// The shared read budget, charged only by authorized requests.
    rate: Arc<RateLimiter>,
    /// The per-peer budget charged by requests authentication refused.
    unauth_rate: Arc<PeerRateLimiter>,
}

/// Where a LAN-edge request came from: the TCP peer and the local address it
/// reached. Attached to every request on the TCP listener so the auth layer can
/// classify the caller; the Unix listener attaches none.
#[derive(Clone, Copy, Debug)]
struct TcpPeer {
    peer: std::net::IpAddr,
    local: Option<std::net::IpAddr>,
}

/// Build the `/v1` Router for a given app state. The same Router is served on
/// both edges; the auth/rate-limit layer is added per edge by the caller.
fn build_router(state: AppState) -> Router {
    Router::new()
        .route("/v1/query", get(routes::query))
        .route("/v1/tail", get(routes::tail))
        .route("/v1/aggregate", get(routes::aggregate))
        .route("/v1/export", get(routes::export))
        .route("/v1/sessions", get(routes::sessions))
        .route("/v1/stats", get(routes::stats))
        .route(SYNCED_PATH, post(routes::synced))
        .route("/v1/healthz", get(routes::healthz))
        .route("/v1/openapi.json", get(routes::openapi))
        .fallback(not_found)
        .with_state(state)
}

/// The 404 handler returns the same error envelope shape as the rest of the API.
async fn not_found() -> Response {
    let body = serde_json::json!({
        "error": { "code": "not_found", "message": "no such path" }
    });
    (StatusCode::NOT_FOUND, Json(body)).into_response()
}

/// The TCP-edge middleware: local-only refusal, public-path bypass, then
/// authentication, then the rate limit of whichever side of auth the request
/// landed on. The Unix edge does not mount this, so trusted on-box callers
/// bypass all of it.
///
/// Authentication runs *before* the rate limit so the two budgets never mix: a
/// refused request is charged to its peer's own bucket and answered 401 (or 429
/// once that peer's bucket is spent), and never touches the shared budget an
/// authorized caller draws from. Rate-limiting first let an unauthenticated
/// flood drain that shared bucket and lock out the keyed operator. The pairing
/// read behind the auth check is a short-TTL cache, so running it per request
/// costs a lock and a compare, not a file read.
async fn tcp_edge(State(edge): State<EdgeAuth>, request: Request, next: Next) -> Response {
    let path = request.uri().path().to_string();
    // The mark-synced write is reachable ONLY on the trusted local socket: the
    // LAN edge can never mutate the store. Reject it here before the public-path
    // bypass and the rate limiter, regardless of pairing, so a key never unlocks
    // a write over TCP.
    if request.method() == axum::http::Method::POST && path == SYNCED_PATH {
        let body = serde_json::json!({
            "error": {
                "code": "local_only",
                "message": "this endpoint is reachable only on the local trusted socket"
            }
        });
        return (StatusCode::FORBIDDEN, Json(body)).into_response();
    }
    // Liveness and discovery are public and must always answer: a watchdog or
    // reachability probe must never be starved by a query flood, so the public
    // paths skip both the rate limiter and the auth check.
    if auth::is_public(&path) {
        return next.run(request).await;
    }
    let presented = request
        .headers()
        .get("X-ADOS-Key")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    // Who is calling, from the accepted socket and the forwarding headers. A
    // request with no recorded peer cannot be placed and is treated as remote.
    let peer = request.extensions().get::<TcpPeer>().copied();
    let caller = match peer {
        Some(p) => classify_caller(Some(p.peer), p.local, |h| request.headers().contains_key(h)),
        None => CallerClass::Remote,
    };
    if !edge.pairing.authorize(&path, caller, presented.as_deref()) {
        // A request with no recorded peer shares one bucket under the
        // unspecified address rather than escaping the per-peer limit.
        let ip = peer.map_or(std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED), |p| {
            p.peer
        });
        if !edge.unauth_rate.check(ip) {
            return rate_limited();
        }
        let body = serde_json::json!({
            "error": {
                "code": "unauthorized",
                "message": "missing or invalid X-ADOS-Key, or a caller an unpaired node does not serve"
            }
        });
        return (StatusCode::UNAUTHORIZED, Json(body)).into_response();
    }
    if !edge.rate.check() {
        return rate_limited();
    }
    next.run(request).await
}

/// The 429 envelope both TCP-edge budgets answer with once spent.
fn rate_limited() -> Response {
    let body = serde_json::json!({
        "error": { "code": "rate_limited", "message": "read budget exceeded; slow down" }
    });
    (StatusCode::TOO_MANY_REQUESTS, Json(body)).into_response()
}

/// Spawn the query server: bind both listeners, serve the shared Router on each,
/// and run until `shutdown` resolves. The Unix socket is created with `0o660`
/// and unlinked on the way out; the TCP listener binds the LAN port.
///
/// `db_path` is opened read-only per request, never read-write. `broadcast` is
/// the writer's sender clone; `ingest` are the live counters; `pairing_path`
/// points at the agent's `pairing.json` for the TCP-edge auth; `mark_synced` is
/// the writer's control sender the on-socket mark path enqueues on;
/// `writer_health` is the writer's progress stamp, which is what makes
/// `/v1/healthz` able to report a dead writer instead of assuming a live one.
// This is a wiring seam: each argument is an independently-owned resource handed
// in by the daemon, so bundling them into a struct adds indirection without
// buying clarity. Matches the `query_sessions` precedent in `rows.rs`.
#[allow(clippy::too_many_arguments)]
pub async fn spawn_query_server<F>(
    db_path: PathBuf,
    query_socket: PathBuf,
    tcp_port: u16,
    broadcast: broadcast::Sender<IngestFrame>,
    ingest: Arc<IngestStats>,
    pairing_path: PathBuf,
    mark_synced: mpsc::Sender<ControlMsg>,
    writer_health: crate::writer::WriterHealth,
    shutdown: F,
) -> Result<()>
where
    F: Future<Output = ()> + Send + 'static,
{
    let pairing = Arc::new(PairingState::with_path(pairing_path));
    // The structural check runs in the background on a fixed cadence, never on
    // a request; the handlers report its latest verdict.
    let integrity = self::stats::IntegrityVerdict::new();
    // The shared pool of warm read-only connections the handlers check out from.
    // The export path opens its own connection on its dedicated thread, so it
    // does not draw from (or starve) this pool.
    let pool = self::pool::ConnPool::new(db_path.clone(), self::pool::DEFAULT_MAX_IDLE);
    let state = AppState {
        db_path: db_path.clone(),
        pool,
        broadcast,
        ingest,
        tail_slots: Arc::new(TailSlots::default()),
        export_slots: Arc::new(ExportSlots::default()),
        mark_synced,
        writer_health,
        integrity: integrity.clone(),
    };

    // The Unix edge: the bare Router, no auth.
    let unix_app = build_router(state.clone());

    // The TCP edge: the same Router wrapped with the auth + rate-limit layer.
    let edge = EdgeAuth {
        pairing,
        rate: Arc::new(RateLimiter::default_read()),
        unauth_rate: Arc::new(PeerRateLimiter::default_unauth()),
    };
    let tcp_app = build_router(state).layer(middleware::from_fn_with_state(edge, tcp_edge));

    // The on-box Unix query socket is the PRIMARY, always-required read plane
    // (`ados logs query` prefers it, and it works with the network down). Bind it
    // up front and hard-fail if it cannot bind.
    let unix_listener = bind_unix(&query_socket)
        .with_context(|| format!("bind query socket {}", query_socket.display()))?;

    // The LAN TCP edge is an OPTIONAL convenience for off-box `ados logs query`.
    // A TCP bind failure — most commonly the port already taken by an unrelated
    // local app (e.g. another service that grabbed this port) — must NOT take the
    // Unix plane down with it: coupling them means one port collision silently
    // kills ALL local log queries while the daemon is up, exactly the Rule-44
    // lying surface where `ados logs` says "is ados-logd running?" and it is.
    // Bind it best-effort: warn and serve Unix-only on failure.
    let tcp_listener = match TcpListener::bind(("0.0.0.0", tcp_port)).await {
        Ok(l) => Some(l),
        Err(e) => {
            tracing::warn!(
                tcp_port,
                error = %e,
                "query LAN TCP port unavailable; serving the on-box Unix query socket only"
            );
            None
        }
    };
    tracing::info!(
        socket = %query_socket.display(),
        tcp_port = tcp_listener.as_ref().map(|_| tcp_port),
        "query API listening"
    );
    let integrity_task = tokio::spawn(self::stats::run_integrity_refresh(db_path, integrity));

    // Two graceful-shutdown signals fan out from the single shutdown future.
    let (unix_stop_tx, unix_stop_rx) = oneshot::channel::<()>();
    let (tcp_stop_tx, tcp_stop_rx) = oneshot::channel::<()>();
    tokio::spawn(async move {
        shutdown.await;
        let _ = unix_stop_tx.send(());
        let _ = tcp_stop_tx.send(());
    });

    let unix = tokio::spawn(serve_unix(unix_listener, unix_app, unix_stop_rx));
    // Only serve the TCP edge when it actually bound; when it did not, the stop
    // receiver is dropped with the closure and its unused sender send is ignored.
    let tcp = tcp_listener.map(|l| tokio::spawn(serve_tcp(l, tcp_app, tcp_stop_rx)));

    let _ = unix.await;
    if let Some(tcp) = tcp {
        let _ = tcp.await;
    }
    integrity_task.abort();

    // tmpfs cleanup: a stale socket path confuses a probing reader on restart.
    let _ = std::fs::remove_file(&query_socket);
    tracing::info!("query API stopped");
    Ok(())
}

/// Bind the Unix listener through the shared command-plane helper: stale socket
/// removed, mode `0o660`, group `ados-operator`, and a peer-credential check on
/// every accept.
fn bind_unix(path: &Path) -> std::io::Result<OperatorListener> {
    ados_protocol::ipc::bind_command_socket(path, 0o660)
}

/// Serve the Router on the Unix listener: accept connections and hand each to
/// hyper with the axum service, until the stop signal fires. Each connection is
/// driven on its own task so one slow client cannot stall the accept loop.
async fn serve_unix(listener: OperatorListener, app: Router, stop: oneshot::Receiver<()>) {
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
                        tracing::warn!(error = %e, "query unix accept failed");
                        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                    }
                }
            }
        }
    }
}

/// Serve the Router on the TCP listener, mirroring the unix accept loop. Each
/// request carries the accepted peer and local address, which the edge auth
/// classifies the caller from alongside the forwarding headers.
async fn serve_tcp(listener: TcpListener, app: Router, stop: oneshot::Receiver<()>) {
    tokio::pin!(stop);
    loop {
        tokio::select! {
            _ = &mut stop => break,
            accepted = listener.accept() => {
                match accepted {
                    Ok((stream, peer)) => {
                        let app = app.clone();
                        let tcp_peer = TcpPeer {
                            peer: peer.ip(),
                            local: stream.local_addr().ok().map(|a| a.ip()),
                        };
                        tokio::spawn(serve_conn(TokioIo::new(stream), app, Some(tcp_peer)));
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "query tcp accept failed");
                        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                    }
                }
            }
        }
    }
}

/// Drive one accepted connection through hyper with the axum service. Generic
/// over the IO so the same code serves a Unix stream and a TCP stream; a TCP
/// connection's peer is attached to each of its requests.
async fn serve_conn<I>(io: TokioIo<I>, app: Router, tcp_peer: Option<TcpPeer>)
where
    I: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    // Bridge the axum Router (a tower Service over axum's Request) to hyper's
    // service over `Incoming` request bodies.
    let svc = hyper::service::service_fn(move |req: hyper::Request<Incoming>| {
        let mut app = app.clone();
        async move {
            let mut req = req.map(Body::new);
            if let Some(p) = tcp_peer {
                req.extensions_mut().insert(p);
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
        tracing::debug!(error = %e, "query connection ended");
    }
}

#[cfg(test)]
mod edge_tests;

#[cfg(test)]
mod tests;
