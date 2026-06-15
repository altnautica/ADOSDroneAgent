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
use std::path::PathBuf;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use ados_protocol::pairing_posture::{
    data_plane_access, is_on_box, load_pairing, Access, Pairing, FORWARDED_HEADERS,
};
use futures_util::{SinkExt, StreamExt};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, UdpSocket};
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::{Mutex, Notify};
use tokio_tungstenite::tungstenite::handshake::server::{
    ErrorResponse, Request as HandshakeRequest, Response as HandshakeResponse,
};
use tokio_tungstenite::tungstenite::http::{HeaderValue, StatusCode};
use tokio_tungstenite::tungstenite::Message;

use crate::connection::FcConnection;

/// The data-path auth header a paired GCS presents to the direct WebSocket
/// proxy, matching the HTTP control surface's `X-ADOS-Key`.
const WS_KEY_HEADER: &str = "x-ados-key";

/// How long a loaded pairing state is trusted before `pairing.json` is re-read.
/// Short enough that a pair/unpair is honoured for a new connection within a few
/// seconds, long enough that a connection burst does not stat the file every
/// time. Mirrors the HTTP control surface's pairing TTL.
const PAIRING_TTL: Duration = Duration::from_secs(2);

/// The WebSocket proxy's view of the agent's pairing posture and whether it
/// enforces the data-path auth gate.
///
/// The proxy bridges raw MAVLink to/from the flight controller, so an
/// unauthenticated LAN caller could otherwise inject commands. The gate mirrors
/// the agent's HTTP auth posture: unpaired ⇒ open (LAN presence is the gate),
/// paired + on-box ⇒ open (the local operator already holds shell privilege),
/// paired + off-box ⇒ the stored pairing key is required in `X-ADOS-Key`.
///
/// **Two-stage rollout.** `enforce` defaults off. With `enforce` off the gate
/// is observe-only: an unauthorized connection is logged and STILL admitted, so
/// the default build does not change the data path. A bench session flips the
/// config flag to on, after which an unauthorized off-box connection is
/// rejected at the handshake.
#[derive(Clone)]
pub struct WsProxyAuth {
    enforce: bool,
    pairing_path: PathBuf,
    cache: Arc<StdMutex<PairingCache>>,
}

struct PairingCache {
    loaded: Pairing,
    at: Instant,
    primed: bool,
}

impl WsProxyAuth {
    /// Build an auth context against an explicit pairing-state path.
    pub fn new(enforce: bool, pairing_path: PathBuf) -> Self {
        Self {
            enforce,
            pairing_path,
            cache: Arc::new(StdMutex::new(PairingCache {
                loaded: Pairing::Unpaired,
                at: Instant::now(),
                primed: false,
            })),
        }
    }

    /// Build an auth context against the agent's standard pairing-state path
    /// (overridable via `ADOS_PAIRING_JSON`, the same override the HTTP control
    /// surface honours), with enforcement controlled by the config flag.
    pub fn from_config(enforce: bool) -> Self {
        let path = std::env::var("ADOS_PAIRING_JSON")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("/etc/ados/pairing.json"));
        Self::new(enforce, path)
    }

    /// The current pairing posture, reading `pairing.json` at most once per TTL.
    fn current(&self) -> Pairing {
        let mut cache = self.cache.lock().unwrap_or_else(|p| p.into_inner());
        if cache.primed && cache.at.elapsed() < PAIRING_TTL {
            return cache.loaded.clone();
        }
        let fresh = load_pairing(&self.pairing_path);
        cache.loaded = fresh.clone();
        cache.at = Instant::now();
        cache.primed = true;
        fresh
    }

    /// Resolve the access decision for a connection, given whether the peer is
    /// loopback, whether the handshake carried a proxy-forwarding header, and the
    /// presented key. Pure given the cached posture; the enforce flag is applied
    /// by the caller ([`should_admit`]).
    fn decide(
        &self,
        peer_is_loopback: bool,
        has_forwarding_header: bool,
        presented_key: Option<&str>,
    ) -> Access {
        let on_box = is_on_box(peer_is_loopback, has_forwarding_header);
        data_plane_access(&self.current(), on_box, presented_key)
    }

    /// Whether to admit a connection, honouring the two-stage rollout. Returns
    /// `(admit, access)`: when `enforce` is off an unauthorized posture still
    /// admits (`admit = true`) so the caller can log-only; when `enforce` is on
    /// an unauthorized posture rejects (`admit = false`).
    fn should_admit(
        &self,
        peer_is_loopback: bool,
        has_forwarding_header: bool,
        presented_key: Option<&str>,
    ) -> (bool, Access) {
        let access = self.decide(peer_is_loopback, has_forwarding_header, presented_key);
        let admit = match access {
            Access::Accept => true,
            Access::Unauthorized => !self.enforce,
        };
        (admit, access)
    }
}

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
///
/// `auth` gates the handshake by the agent's pairing posture (see
/// [`WsProxyAuth`]). With enforcement off (the default) the gate is observe-only.
pub async fn run_ws_proxy(
    fc: Arc<FcConnection>,
    port: u16,
    auth: WsProxyAuth,
    cancel: Arc<Notify>,
) {
    let listener = match TcpListener::bind(("0.0.0.0", port)).await {
        Ok(l) => l,
        Err(e) => {
            tracing::warn!(port, error = %e, "ws_proxy_bind_failed");
            return;
        }
    };
    tracing::info!(port, enforce_auth = auth.enforce, "ws_proxy_listening");
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                if let Ok((stream, addr)) = accepted {
                    tokio::spawn(handle_ws_client(fc.clone(), stream, addr, auth.clone()));
                }
            }
            _ = cancel.notified() => return,
        }
    }
}

