//! Direct GCS transport proxies.
//!
//! Mirror the Python TCP (`tcp_proxy.py`, port 5760), UDP (`udp_proxy.py`,
//! ports 14550/14551), and WebSocket (port 8765) proxies: a GCS connects
//! directly and exchanges raw MAVLink frames with the FC, bypassing the cloud
//! relay. Each proxy relays the FC frame stream (via [`FcConnection::subscribe`])
//! out to its clients and forwards client bytes back with
//! [`FcConnection::send_client_bytes`] — to the local FC when one is attached,
//! or onto the aux uplink toward a relayed drone when this node is a ground
//! station relaying one instead. The WebSocket proxy ([`run_ws_proxy`]) carries
//! MAVLink in binary WebSocket frames, the way a browser GCS connects.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use ados_protocol::pairing_posture::{
    classify_caller, data_plane_access, load_pairing, Access, CallerClass, Pairing,
};
use ados_protocol::shutdown::Shutdown;
use ados_protocol::ws_ticket::{now_unix, WsTicketIssuer, SCOPE_MAVLINK_WS};
use futures_util::{SinkExt, StreamExt};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, UdpSocket};
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::Mutex;
use tokio_tungstenite::tungstenite::handshake::server::{
    ErrorResponse, Request as HandshakeRequest, Response as HandshakeResponse,
};
use tokio_tungstenite::tungstenite::http::header::SEC_WEBSOCKET_PROTOCOL;
use tokio_tungstenite::tungstenite::http::{HeaderValue, StatusCode};
use tokio_tungstenite::tungstenite::Message;

use crate::connection::{ClientOrigin, FcConnection};

/// The data-path auth header a paired GCS presents to the direct WebSocket
/// proxy, matching the HTTP control surface's `X-ADOS-Key`.
const WS_KEY_HEADER: &str = "x-ados-key";

/// The WebSocket subprotocol marker carrying an auth ticket, for a browser GCS
/// that cannot set `X-ADOS-Key` on the handshake: it dials
/// `new WebSocket(url, ["ados-ws-ticket", <token>])`. The agent echoes the
/// marker back on the accepted handshake per RFC 6455. Matches the GCS
/// `WS_TICKET_PROTOCOL` and the Python `ws_auth.WS_TICKET_PROTOCOL`.
const WS_TICKET_SUBPROTOCOL: &str = "ados-ws-ticket";

/// How long a loaded pairing state is trusted before `pairing.json` is re-read.
/// Short enough that a pair/unpair is honoured for a new connection within a few
/// seconds, long enough that a connection burst does not stat the file every
/// time. Mirrors the HTTP control surface's pairing TTL.
const PAIRING_TTL: Duration = Duration::from_secs(2);

/// The direct-GCS proxies' view of the agent's pairing posture and whether it
/// enforces the data-path auth gate.
///
/// The proxies bridge raw MAVLink to/from the flight controller, so an
/// unauthenticated caller could otherwise inject commands. Each connection's
/// caller is classified once ([`classify_caller`]) and decided by
/// [`direct_access`]:
///
/// - **Unpaired ⇒ the local operator and the first-boot lifelines only.** A
///   fresh node has no key to check, and these edges carry flight-controller
///   bytes, so the rest of the LAN and anything relayed through a proxy or
///   tunnel is unauthorized. The HTTP surface's PIN-gated operator-LAN scope
///   does not extend here: there is no PIN channel on a MAVLink stream.
/// - **Paired + on-box ⇒ open.** The local operator already holds shell
///   privilege.
/// - **Paired + anyone else ⇒ the stored pairing key** (`X-ADOS-Key` or a
///   `gs.mavlink_ws` ticket on the WebSocket; the raw edges have neither).
///
/// **Two defaults, one mechanism.** `enforce` is supplied by the caller from
/// the config, and the edges do not share a value: the WebSocket enforces by
/// default because a client can present either the `X-ADOS-Key` header or an
/// `ados-ws-ticket` subprotocol, while the byte-stream edges (TCP, UDP) default
/// off because they have no credential channel to present anything on. With
/// `enforce` off the gate is observe-only: an unauthorized connection is logged
/// and STILL admitted. With it on, an unauthorized connection is refused at the
/// handshake (WebSocket) or before any bytes are read (TCP, UDP).
///
/// The byte-stream edges add one gate of their own on a PAIRED node: an
/// off-box raw peer can never present the key, so it is served only when the
/// operator opted the raw edges into LAN access
/// ([`WsProxyAuth::with_raw_lan_access`]). Without that opt-in a paired node's
/// raw proxies serve on-box callers only, whatever `enforce` says.
///
/// The byte-stream proxies share this gate with the WebSocket, so the name is
/// an alias rather than a second type: one posture, three edges.
pub type ProxyAuth = WsProxyAuth;

#[derive(Clone)]
pub struct WsProxyAuth {
    enforce: bool,
    raw_lan_access: bool,
    pairing_path: PathBuf,
    cache: Arc<StdMutex<PairingCache>>,
}

struct PairingCache {
    loaded: Pairing,
    at: Instant,
    primed: bool,
}

/// The direct-proxy access rule for one classified caller, given the pairing
/// posture. See [`WsProxyAuth`].
fn direct_access(pairing: &Pairing, caller: CallerClass, presented_key: Option<&str>) -> Access {
    if *pairing == Pairing::Unpaired && !caller.is_first_boot_reach() {
        return Access::Unauthorized;
    }
    data_plane_access(pairing, caller, presented_key)
}

