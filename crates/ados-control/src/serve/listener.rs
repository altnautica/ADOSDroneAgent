//! Listener binding and the per-connection accept loops for both edges.

use super::*;

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
pub(super) fn bind_one(domain: Domain, port: u16, v6only: bool) -> std::io::Result<TcpListener> {
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
pub(super) async fn serve_conn<I>(io: TokioIo<I>, app: Router, edge: ConnEdge)
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
