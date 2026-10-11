//! The UDP proxy and its learned-peer table.

use super::*;

/// How long a learned UDP peer survives without an inbound datagram before it
/// is evicted from the fan-out set. UDP is connectionless, so a GCS that roams
/// to a new ephemeral source port or stops listening leaves no close signal; a
/// peer that has gone quiet for longer than this window is treated as gone.
/// Sized at roughly twice a typical 1 Hz GCS heartbeat plus slack so a present
/// client is never dropped between heartbeats.
pub(super) const UDP_PEER_TTL: Duration = Duration::from_secs(12);

/// Hard backstop on the number of tracked UDP peers. If the map ever exceeds
/// this many entries (e.g. an aggressive scanner spraying source ports faster
/// than the TTL evicts them) the least-recently-seen entries are dropped so the
/// set cannot grow without bound.
pub(super) const UDP_MAX_PEERS: usize = 64;

/// UDP MAVLink proxy. Binds `<bind_addr>:<port>` (retrying like
/// [`run_tcp_proxy`]), learns each admitted GCS peer from its inbound
/// datagrams, forwards peer bytes to the FC, and sends FC frames to every
/// learned peer.
pub async fn run_udp_proxy(
    fc: Arc<FcConnection>,
    bind_addr: &str,
    port: u16,
    auth: ProxyAuth,
    listening: ListenFlag,
    cancel: Shutdown,
) {
    let bound = bind_until(
        &cancel,
        || UdpSocket::bind((bind_addr, port)),
        |e| tracing::warn!(bind_addr, port, error = %e, "udp_proxy_bind_failed"),
    )
    .await;
    let Some(sock) = bound else {
        return;
    };
    let sock = Arc::new(sock);
    let _listening = ListeningGuard::new(&listening);
    tracing::info!(port, "udp_proxy_listening");
    // Each learned GCS peer and when it was last heard from. Stale peers are
    // evicted on a TTL so a connectionless UDP fan-out never grows without
    // bound or wastes air-side bandwidth on a peer that has gone away.
    let peers = Arc::new(UdpPeers::default());

    // FC -> peers. The target list is rebuilt only when the peer set changes,
    // and quiet peers are evicted once a second rather than per frame.
    let send_sock = sock.clone();
    let send_peers = peers.clone();
    let mut rx = crate::routing::PeerFeed::new(fc.routes(), fc.subscribe());
    let peer_eps = crate::routing::PeerEndpoints::new(fc.routes().clone(), &format!("udp{port}"));
    let send_eps = peer_eps.clone();
    let mut raw_rx = fc.subscribe_raw();
    let sender = tokio::spawn(async move {
        let mut targets: Vec<SocketAddr> = Vec::with_capacity(UDP_MAX_PEERS);
        let mut targets_generation = u64::MAX;
        let mut evicted_at = Instant::now();
        loop {
            // Forward whichever lane produces bytes: the MAVLink frame lane for a
            // MAVLink FC, the raw byte lane for an MSP FC. Exactly one lane is
            // populated for a given FC, so there is no duplication.
            let (frame, routed) = tokio::select! {
                r = rx.recv() => match r {
                    Ok(f) => f,
                    Err(RecvError::Lagged(_)) => continue,
                    Err(RecvError::Closed) => break,
                },
                r = raw_rx.recv() => match r {
                    Ok(b) => (b, None),
                    Err(RecvError::Lagged(_)) => continue,
                    Err(RecvError::Closed) => break,
                },
            };
            let now = Instant::now();
            if now.saturating_duration_since(evicted_at) >= edge::UDP_EVICT_EVERY {
                send_peers.evict(now);
                evicted_at = now;
            }
            let generation = send_peers.generation();
            if generation != targets_generation {
                send_peers.fill(&mut targets);
                send_eps.retain(&targets);
                targets_generation = generation;
            }
            for peer in &targets {
                // Route-aware: only the peers this frame is addressed to.
                if let Some(routed) = &routed {
                    if !routed.accepts(&send_eps.get(*peer)) {
                        continue;
                    }
                }
                let _ = send_sock.send_to(&frame, *peer).await;
            }
        }
    });

    // peers -> FC.
    let mut verdicts = UdpVerdicts::new(PAIRING_TTL, UDP_MAX_PEERS);
    let mut buf = [0u8; 4096];
    let recv_eps = peer_eps;
    loop {
        tokio::select! {
            recv = sock.recv_from(&mut buf) => {
                if let Ok((n, addr)) = recv {
                    // UDP has no handshake, so this is the only point at which
                    // the sender is ever considered. An admitted source both
                    // injects bytes into the flight controller AND enrols
                    // itself into the fan-out, so a refused sender skips BOTH
                    // halves. The verdict is cached per source address for the
                    // pairing TTL, so the route lookup and the pairing read run
                    // once per peer per window, and an unauthorized peer is
                    // logged once per minute with a suppressed count.
                    let admission = verdicts.admit(addr, Instant::now(), || {
                        // A wildcard-bound UDP socket has no per-datagram local
                        // address; the route toward the sender names the
                        // interface it sits on.
                        let local = ados_protocol::pairing_posture::local_addr_toward(addr.ip());
                        raw_peer_verdict(&auth, addr.ip(), local)
                    });
                    if let Some(suppressed) = admission.warn {
                        log_unauthorized_raw_peer(
                            port,
                            addr.ip(),
                            admission.origin.is_some(),
                            RawEdge::Udp,
                            suppressed,
                        );
                    }
                    let Some(origin) = admission.origin else {
                        continue;
                    };
                    peers.touch(addr, Instant::now(), UDP_MAX_PEERS);
                    recv_eps.ingress(addr, &buf[..n]);
                    let _ = fc.send_client_bytes(&buf[..n], origin, None).await;
                }
            }
            _ = cancel.wait() => {
                sender.abort();
                return;
            }
        }
    }
}

/// Remove peers not heard from within [`UDP_PEER_TTL`] of `now`.
pub(super) fn evict_stale_peers(peers: &mut HashMap<SocketAddr, Instant>, now: Instant) {
    peers.retain(|_, last_seen| now.duration_since(*last_seen) <= UDP_PEER_TTL);
}

/// Hard backstop: while the map exceeds `max`, drop the least-recently-seen
/// peer. Bounds the set even if peers churn faster than the TTL can evict them.
pub(super) fn cap_peers(peers: &mut HashMap<SocketAddr, Instant>, max: usize) {
    while peers.len() > max {
        // Find the oldest entry by last-seen time and remove it.
        let oldest = peers
            .iter()
            .min_by_key(|(_, last_seen)| **last_seen)
            .map(|(addr, _)| *addr);
        match oldest {
            Some(addr) => {
                peers.remove(&addr);
            }
            None => break,
        }
    }
}
