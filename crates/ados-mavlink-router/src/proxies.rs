//! Direct GCS transport proxies.
//!
//! Mirror the Python TCP (`tcp_proxy.py`, port 5760), UDP (`udp_proxy.py`,
//! ports 14550/14551), and WebSocket (port 8765) proxies: a GCS connects
//! directly and exchanges raw MAVLink frames with the FC, bypassing the cloud
//! relay. Each proxy relays the FC frame stream (via [`FcConnection::subscribe`])
//! out to its clients and forwards client bytes back to the FC with
//! [`FcConnection::send_bytes`]. The WebSocket proxy ([`run_ws_proxy`]) carries
//! MAVLink in binary WebSocket frames, the way a browser GCS connects.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, UdpSocket};
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::{Mutex, Notify};
use tokio_tungstenite::tungstenite::Message;

use crate::connection::FcConnection;

/// How long a learned UDP peer survives without an inbound datagram before it
/// is evicted from the fan-out set. UDP is connectionless, so a GCS that roams
/// to a new ephemeral source port or stops listening leaves no close signal; a
/// peer that has gone quiet for longer than this window is treated as gone.
/// Sized at roughly twice a typical 1 Hz GCS heartbeat plus slack so a present
/// client is never dropped between heartbeats.
const UDP_PEER_TTL: Duration = Duration::from_secs(12);

/// Hard backstop on the number of tracked UDP peers. If the map ever exceeds
/// this many entries (e.g. an aggressive scanner spraying source ports faster
/// than the TTL evicts them) the least-recently-seen entries are dropped so the
/// set cannot grow without bound.
const UDP_MAX_PEERS: usize = 64;

/// TCP MAVLink proxy. Binds `0.0.0.0:<port>` and serves each client a copy of
/// the FC frame stream while forwarding its bytes to the FC.
pub async fn run_tcp_proxy(fc: Arc<FcConnection>, port: u16, cancel: Arc<Notify>) {
    let listener = match TcpListener::bind(("0.0.0.0", port)).await {
        Ok(l) => l,
        Err(e) => {
            tracing::warn!(port, error = %e, "tcp_proxy_bind_failed");
            return;
        }
    };
    tracing::info!(port, "tcp_proxy_listening");
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                if let Ok((stream, _addr)) = accepted {
                    tokio::spawn(handle_tcp_client(fc.clone(), stream));
                }
            }
            _ = cancel.notified() => return,
        }
    }
}

async fn handle_tcp_client(fc: Arc<FcConnection>, stream: tokio::net::TcpStream) {
    let (mut rd, mut wr) = stream.into_split();
    let mut rx = fc.subscribe();

    // FC -> client.
    let writer = tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(frame) => {
                    if wr.write_all(&frame).await.is_err() {
                        break;
                    }
                }
                Err(RecvError::Lagged(_)) => continue,
                Err(RecvError::Closed) => break,
            }
        }
    });

    // client -> FC.
    let mut buf = [0u8; 4096];
    loop {
        match rd.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => fc.send_bytes(&buf[..n]).await,
        }
    }
    writer.abort();
}

/// UDP MAVLink proxy. Binds `0.0.0.0:<port>`, learns each GCS peer from its
/// inbound datagrams, forwards peer bytes to the FC, and sends FC frames to
/// every learned peer.
pub async fn run_udp_proxy(fc: Arc<FcConnection>, port: u16, cancel: Arc<Notify>) {
    let sock = match UdpSocket::bind(("0.0.0.0", port)).await {
        Ok(s) => Arc::new(s),
        Err(e) => {
            tracing::warn!(port, error = %e, "udp_proxy_bind_failed");
            return;
        }
    };
    tracing::info!(port, "udp_proxy_listening");
    // Tracks each learned GCS peer and when it was last heard from. Stale peers
    // are evicted on a TTL so a connectionless UDP fan-out never grows without
    // bound or wastes air-side bandwidth on a peer that has gone away.
    let peers: Arc<Mutex<HashMap<SocketAddr, Instant>>> = Arc::new(Mutex::new(HashMap::new()));

    // FC -> peers.
    let send_sock = sock.clone();
    let send_peers = peers.clone();
    let mut rx = fc.subscribe();
    let sender = tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(frame) => {
                    // Evict peers that have gone quiet, then fan the frame out
                    // only to the survivors.
                    let targets: Vec<SocketAddr> = {
                        let mut map = send_peers.lock().await;
                        evict_stale_peers(&mut map, Instant::now());
                        map.keys().copied().collect()
                    };
                    for peer in targets {
                        let _ = send_sock.send_to(&frame, peer).await;
                    }
                }
                Err(RecvError::Lagged(_)) => continue,
                Err(RecvError::Closed) => break,
            }
        }
    });

    // peers -> FC.
    let mut buf = [0u8; 4096];
    loop {
        tokio::select! {
            recv = sock.recv_from(&mut buf) => {
                if let Ok((n, addr)) = recv {
                    {
                        let mut map = peers.lock().await;
                        map.insert(addr, Instant::now());
                        // Backstop: if the set has somehow grown past the cap,
                        // drop the least-recently-seen entries.
                        cap_peers(&mut map, UDP_MAX_PEERS);
                    }
                    fc.send_bytes(&buf[..n]).await;
                }
            }
            _ = cancel.notified() => {
                sender.abort();
                return;
            }
        }
    }
}

