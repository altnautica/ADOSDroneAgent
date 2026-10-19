//! The TCP proxy.

use super::*;

/// TCP MAVLink proxy. Binds `<bind_addr>:<port>`, retrying every
/// [`crate::connection::RECONNECT_INTERVAL`] with no cap while the bind fails,
/// and serves each admitted client a copy of the FC frame stream while
/// forwarding its whole frames to the FC. `listening` reads true exactly while
/// the socket is bound.
pub async fn run_tcp_proxy(
    fc: Arc<FcConnection>,
    bind_addr: &str,
    port: u16,
    auth: ProxyAuth,
    listening: ListenFlag,
    cancel: Shutdown,
) {
    let bound = bind_until(
        &cancel,
        || TcpListener::bind((bind_addr, port)),
        |e| tracing::warn!(bind_addr, port, error = %e, "tcp_proxy_bind_failed"),
    )
    .await;
    let Some(listener) = bound else {
        return;
    };
    let _listening = ListeningGuard::new(&listening);
    tracing::info!(bind_addr, port, "tcp_proxy_listening");
    let mut accept_errors = AcceptErrors::default();
    loop {
        tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok((stream, addr)) => {
                    // The peer address is the only thing this socket knows
                    // about its caller: MAVLink is a raw byte stream, so there
                    // is no header to carry a credential and no handshake to
                    // hang one on. So on a paired node an off-box peer is
                    // served only when the operator opened the raw edges to
                    // the LAN (the QGroundControl / Mission Planner path). A
                    // refused connection is dropped before a single byte is
                    // read, so no client bytes ever reach the flight
                    // controller.
                    let local = stream.local_addr().ok().map(|a| a.ip());
                    let Some(origin) = admit_raw_peer(&auth, addr.ip(), local, port, RawEdge::Tcp) else {
                        // `stream` drops here, closing the connection.
                        continue;
                    };
                    tokio::spawn(handle_tcp_client(
                        fc.clone(),
                        stream,
                        origin,
                        auth.clone(),
                        addr.ip(),
                        local,
                    ));
                }
                Err(e) => {
                    accept_errors.on_error(port, &e).await;
                }
            },
            _ = cancel.wait() => return,
        }
    }
}

/// Serve one admitted TCP client until it disconnects or stops passing the
/// admission check, which is re-run every [`edge::AUTH_RECHECK`] so a pair,
/// unpair or key change reaches a session that is already open.
pub(super) async fn handle_tcp_client(
    fc: Arc<FcConnection>,
    stream: tokio::net::TcpStream,
    mut origin: ClientOrigin,
    auth: ProxyAuth,
    peer: std::net::IpAddr,
    local: Option<std::net::IpAddr>,
) {
    let (mut rd, mut wr) = stream.into_split();
    let (mut rx, route_ep) = fc.routes().client_feed("tcp", fc.subscribe());
    let mut raw_rx = fc.subscribe_raw();

    // FC -> client.
    let writer = tokio::spawn(async move {
        loop {
            // Forward whichever lane produces bytes: the MAVLink frame lane for a
            // MAVLink FC, the raw byte lane for an MSP FC. Exactly one lane is
            // populated for a given FC, so there is no duplication.
            let bytes = tokio::select! {
                r = rx.recv() => match r {
                    Ok(frame) => frame.bytes,
                    Err(RecvError::Lagged(_)) => continue,
                    Err(RecvError::Closed) => break,
                },
                r = raw_rx.recv() => match r {
                    Ok(raw) => raw,
                    Err(RecvError::Lagged(_)) => continue,
                    Err(RecvError::Closed) => break,
                },
            };
            if wr.write_all(&bytes).await.is_err() {
                break;
            }
        }
    });

    // client -> FC, whole frames only.
    let mut buf = [0u8; 4096];
    let mut framer = ClientFramer::default();
    let mut recheck = recheck_interval();
    loop {
        tokio::select! {
            read = rd.read(&mut buf) => match read {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    forward_client_bytes(&fc, &mut framer, &buf[..n], origin, route_ep.as_ref())
                        .await
                }
            },
            _ = recheck.tick() => match raw_peer_verdict(&auth, peer, local) {
                (Some(now), _) => origin = now,
                (None, _) => {
                    tracing::warn!(peer = %peer, "tcp_proxy_session_revoked");
                    break;
                }
            },
        }
    }
    writer.abort();
    if let Some(ep) = &route_ep {
        fc.routes().retire(ep);
    }
}

/// The admission re-check cadence, first tick one period from now.
pub(super) fn recheck_interval() -> tokio::time::Interval {
    let mut interval = tokio::time::interval_at(
        tokio::time::Instant::now() + edge::AUTH_RECHECK,
        edge::AUTH_RECHECK,
    );
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    interval
}