impl WsProxyAuth {
    /// Build an auth context against an explicit pairing-state path. The raw
    /// edges start closed to off-box peers on a paired node; see
    /// [`Self::with_raw_lan_access`].
    pub fn new(enforce: bool, pairing_path: PathBuf) -> Self {
        Self {
            enforce,
            raw_lan_access: false,
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

    /// Open the raw byte-stream edges (TCP, UDP) to off-box peers on a PAIRED
    /// node (`mavlink.raw_proxy_lan_access`). Those edges carry no credential
    /// channel, so on a paired node an off-box peer is always unauthorized;
    /// without this opt-in it is refused, which makes the raw proxies on-box
    /// only once the node is paired. With it, the peer is served unless
    /// `enforce` is also on. Consulted only by [`Self::classify`]; the
    /// WebSocket authenticates its callers instead.
    pub fn with_raw_lan_access(mut self, open: bool) -> Self {
        self.raw_lan_access = open;
        self
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

    /// Resolve the access decision for a classified caller and the presented
    /// key, against the cached posture. The enforce flag is applied by the
    /// callers ([`Self::should_admit`], [`Self::classify`]).
    fn decide(&self, caller: CallerClass, presented_key: Option<&str>) -> Access {
        direct_access(&self.current(), caller, presented_key)
    }

    /// Whether the offered WebSocket subprotocols carry a valid `gs.mavlink_ws`
    /// ticket for the current paired key. A browser cannot set `X-ADOS-Key`, so
    /// it presents a short-lived HMAC ticket (minted by the native control
    /// surface, keyed off the same `pairing.json`) through the subprotocol list.
    /// Always false when unpaired (there is no key to verify against) or when
    /// no ticket is offered.
    fn ticket_valid(&self, offered: &[String]) -> bool {
        let Pairing::Paired(key) = self.current() else {
            return false;
        };
        let Some(token) = extract_ticket(offered) else {
            return false;
        };
        WsTicketIssuer::from_api_key(&key)
            .verify(token, SCOPE_MAVLINK_WS, now_unix())
            .is_ok()
    }

    /// Classify a raw-socket peer by address alone and decide it.
    ///
    /// The byte-stream proxies have no header and no handshake, so the peer
    /// address is the only signal available and there is no key or ticket to
    /// present. Returns `(admit, access)`:
    ///
    /// - an authorized caller (on-box; a lifeline while unpaired) is admitted;
    /// - on a PAIRED node any other caller is admitted only when the raw edges
    ///   were opened to the LAN AND enforcement is off;
    /// - on an UNPAIRED node any other caller is admitted only when
    ///   enforcement is off (observe-only).
    ///
    /// `local` is the address the peer reached (see
    /// [`ados_protocol::pairing_posture::classify_caller`]); a private-LAN peer
    /// is a lifeline only when that is the agent's own AP or USB address.
    /// An unreadable pairing file is treated like a paired node with no key:
    /// only the on-box operator is admitted, whatever the flags say.
    pub fn classify(
        &self,
        peer: std::net::IpAddr,
        local: Option<std::net::IpAddr>,
    ) -> (bool, Access) {
        let pairing = self.current();
        let access = direct_access(
            &pairing,
            classify_caller(Some(peer), local, |_| false),
            None,
        );
        let admit = match (access, &pairing) {
            (Access::Accept, _) => true,
            (Access::Unauthorized, Pairing::Paired(_)) => self.raw_lan_access && !self.enforce,
            (Access::Unauthorized, Pairing::Unpaired) => !self.enforce,
            (Access::Unauthorized, Pairing::Unreadable) => false,
        };
        (admit, access)
    }

    /// Whether to admit a WebSocket connection, honouring the enforce flag.
    /// Returns `(admit, access)`: when `enforce` is off an unauthorized posture
    /// still admits (`admit = true`) so the caller can log-only; when `enforce`
    /// is on an unauthorized posture rejects (`admit = false`).
    ///
    /// A valid ticket in the offered subprotocols promotes an otherwise
    /// unauthorized (paired, not on-box, no/bad key) connection to `Accept` — it
    /// is an off-box credential equivalent to a valid `X-ADOS-Key`. So a browser
    /// GCS (ticket) and a native client (header) both authenticate.
    fn should_admit(
        &self,
        caller: CallerClass,
        presented_key: Option<&str>,
        offered_subprotocols: &[String],
    ) -> (bool, Access) {
        let mut access = self.decide(caller, presented_key);
        if access == Access::Unauthorized && self.ticket_valid(offered_subprotocols) {
            access = Access::Accept;
        }
        let admit = match access {
            Access::Accept => true,
            Access::Unauthorized => !self.enforce,
        };
        (admit, access)
    }

    /// Decide a WebSocket handshake from its peer address and headers: classify
    /// the caller (a forwarding header makes it remote), read the presented key
    /// and the offered subprotocols, then [`Self::should_admit`].
    fn handshake_decision(
        &self,
        peer: std::net::IpAddr,
        local: Option<std::net::IpAddr>,
        headers: &tokio_tungstenite::tungstenite::http::HeaderMap,
    ) -> (bool, Access) {
        let caller = classify_caller(Some(peer), local, |h| headers.contains_key(h));
        let presented = headers.get(WS_KEY_HEADER).and_then(|v| v.to_str().ok());
        self.should_admit(caller, presented, &offered_subprotocols(headers))
    }
}

/// Pull the ticket value that follows the `ados-ws-ticket` marker in the offered
/// subprotocol list (`["ados-ws-ticket", "<token>"]`). The token itself is
/// pipe-delimited and carries no commas, so it survives the comma-split of the
/// `Sec-WebSocket-Protocol` header intact.
fn extract_ticket(offered: &[String]) -> Option<&str> {
    let pos = offered.iter().position(|p| p == WS_TICKET_SUBPROTOCOL)?;
    offered.get(pos + 1).map(String::as_str)
}

/// Parse the offered WebSocket subprotocols from the handshake headers. Values
/// may be split across multiple `Sec-WebSocket-Protocol` headers and/or
/// comma-joined within one; flatten both forms.
fn offered_subprotocols(headers: &tokio_tungstenite::tungstenite::http::HeaderMap) -> Vec<String> {
    headers
        .get_all(SEC_WEBSOCKET_PROTOCOL)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|raw| raw.split(','))
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .collect()
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

/// Map an access decision to the provenance the send path records.
///
/// Deliberately not a permission: the byte path is unchanged either way. It
/// exists so the one fallback that radiates a client's bytes to a remote
/// aircraft can say when the client was anonymous.
fn origin_of(access: Access) -> ClientOrigin {
    match access {
        Access::Accept => ClientOrigin::Trusted,
        Access::Unauthorized => ClientOrigin::Unauthenticated,
    }
}

/// Which byte-stream edge a peer arrived on. Exists only so the refusal log
/// keeps one static event name per edge: an operator greps
/// `tcp_proxy_unauthorized` / `udp_proxy_unauthorized`, so the two must not
/// collapse into one formatted message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RawEdge {
    Tcp,
    Udp,
}

/// The accept-time verdict for one raw byte-stream peer: `Some(origin)` to serve
/// it and stamp that provenance on its bytes, `None` to refuse it outright.
///
/// Shared by both loops rather than written twice. Each loop previously carried
/// its own copy of "classify, log, then serve regardless", and both logged a
/// hardcoded `admitted = true` that outlived the flag it claimed to describe.
/// One decision point means the flag cannot be honoured on one edge and
/// silently dropped on the other.
fn admit_raw_peer(
    auth: &ProxyAuth,
    peer: std::net::IpAddr,
    local: Option<std::net::IpAddr>,
    port: u16,
    edge: RawEdge,
) -> Option<ClientOrigin> {
    let (admit, access) = auth.classify(peer, local);
    if access != Access::Accept {
        match edge {
            RawEdge::Tcp => tracing::warn!(
                port,
                peer = %peer,
                admitted = admit,
                "tcp_proxy_unauthorized"
            ),
            RawEdge::Udp => tracing::warn!(
                port,
                peer = %peer,
                admitted = admit,
                "udp_proxy_unauthorized"
            ),
        }
    }
    admit.then(|| origin_of(access))
}

/// Bind address for the direct-GCS proxies when nothing overrides it.
///
/// Wide on purpose: a fresh unit is reached over its AP hotspot and USB gadget,
/// neither of which is loopback, and an operator who opts the raw edges into
/// LAN access (`mavlink.raw_proxy_lan_access`) needs them on the LAN. Who is
/// actually served is decided per connection by [`WsProxyAuth`], which follows
/// the pairing state with no restart; on a paired node without that opt-in the
/// raw edges serve on-box callers only.
pub const DEFAULT_PROXY_BIND_ADDR: &str = "0.0.0.0";

/// The operator's `ADOS_MAVLINK_BIND_ADDR` override, trimmed; `None` when it is
/// unset or blank (a blank override is a mistake, not a request to bind
/// nowhere).
fn bind_override() -> Option<String> {
    std::env::var("ADOS_MAVLINK_BIND_ADDR")
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// The address the raw direct-GCS proxies (TCP, UDP) bind, from
/// `ADOS_MAVLINK_BIND_ADDR`.
///
/// Settable because the advertisement above is not right for every deployment.
/// A unit that never needs a desktop ground station on its LAN can bind
/// loopback and stop carrying an unauthenticated path to its flight controller
/// at all — a stronger remedy than inspecting callers on a socket that stays
/// open, because there is then nothing left to inspect.
pub fn proxy_bind_addr() -> String {
    bind_override().unwrap_or_else(|| DEFAULT_PROXY_BIND_ADDR.to_string())
}

/// The address the WebSocket proxy binds: the `ADOS_MAVLINK_BIND_ADDR`
/// override when set (it narrows every direct-GCS proxy at once), else the
/// configured WebSocket endpoint's `host`, else the default.
pub fn ws_bind_addr(configured_host: &str) -> String {
    bind_override()
        .or_else(|| Some(configured_host.trim().to_string()).filter(|h| !h.is_empty()))
        .unwrap_or_else(|| DEFAULT_PROXY_BIND_ADDR.to_string())
}

/// TCP MAVLink proxy. Binds `<bind_addr>:<port>` and serves each admitted
/// client a copy of the FC frame stream while forwarding its bytes to the FC.
pub async fn run_tcp_proxy(
    fc: Arc<FcConnection>,
    bind_addr: &str,
    port: u16,
    auth: ProxyAuth,
    cancel: Shutdown,
) {
    let listener = match TcpListener::bind((bind_addr, port)).await {
        Ok(l) => l,
        Err(e) => {
            tracing::warn!(bind_addr, port, error = %e, "tcp_proxy_bind_failed");
            return;
        }
    };
    tracing::info!(bind_addr, port, "tcp_proxy_listening");
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                if let Ok((stream, addr)) = accepted {
                    // The peer address is the only thing this socket knows
                    // about its caller: MAVLink is a raw byte stream, so there
                    // is no header to carry a credential and no handshake to
                    // hang one on. So on a paired node an off-box peer is
                    // served only when the operator opened the raw edges to
                    // the LAN (the QGroundControl / Mission Planner path) and
                    // left enforcement off. A refused connection is dropped
                    // before a single byte is read, so no client bytes ever
                    // reach the flight controller.
                    let Some(origin) = admit_raw_peer(&auth, addr.ip(), stream.local_addr().ok().map(|a| a.ip()), port, RawEdge::Tcp) else {
                        // `stream` drops here, closing the connection.
                        continue;
                    };
                    tokio::spawn(handle_tcp_client(fc.clone(), stream, origin));
                }
            }
            _ = cancel.wait() => return,
        }
    }
}