/// The handshake-inspection result computed in the `accept_hdr_async` callback
/// and acted on after the handshake completes.
struct HandshakeDecision {
    admit: bool,
    access: Access,
}

// The handshake-rejection callback returns the WebSocket library's
// `ErrorResponse` (an `http::Response<Option<String>>`), whose size the callback
// signature fixes; it cannot be boxed without changing the upstream contract, so
// the large-Err lint is allowed for this one site.
#[allow(clippy::result_large_err)]
async fn handle_ws_client(
    fc: Arc<FcConnection>,
    stream: tokio::net::TcpStream,
    peer: SocketAddr,
    auth: WsProxyAuth,
) {
    // Inspect the handshake for the pairing key + any forwarding header, resolve
    // the posture, and (with enforcement on) reject an unauthorized off-box
    // connection at the handshake. With enforcement off the callback admits
    // everything; the log happens after the handshake from the captured decision.
    let decision: Arc<StdMutex<Option<HandshakeDecision>>> = Arc::new(StdMutex::new(None));
    let callback_decision = decision.clone();
    let callback_auth = auth.clone();
    let ws = tokio_tungstenite::accept_hdr_async(
        stream,
        move |req: &HandshakeRequest, response: HandshakeResponse| {
            let presented = req
                .headers()
                .get(WS_KEY_HEADER)
                .and_then(|v| v.to_str().ok());
            let has_forwarding_header = FORWARDED_HEADERS
                .iter()
                .any(|h| req.headers().contains_key(*h));
            let (admit, access) = callback_auth.should_admit(
                peer.ip().is_loopback(),
                has_forwarding_header,
                presented,
            );
            *callback_decision.lock().unwrap_or_else(|p| p.into_inner()) =
                Some(HandshakeDecision { admit, access });
            if admit {
                Ok(response)
            } else {
                let mut err = ErrorResponse::new(Some(
                    "Missing X-ADOS-Key header. This agent is paired and requires authentication."
                        .to_string(),
                ));
                *err.status_mut() = StatusCode::UNAUTHORIZED;
                err.headers_mut()
                    .insert("x-ados-auth", HeaderValue::from_static("required"));
                Err(err)
            }
        },
    )
    .await;

    // Surface the posture decision regardless of admit/reject, so the
    // observe-only stage produces the same audit signal a bench session uses
    // before flipping enforcement on.
    if let Some(d) = decision.lock().unwrap_or_else(|p| p.into_inner()).take() {
        if d.access == Access::Unauthorized {
            tracing::warn!(
                peer = %peer,
                enforce_auth = auth.enforce,
                admitted = d.admit,
                "ws_proxy_unauthorized"
            );
        }
    }

    let ws = match ws {
        Ok(w) => w,
        // A rejected handshake (enforcement on) or a malformed one both land
        // here; the rejection already wrote the 401 response.
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
    use std::io::Write;

    fn addr(n: u16) -> SocketAddr {
        format!("127.0.0.1:{n}").parse().unwrap()
    }

    /// Write a `pairing.json` into `dir` and return its path.
    fn write_pairing(dir: &std::path::Path, body: &str) -> PathBuf {
        let path = dir.join("pairing.json");
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(body.as_bytes()).unwrap();
        path
    }

    /// An auth context over a paired-state file with the given enforcement.
    fn paired_auth(enforce: bool, key: &str) -> (tempfile::TempDir, WsProxyAuth) {
        let dir = tempfile::tempdir().unwrap();
        let path = write_pairing(
            dir.path(),
            &format!(r#"{{"paired": true, "api_key": "{key}"}}"#),
        );
        (dir, WsProxyAuth::new(enforce, path))
    }

    /// An auth context over an absent (unpaired) state file.
    fn unpaired_auth(enforce: bool) -> (tempfile::TempDir, WsProxyAuth) {
        let dir = tempfile::tempdir().unwrap();
        let auth = WsProxyAuth::new(enforce, dir.path().join("absent.json"));
        (dir, auth)
    }

    // Posture: unpaired admits any caller, regardless of key or peer.
    #[test]
    fn unpaired_admits_off_box_without_a_key() {
        let (_d, auth) = unpaired_auth(true);
        // off-box (not loopback), no forwarding header, no key
        assert_eq!(auth.decide(false, false, None), Access::Accept);
        let (admit, access) = auth.should_admit(false, false, None);
        assert!(admit);
        assert_eq!(access, Access::Accept);
    }

    // Posture: paired + loopback (on-box) admits without a key.
    #[test]
    fn paired_on_box_admits_without_a_key() {
        let (_d, auth) = paired_auth(true, "k");
        assert_eq!(auth.decide(true, false, None), Access::Accept);
    }

    // A loopback peer relayed by a proxy/tunnel is NOT on-box.
    #[test]
    fn paired_loopback_with_forwarding_header_is_not_on_box() {
        let (_d, auth) = paired_auth(true, "k");
        // loopback but a forwarding header present + no key => unauthorized
        assert_eq!(auth.decide(true, true, None), Access::Unauthorized);
    }

    // Posture: paired + off-box + the valid key admits.
    #[test]
    fn paired_off_box_with_a_valid_key_admits() {
        let (_d, auth) = paired_auth(true, "ados_secret");
        assert_eq!(
            auth.decide(false, false, Some("ados_secret")),
            Access::Accept
        );
    }

    // Posture: paired + off-box + no key is unauthorized.
    #[test]
    fn paired_off_box_with_no_key_is_unauthorized() {
        let (_d, auth) = paired_auth(true, "ados_secret");
        assert_eq!(auth.decide(false, false, None), Access::Unauthorized);
    }

    // Posture: paired + off-box + a wrong key is unauthorized.
    #[test]
    fn paired_off_box_with_a_wrong_key_is_unauthorized() {
        let (_d, auth) = paired_auth(true, "ados_secret");
        assert_eq!(
            auth.decide(false, false, Some("wrong")),
            Access::Unauthorized
        );
    }

    // Two-stage rollout: with the enforce flag OFF an unauthorized posture is
    // still ADMITTED (the default build is inert/observe-only).
    #[test]
    fn enforce_off_admits_an_unauthorized_connection_for_log_only() {
        let (_d, auth) = paired_auth(false, "ados_secret");
        let (admit, access) = auth.should_admit(false, false, None);
        assert!(
            admit,
            "with enforcement off the connection is still admitted"
        );
        assert_eq!(
            access,
            Access::Unauthorized,
            "the decision is surfaced as unauthorized so it can be logged"
        );
    }

    // Two-stage rollout: with the enforce flag ON an unauthorized posture is
    // REJECTED.
    #[test]
    fn enforce_on_rejects_an_unauthorized_connection() {
        let (_d, auth) = paired_auth(true, "ados_secret");
        let (admit, access) = auth.should_admit(false, false, None);
        assert!(!admit, "with enforcement on the connection is rejected");
        assert_eq!(access, Access::Unauthorized);
    }

    // An authorized connection is admitted whether or not enforcement is on.
    #[test]
    fn an_authorized_connection_is_admitted_under_either_flag() {
        for enforce in [false, true] {
            let (_d, auth) = paired_auth(enforce, "k");
            let (admit, access) = auth.should_admit(false, false, Some("k"));
            assert!(admit);
            assert_eq!(access, Access::Accept);
        }
    }

    // A pair/unpair that happens while the proxy runs is honoured for a new
    // connection within the TTL once it lapses (the cache re-reads the file).
    #[test]
    fn posture_re_reads_after_the_ttl_lapses() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_pairing(dir.path(), r#"{"paired": false}"#);
        let auth = WsProxyAuth::new(true, path.clone());
        // First read: unpaired => off-box no-key admits.
        assert_eq!(auth.decide(false, false, None), Access::Accept);
        // Now pair and let the TTL lapse so the next read picks it up.
        write_pairing(dir.path(), r#"{"paired": true, "api_key": "k2"}"#);
        {
            let mut c = auth.cache.lock().unwrap();
            c.at = Instant::now() - (PAIRING_TTL + Duration::from_secs(1));
        }
        assert_eq!(auth.decide(false, false, None), Access::Unauthorized);
        assert_eq!(auth.decide(false, false, Some("k2")), Access::Accept);
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