/// Remove peers not heard from within [`UDP_PEER_TTL`] of `now`.
fn evict_stale_peers(peers: &mut HashMap<SocketAddr, Instant>, now: Instant) {
    peers.retain(|_, last_seen| now.duration_since(*last_seen) <= UDP_PEER_TTL);
}

/// Hard backstop: while the map exceeds `max`, drop the least-recently-seen
/// peer. Bounds the set even if peers churn faster than the TTL can evict them.
fn cap_peers(peers: &mut HashMap<SocketAddr, Instant>, max: usize) {
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

/// WebSocket MAVLink proxy. Binds `0.0.0.0:<port>` and bridges binary WebSocket
/// frames to/from the FC, the way a browser GCS connects. Text/ping frames are
/// ignored; only binary frames carry MAVLink.
pub async fn run_ws_proxy(fc: Arc<FcConnection>, port: u16, cancel: Arc<Notify>) {
    let listener = match TcpListener::bind(("0.0.0.0", port)).await {
        Ok(l) => l,
        Err(e) => {
            tracing::warn!(port, error = %e, "ws_proxy_bind_failed");
            return;
        }
    };
    tracing::info!(port, "ws_proxy_listening");
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                if let Ok((stream, _addr)) = accepted {
                    tokio::spawn(handle_ws_client(fc.clone(), stream));
                }
            }
            _ = cancel.notified() => return,
        }
    }
}

async fn handle_ws_client(fc: Arc<FcConnection>, stream: tokio::net::TcpStream) {
    let ws = match tokio_tungstenite::accept_async(stream).await {
        Ok(w) => w,
        Err(_) => return,
    };
    let (mut write, mut read) = ws.split();
    let mut rx = fc.subscribe();

    // FC -> client (binary frames).
    let writer = tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(frame) => {
                    if write.send(Message::Binary(frame)).await.is_err() {
                        break;
                    }
                }
                Err(RecvError::Lagged(_)) => continue,
                Err(RecvError::Closed) => break,
            }
        }
    });

    // client -> FC (binary frames only; ignore text/ping/pong).
    while let Some(msg) = read.next().await {
        match msg {
            Ok(Message::Binary(data)) => fc.send_bytes(&data).await,
            Ok(Message::Close(_)) | Err(_) => break,
            _ => {}
        }
    }
    writer.abort();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(n: u16) -> SocketAddr {
        format!("127.0.0.1:{n}").parse().unwrap()
    }

    #[test]
    fn fresh_peers_are_kept_and_stale_peers_are_evicted() {
        let now = Instant::now();
        let mut peers: HashMap<SocketAddr, Instant> = HashMap::new();
        // A peer seen just now and one seen well past the TTL.
        peers.insert(addr(14550), now);
        peers.insert(addr(14551), now - (UDP_PEER_TTL + Duration::from_secs(1)));

        evict_stale_peers(&mut peers, now);

        assert!(peers.contains_key(&addr(14550)), "fresh peer must survive");
        assert!(
            !peers.contains_key(&addr(14551)),
            "stale peer must be evicted"
        );
    }

    #[test]
    fn peer_exactly_at_ttl_boundary_is_kept() {
        let now = Instant::now();
        let mut peers: HashMap<SocketAddr, Instant> = HashMap::new();
        peers.insert(addr(14550), now - UDP_PEER_TTL);
        evict_stale_peers(&mut peers, now);
        assert!(
            peers.contains_key(&addr(14550)),
            "a peer exactly at the TTL boundary is still present"
        );
    }

    #[test]
    fn cap_drops_least_recently_seen_first() {
        let now = Instant::now();
        let mut peers: HashMap<SocketAddr, Instant> = HashMap::new();
        // Three peers with distinct last-seen times; cap to two.
        peers.insert(addr(1), now - Duration::from_secs(3)); // oldest
        peers.insert(addr(2), now - Duration::from_secs(2));
        peers.insert(addr(3), now - Duration::from_secs(1)); // newest

        cap_peers(&mut peers, 2);

        assert_eq!(peers.len(), 2);
        assert!(
            !peers.contains_key(&addr(1)),
            "the least-recently-seen peer is dropped first"
        );
        assert!(peers.contains_key(&addr(2)));
        assert!(peers.contains_key(&addr(3)));
    }

    #[test]
    fn cap_is_a_noop_under_the_limit() {
        let now = Instant::now();
        let mut peers: HashMap<SocketAddr, Instant> = HashMap::new();
        peers.insert(addr(1), now);
        peers.insert(addr(2), now);
        cap_peers(&mut peers, 64);
        assert_eq!(peers.len(), 2);
    }

    #[test]
    fn reinserting_a_peer_refreshes_its_last_seen_so_it_survives_eviction() {
        let now = Instant::now();
        let mut peers: HashMap<SocketAddr, Instant> = HashMap::new();
        // Peer first seen long ago.
        peers.insert(addr(14550), now - (UDP_PEER_TTL + Duration::from_secs(5)));
        // A new datagram refreshes the timestamp (mirrors the recv path).
        peers.insert(addr(14550), now);
        evict_stale_peers(&mut peers, now);
        assert!(
            peers.contains_key(&addr(14550)),
            "a refreshed peer must not be evicted"
        );
    }
}