async fn handle_tcp_client(
    fc: Arc<FcConnection>,
    stream: tokio::net::TcpStream,
    origin: ClientOrigin,
) {
    let (mut rd, mut wr) = stream.into_split();
    let mut rx = fc.subscribe();
    let mut raw_rx = fc.subscribe_raw();

    // FC -> client.
    let writer = tokio::spawn(async move {
        loop {
            // Forward whichever lane produces bytes: the MAVLink frame lane for a
            // MAVLink FC, the raw byte lane for an MSP FC. Exactly one lane is
            // populated for a given FC, so there is no duplication.
            let bytes = tokio::select! {
                r = rx.recv() => match r {
                    Ok(frame) => frame,
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

    // client -> FC.
    let mut buf = [0u8; 4096];
    loop {
        match rd.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => fc.send_client_bytes(&buf[..n], origin, None).await,
        }
    }
    writer.abort();
}

/// UDP MAVLink proxy. Binds `<bind_addr>:<port>`, learns each admitted GCS peer from its
/// inbound datagrams, forwards peer bytes to the FC, and sends FC frames to
/// every learned peer.
pub async fn run_udp_proxy(
    fc: Arc<FcConnection>,
    bind_addr: &str,
    port: u16,
    auth: ProxyAuth,
    cancel: Shutdown,
) {
    let sock = match UdpSocket::bind((bind_addr, port)).await {
        Ok(s) => Arc::new(s),
        Err(e) => {
            tracing::warn!(bind_addr, port, error = %e, "udp_proxy_bind_failed");
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
    let mut raw_rx = fc.subscribe_raw();
    let sender = tokio::spawn(async move {
        loop {
            // Forward whichever lane produces bytes: the MAVLink frame lane for a
            // MAVLink FC, the raw byte lane for an MSP FC. Exactly one lane is
            // populated for a given FC, so there is no duplication.
            let frame = tokio::select! {
                r = rx.recv() => match r {
                    Ok(f) => f,
                    Err(RecvError::Lagged(_)) => continue,
                    Err(RecvError::Closed) => break,
                },
                r = raw_rx.recv() => match r {
                    Ok(b) => b,
                    Err(RecvError::Lagged(_)) => continue,
                    Err(RecvError::Closed) => break,
                },
            };
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
    });

    // peers -> FC.
    let mut buf = [0u8; 4096];
    loop {
        tokio::select! {
            recv = sock.recv_from(&mut buf) => {
                if let Ok((n, addr)) = recv {
                    // UDP has no handshake, so this is the only point at which
                    // the sender is ever considered. Note what the insert below
                    // does: an unrecognised source both injects bytes into the
                    // flight controller AND enrols itself into the fan-out, so
                    // the FC's whole telemetry stream is mirrored back to it.
                    //
                    // The same verdict as TCP. A refused sender skips BOTH
                    // halves: refusing the injection while still enrolling the
                    // peer would leave the telemetry mirror open, which is the
                    // half of this that reaches furthest.
                    // A wildcard-bound UDP socket has no per-datagram local
                    // address; the route toward the sender names the interface
                    // it sits on.
                    let local = ados_protocol::pairing_posture::local_addr_toward(addr.ip());
                    let Some(origin) = admit_raw_peer(&auth, addr.ip(), local, port, RawEdge::Udp) else {
                        continue;
                    };
                    {
                        let mut map = peers.lock().await;
                        map.insert(addr, Instant::now());
                        // Backstop: if the set has somehow grown past the cap,
                        // drop the least-recently-seen entries.
                        cap_peers(&mut map, UDP_MAX_PEERS);
                    }
                    fc.send_client_bytes(&buf[..n], origin, None).await;
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

/// WebSocket MAVLink proxy. Binds `<bind_addr>:<port>` ([`ws_bind_addr`]) and
/// bridges binary WebSocket frames to/from the FC, the way a browser GCS
/// connects. Text/ping frames are ignored; only binary frames carry MAVLink.
///
/// `auth` gates the handshake by the agent's pairing posture (see
/// [`WsProxyAuth`]).
pub async fn run_ws_proxy(
    fc: Arc<FcConnection>,
    bind_addr: &str,
    port: u16,
    auth: WsProxyAuth,
    cancel: Shutdown,
) {
    let listener = match TcpListener::bind((bind_addr, port)).await {
        Ok(l) => l,
        Err(e) => {
            tracing::warn!(bind_addr, port, error = %e, "ws_proxy_bind_failed");
            return;
        }
    };
    tracing::info!(
        bind_addr,
        port,
        enforce_auth = auth.enforce,
        "ws_proxy_listening"
    );
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                if let Ok((stream, addr)) = accepted {
                    tokio::spawn(handle_ws_client(fc.clone(), stream, addr, auth.clone()));
                }
            }
            _ = cancel.wait() => return,
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
    // Classify the caller from the peer and the handshake headers (a
    // forwarding header makes it remote), resolve the posture, and (with
    // enforcement on) reject an unauthorized connection at the handshake. With
    // enforcement off the callback admits everything; the log happens after the
    // handshake from the captured decision.
    let local = stream.local_addr().ok().map(|a| a.ip());
    let decision: Arc<StdMutex<Option<HandshakeDecision>>> = Arc::new(StdMutex::new(None));
    let callback_decision = decision.clone();
    let callback_auth = auth.clone();
    let ws = tokio_tungstenite::accept_hdr_async(
        stream,
        move |req: &HandshakeRequest, mut response: HandshakeResponse| {
            let (admit, access) = callback_auth.handshake_decision(peer.ip(), local, req.headers());
            *callback_decision.lock().unwrap_or_else(|p| p.into_inner()) =
                Some(HandshakeDecision { admit, access });
            if admit {
                // If the client offered the ticket subprotocol, the server MUST
                // select one of the offered subprotocols (RFC 6455 §4.2.2) or a
                // browser handshake fails — echo the marker back. We never echo
                // the token itself, only the marker.
                if offered_subprotocols(req.headers())
                    .iter()
                    .any(|p| p == WS_TICKET_SUBPROTOCOL)
                {
                    response.headers_mut().insert(
                        SEC_WEBSOCKET_PROTOCOL,
                        HeaderValue::from_static(WS_TICKET_SUBPROTOCOL),
                    );
                }
                Ok(response)
            } else if callback_auth.current() == Pairing::Unpaired {
                // The same refusal the HTTP edge gives an unpaired node's
                // non-lifeline caller: there is no key it could have sent.
                let mut err = ErrorResponse::new(Some(
                    "This device is not paired yet. Pair it first, or reach it over its hotspot or USB connection."
                        .to_string(),
                ));
                *err.status_mut() = StatusCode::FORBIDDEN;
                Err(err)
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
    // Carried into the send path so the relay fallback can record when the
    // bytes it radiates came from a caller that did not clear the gate.
    let mut ws_origin = ClientOrigin::Trusted;
    if let Some(d) = decision.lock().unwrap_or_else(|p| p.into_inner()).take() {
        ws_origin = origin_of(d.access);
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
    let mut raw_rx = fc.subscribe_raw();

    // FC -> client (binary frames).
    let writer = tokio::spawn(async move {
        loop {
            // Forward whichever lane produces bytes: the MAVLink frame lane for a
            // MAVLink FC, the raw byte lane for an MSP FC. Exactly one lane is
            // populated for a given FC, so there is no duplication.
            let bytes = tokio::select! {
                r = rx.recv() => match r {
                    Ok(frame) => frame,
                    Err(RecvError::Lagged(_)) => continue,
                    Err(RecvError::Closed) => break,
                },
                r = raw_rx.recv() => match r {
                    Ok(raw) => raw,
                    Err(RecvError::Lagged(_)) => continue,
                    Err(RecvError::Closed) => break,
                },
            };
            // tungstenite 0.24's `Binary` owns a `Vec`, so this lane pays one
            // copy per frame per WebSocket client — exactly what it paid before,
            // when the fan-out handed every consumer its own `Vec`. Every other
            // consumer now avoids that copy.
            if write.send(Message::Binary(bytes.to_vec())).await.is_err() {
                break;
            }
        }
    });

    // client -> FC (binary frames only; ignore text/ping/pong).
    while let Some(msg) = read.next().await {
        match msg {
            Ok(Message::Binary(data)) => fc.send_client_bytes(&data, ws_origin, None).await,
            Ok(Message::Close(_)) | Err(_) => break,
            _ => {}
        }
    }
    writer.abort();
}

#[cfg(test)]
mod tests {

    // --- raw-socket posture (TCP / UDP), which have no handshake ------------

    #[test]
    fn the_bind_default_stays_wide_for_the_lifelines() {
        // A fresh unit's AP and USB lifelines are not loopback, so the listener
        // binds wide and the per-connection gate decides who is served.
        assert_eq!(DEFAULT_PROXY_BIND_ADDR, "0.0.0.0");
    }

    /// Serialises the env mutation below; the house pattern from the uplink
    /// consumer's tests.
    static BIND_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn an_operator_can_narrow_the_bind_and_blank_values_do_not_count() {
        // A unit with no desktop ground station on its LAN can remove the
        // unauthenticated path entirely rather than inspect callers on a socket
        // that stays open. A blank override is a mistake, not a request to bind
        // nowhere, so it falls back rather than failing to bind at all.
        let _guard = BIND_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let restore = std::env::var("ADOS_MAVLINK_BIND_ADDR").ok();

        std::env::set_var("ADOS_MAVLINK_BIND_ADDR", "127.0.0.1");
        assert_eq!(proxy_bind_addr(), "127.0.0.1");

        std::env::set_var("ADOS_MAVLINK_BIND_ADDR", "  10.0.0.5  ");
        assert_eq!(
            proxy_bind_addr(),
            "10.0.0.5",
            "surrounding space is trimmed"
        );

        for blank in ["", "   "] {
            std::env::set_var("ADOS_MAVLINK_BIND_ADDR", blank);
            assert_eq!(
                proxy_bind_addr(),
                DEFAULT_PROXY_BIND_ADDR,
                "a blank override must not bind nowhere"
            );
        }

        std::env::remove_var("ADOS_MAVLINK_BIND_ADDR");
        assert_eq!(proxy_bind_addr(), DEFAULT_PROXY_BIND_ADDR);

        // The WebSocket follows its configured endpoint host, and the operator
        // override narrows it along with the raw edges.
        assert_eq!(ws_bind_addr("127.0.0.1"), "127.0.0.1");
        assert_eq!(ws_bind_addr("  "), DEFAULT_PROXY_BIND_ADDR);
        std::env::set_var("ADOS_MAVLINK_BIND_ADDR", "127.0.0.1");
        assert_eq!(ws_bind_addr("0.0.0.0"), "127.0.0.1");

        match restore {
            Some(v) => std::env::set_var("ADOS_MAVLINK_BIND_ADDR", v),
            None => std::env::remove_var("ADOS_MAVLINK_BIND_ADDR"),
        }
    }

    #[test]
    fn an_unpaired_node_still_answers_its_own_lifelines() {
        // A headless node's only two operator routes are the first-boot AP and
        // the USB gadget network, and NEITHER is loopback or link-local. Losing
        // them would leave a fresh unit with no way in at all.
        let (_dir, auth) = unpaired_auth(false);
        for ip in ["127.0.0.1", "192.168.4.20", "192.168.7.2", "169.254.3.4"] {
            let (admit, access) = auth.classify(ip.parse().unwrap(), lifeline_local(ip));
            assert_eq!(access, Access::Accept, "{ip} is a lifeline");
            assert!(admit);
        }
    }

    /// The local address a peer on the AP or USB subnet reaches: the agent's
    /// own gateway address there. `None` for every other peer.
    fn lifeline_local(ip: &str) -> Option<std::net::IpAddr> {
        if ip.starts_with("192.168.4.") {
            Some("192.168.4.1".parse().unwrap())
        } else if ip.starts_with("192.168.7.") {
            Some("192.168.7.1".parse().unwrap())
        } else {
            None
        }
    }

    /// A LAN numbered like the AP subnet, reached on the node's own DHCP lease
    /// rather than the AP gateway, is not a lifeline.
    #[test]
    fn an_ap_numbered_lan_is_not_a_lifeline_off_the_ap_address() {
        let (_dir, auth) = unpaired_auth(true);
        let (admit, access) = auth.classify(
            "192.168.4.20".parse().unwrap(),
            Some("192.168.4.57".parse().unwrap()),
        );
        assert_eq!(access, Access::Unauthorized);
        assert!(!admit);
    }

    /// An unreadable pairing file admits only the local operator, even with the
    /// raw edges opened to the LAN and enforcement off.
    #[test]
    fn an_unreadable_pairing_file_admits_only_the_local_operator() {
        let dir = tempfile::tempdir().unwrap();
        let pairing = write_pairing(dir.path(), "not json");
        let auth = ProxyAuth::new(false, pairing).with_raw_lan_access(true);
        assert!(!auth.classify("192.168.1.50".parse().unwrap(), None).0);
        assert!(
            !auth
                .classify(
                    "192.168.4.20".parse().unwrap(),
                    lifeline_local("192.168.4.20")
                )
                .0
        );
        assert!(auth.classify("127.0.0.1".parse().unwrap(), None).0);
    }

    #[test]
    fn an_unpaired_node_treats_the_wider_lan_as_unauthorized() {
        // `data_plane_access` alone says Unpaired => Accept, i.e. an unpaired
        // node accepts flight-controller bytes from the whole LAN. The
        // allowlist is what narrows that to the routes an operator actually
        // has.
        let (_dir, auth) = unpaired_auth(false);
        for ip in ["10.0.0.9", "172.16.4.4", "192.168.1.50", "8.8.8.8"] {
            let (_admit, access) = auth.classify(ip.parse().unwrap(), None);
            assert_eq!(access, Access::Unauthorized, "{ip} is not a lifeline");
        }
    }

    #[test]
    fn observation_admits_everything_it_flags() {
        // The whole point of the first stage: this port is advertised as the
        // third-party GCS path, so refusing a caller would break it at the
        // operator's screen. Flagging must not change the data path.
        let (_dir, auth) = unpaired_auth(false);
        let (admit, access) = auth.classify("10.0.0.9".parse().unwrap(), None);
        assert_eq!(access, Access::Unauthorized);
        assert!(admit, "observe-only must still admit");
    }

    #[test]
    fn enforcing_refuses_the_same_peer_it_would_have_flagged() {
        // Proves the second stage is a flag flip rather than new logic, so the
        // observation gathered now describes exactly what enforcement will do.
        let (_dir, auth) = unpaired_auth(true);
        let (admit, access) = auth.classify("10.0.0.9".parse().unwrap(), None);
        assert_eq!(access, Access::Unauthorized);
        assert!(!admit);
        // A lifeline is still admitted under enforcement.
        assert!(
            auth.classify(
                "192.168.4.20".parse().unwrap(),
                lifeline_local("192.168.4.20")
            )
            .0
        );
    }

    #[test]
    fn a_raw_socket_peer_can_present_no_key_and_no_ticket() {
        // MAVLink is a byte stream: there is no header and no subprotocol, so
        // address is the only signal. This pins that classify never claims
        // otherwise — a paired node cannot admit an off-box raw peer.
        let dir = tempfile::tempdir().unwrap();
        let pairing = dir.path().join("pairing.json");
        std::fs::write(&pairing, r#"{"paired":true,"api_key":"secret-key"}"#).unwrap();
        let auth = ProxyAuth::new(false, pairing);
        let (_admit, access) = auth.classify("10.0.0.9".parse().unwrap(), None);
        assert_eq!(
            access,
            Access::Unauthorized,
            "a paired node must not admit an off-box peer that presented nothing"
        );
        assert_eq!(
            auth.classify("127.0.0.1".parse().unwrap(), None).1,
            Access::Accept
        );
    }

    #[test]
    fn the_loop_verdict_serves_a_flagged_peer_with_the_flag_off() {
        // `enforcing_refuses_the_same_peer_it_would_have_flagged` covers
        // `classify`. This covers what the accept loops actually consume: with
        // the flag off an unauthorized peer must still be served, and served
        // with the anonymous provenance rather than as a trusted local caller.
        let (_dir, auth) = unpaired_auth(false);
        let lan: std::net::IpAddr = "10.0.0.9".parse().unwrap();
        for edge in [RawEdge::Tcp, RawEdge::Udp] {
            assert_eq!(
                admit_raw_peer(&auth, lan, None, 5760, edge),
                Some(ClientOrigin::Unauthenticated),
                "{edge:?}: observe-only must serve, and record the anonymity"
            );
        }
    }

    #[test]
    fn the_loop_verdict_refuses_a_flagged_peer_on_both_edges_with_the_flag_on() {
        // The refusal must reach BOTH loops. A flag honoured on one edge and
        // dropped on the other closes the door an operator can see and leaves
        // the one they cannot.
        //
        // On UDP the `None` is what skips the peer-table insert as well as the
        // injection: enrolling a refused source would keep mirroring the FC's
        // whole telemetry stream back to it even with its own bytes discarded.
        let (_dir, auth) = unpaired_auth(true);
        let lan: std::net::IpAddr = "10.0.0.9".parse().unwrap();
        for edge in [RawEdge::Tcp, RawEdge::Udp] {
            assert_eq!(
                admit_raw_peer(&auth, lan, None, 5760, edge),
                None,
                "{edge:?}: enforcement must refuse, not log and continue"
            );
        }
    }

    #[test]
    fn enforcement_still_serves_the_on_box_and_lifeline_routes() {
        // The regression that would matter most: enforcing must not strand the
        // local operator or a fresh unit's only two ways in.
        let (_dir, auth) = unpaired_auth(true);
        for ip in ["127.0.0.1", "192.168.4.20", "192.168.7.2", "169.254.3.4"] {
            for edge in [RawEdge::Tcp, RawEdge::Udp] {
                assert_eq!(
                    admit_raw_peer(&auth, ip.parse().unwrap(), lifeline_local(ip), 14550, edge),
                    Some(ClientOrigin::Trusted),
                    "{ip} on {edge:?} must survive enforcement"
                );
            }
        }
    }

    /// The raw edges have no credential channel, so on a PAIRED node an off-box
    /// peer can never authenticate. Serving it anyway made the pairing gate
    /// cosmetic for flight control. It is refused unless the operator opted
    /// the raw edges into LAN access, and even then enforcement still wins.
    /// Built over an explicit pairing file rather than `from_config` +
    /// `ADOS_PAIRING_JSON` on purpose: that env var is read by the injector
    /// gate in this same test binary, and setting a process global would make
    /// two unrelated suites race.
    #[test]
    fn a_paired_node_serves_an_off_box_raw_peer_only_when_opted_into_lan_access() {
        let dir = tempfile::tempdir().unwrap();
        let pairing = write_pairing(dir.path(), r#"{"paired": true, "api_key": "k"}"#);
        for ip in ["10.0.0.9", "192.168.4.20", "8.8.8.8"] {
            let peer: std::net::IpAddr = ip.parse().unwrap();
            for edge in [RawEdge::Tcp, RawEdge::Udp] {
                assert_eq!(
                    admit_raw_peer(
                        &ProxyAuth::new(false, pairing.clone()),
                        peer,
                        lifeline_local(ip),
                        5760,
                        edge
                    ),
                    None,
                    "{ip} on {edge:?}: a paired node's raw edges are on-box only by default"
                );
                assert_eq!(
                    admit_raw_peer(
                        &ProxyAuth::new(false, pairing.clone()).with_raw_lan_access(true),
                        peer,
                        lifeline_local(ip),
                        5760,
                        edge
                    ),
                    Some(ClientOrigin::Unauthenticated),
                    "{ip} on {edge:?}: the opt-in serves it, recorded as anonymous"
                );
                assert_eq!(
                    admit_raw_peer(
                        &ProxyAuth::new(true, pairing.clone()).with_raw_lan_access(true),
                        peer,
                        lifeline_local(ip),
                        5760,
                        edge
                    ),
                    None,
                    "{ip} on {edge:?}: enforcement still refuses an opted-in LAN peer"
                );
            }
        }
        // The local operator is served whatever the flags say.
        assert_eq!(
            admit_raw_peer(
                &ProxyAuth::new(true, pairing),
                "127.0.0.1".parse().unwrap(),
                None,
                5760,
                RawEdge::Tcp
            ),
            Some(ClientOrigin::Trusted)
        );
    }

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

    /// A private-LAN caller, the ordinary off-box WebSocket client.
    const LAN: CallerClass = CallerClass::OperatorLan;

    /// An unpaired node's MAVLink WebSocket used to admit any caller, so a host
    /// the HTTP edge would refuse could still arm the aircraft through it. It
    /// now keeps the same lifeline-only posture as the raw edges.
    #[test]
    fn unpaired_refuses_a_caller_that_is_not_on_box_or_a_lifeline() {
        let (_d, auth) = unpaired_auth(true);
        for caller in [CallerClass::Remote, CallerClass::OperatorLan] {
            let (admit, access) = auth.should_admit(caller, None, &[]);
            assert!(!admit, "{caller:?} must be refused while unpaired");
            assert_eq!(access, Access::Unauthorized);
        }
        for caller in [CallerClass::OnBox, CallerClass::Lifeline] {
            let (admit, access) = auth.should_admit(caller, None, &[]);
            assert!(admit, "{caller:?} is how a fresh unit is reached");
            assert_eq!(access, Access::Accept);
        }
    }

    fn handshake_headers(
        pairs: &[(&'static str, &str)],
    ) -> tokio_tungstenite::tungstenite::http::HeaderMap {
        let mut h = tokio_tungstenite::tungstenite::http::HeaderMap::new();
        for (k, v) in pairs {
            h.insert(*k, v.parse().unwrap());
        }
        h
    }

    /// The handshake itself, headers and all: a tunnel terminating on loopback
    /// is a remote caller, on either pairing state.
    #[test]
    fn a_loopback_handshake_with_a_forwarding_header_is_refused() {
        let lo: std::net::IpAddr = "127.0.0.1".parse().unwrap();
        let tunnelled = handshake_headers(&[("cf-connecting-ip", "203.0.113.5")]);

        let (_d, unpaired) = unpaired_auth(true);
        assert_eq!(
            unpaired.handshake_decision(lo, Some(lo), &tunnelled),
            (false, Access::Unauthorized)
        );
        assert_eq!(
            unpaired.handshake_decision(lo, Some(lo), &handshake_headers(&[])),
            (true, Access::Accept),
            "the local operator is still served"
        );

        let (_d2, paired) = paired_auth(true, "k");
        assert_eq!(
            paired.handshake_decision(lo, Some(lo), &tunnelled),
            (false, Access::Unauthorized)
        );
        // The key still authenticates a tunnelled caller.
        let keyed = handshake_headers(&[("x-forwarded-for", "203.0.113.5"), ("x-ados-key", "k")]);
        assert_eq!(
            paired.handshake_decision(lo, Some(lo), &keyed),
            (true, Access::Accept)
        );
    }

    // Posture: paired + on-box admits without a key.
    #[test]
    fn paired_on_box_admits_without_a_key() {
        let (_d, auth) = paired_auth(true, "k");
        assert_eq!(auth.decide(CallerClass::OnBox, None), Access::Accept);
    }

    // Posture: paired + off-box + the valid key admits.
    #[test]
    fn paired_off_box_with_a_valid_key_admits() {
        let (_d, auth) = paired_auth(true, "ados_secret");
        assert_eq!(auth.decide(LAN, Some("ados_secret")), Access::Accept);
    }

    // Posture: paired + off-box + no key or a wrong key is unauthorized.
    #[test]
    fn paired_off_box_without_the_key_is_unauthorized() {
        let (_d, auth) = paired_auth(true, "ados_secret");
        assert_eq!(auth.decide(LAN, None), Access::Unauthorized);
        assert_eq!(auth.decide(LAN, Some("wrong")), Access::Unauthorized);
    }

    // Two-stage rollout: with the enforce flag OFF an unauthorized posture is
    // still ADMITTED (the default build is inert/observe-only).
    #[test]
    fn enforce_off_admits_an_unauthorized_connection_for_log_only() {
        let (_d, auth) = paired_auth(false, "ados_secret");
        let (admit, access) = auth.should_admit(LAN, None, &[]);
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
        let (admit, access) = auth.should_admit(LAN, None, &[]);
        assert!(!admit, "with enforcement on the connection is rejected");
        assert_eq!(access, Access::Unauthorized);
    }

    // An authorized connection is admitted whether or not enforcement is on.
    #[test]
    fn an_authorized_connection_is_admitted_under_either_flag() {
        for enforce in [false, true] {
            let (_d, auth) = paired_auth(enforce, "k");
            let (admit, access) = auth.should_admit(LAN, Some("k"), &[]);
            assert!(admit);
            assert_eq!(access, Access::Accept);
        }
    }

    // A valid ticket in the offered subprotocols authenticates a paired,
    // off-box, keyless connection (the browser GCS path) under either flag.
    #[test]
    fn a_valid_ticket_admits_off_box_without_a_key() {
        use ados_protocol::ws_ticket::{WsTicketIssuer, SCOPE_MAVLINK_WS};
        for enforce in [false, true] {
            let (_d, auth) = paired_auth(enforce, "ados_secret");
            let token = WsTicketIssuer::from_api_key("ados_secret")
                .mint(SCOPE_MAVLINK_WS, 30)
                .token;
            let offered = vec!["ados-ws-ticket".to_string(), token];
            // off-box, no key, but a valid ticket => Accept under either flag.
            let (admit, access) = auth.should_admit(LAN, None, &offered);
            assert!(admit, "a valid ticket admits even with enforcement on");
            assert_eq!(access, Access::Accept);
        }
    }

    // A ticket minted for a DIFFERENT key does not authenticate.
    #[test]
    fn a_ticket_for_the_wrong_key_is_unauthorized() {
        use ados_protocol::ws_ticket::{WsTicketIssuer, SCOPE_MAVLINK_WS};
        let (_d, auth) = paired_auth(true, "ados_secret");
        let token = WsTicketIssuer::from_api_key("a-different-key")
            .mint(SCOPE_MAVLINK_WS, 30)
            .token;
        let offered = vec!["ados-ws-ticket".to_string(), token];
        let (admit, access) = auth.should_admit(LAN, None, &offered);
        assert!(!admit);
        assert_eq!(access, Access::Unauthorized);
    }

    // A ticket minted for another scope does not authenticate the MAVLink WS.
    #[test]
    fn a_ticket_for_the_wrong_scope_is_unauthorized() {
        use ados_protocol::ws_ticket::WsTicketIssuer;
        let (_d, auth) = paired_auth(true, "ados_secret");
        let token = WsTicketIssuer::from_api_key("ados_secret")
            .mint("gs.pic_events", 30)
            .token;
        let offered = vec!["ados-ws-ticket".to_string(), token];
        let (admit, _access) = auth.should_admit(LAN, None, &offered);
        assert!(!admit);
    }

    #[test]
    fn extract_ticket_finds_the_value_after_the_marker() {
        let offered = vec!["ados-ws-ticket".to_string(), "v1|s|1|2|ff".to_string()];
        assert_eq!(extract_ticket(&offered), Some("v1|s|1|2|ff"));
        // Marker with no following value, and no marker at all, both yield None.
        assert_eq!(extract_ticket(&["ados-ws-ticket".to_string()]), None);
        assert_eq!(extract_ticket(&["mavlink".to_string()]), None);
    }

    // A pair/unpair that happens while the proxy runs is honoured for a new
    // connection within the TTL once it lapses (the cache re-reads the file).
    #[test]
    fn posture_re_reads_after_the_ttl_lapses() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_pairing(dir.path(), r#"{"paired": false}"#);
        let auth = WsProxyAuth::new(true, path.clone());
        // First read: unpaired => an off-box non-lifeline caller is refused.
        assert_eq!(auth.decide(LAN, None), Access::Unauthorized);
        assert_eq!(auth.decide(CallerClass::Lifeline, None), Access::Accept);
        // Now pair and let the TTL lapse so the next read picks it up.
        write_pairing(dir.path(), r#"{"paired": true, "api_key": "k2"}"#);
        {
            let mut c = auth.cache.lock().unwrap();
            c.at = Instant::now() - (PAIRING_TTL + Duration::from_secs(1));
        }
        // Paired: the lifeline is off-box like anyone else and needs the key.
        assert_eq!(
            auth.decide(CallerClass::Lifeline, None),
            Access::Unauthorized
        );
        assert_eq!(auth.decide(LAN, Some("k2")), Access::Accept);
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
