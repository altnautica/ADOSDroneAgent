//! Proxy admission: the WebSocket credential check and the raw-port peer verdict.

use super::*;

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
/// the config. Both edges enforce by default. The WebSocket authenticates its
/// callers with the `X-ADOS-Key` header or an `ados-ws-ticket` subprotocol;
/// the byte-stream edges (TCP, UDP) have no credential channel at all, so an
/// off-box, non-lifeline raw peer is always unauthorized and is served only
/// when the operator opened the raw edges to the LAN
/// ([`WsProxyAuth::with_raw_lan_access`]). With `enforce` off the gate is
/// observe-only for an unpaired node: an unauthorized connection is logged
/// and STILL admitted. With it on, an unauthorized connection is refused at
/// the handshake (WebSocket) or before any bytes are read (TCP, UDP).
///
/// On a PAIRED node the raw edges ignore `enforce`: without the LAN opt-in
/// they serve on-box callers only, and with it they serve the LAN.
///
/// The byte-stream proxies share this gate with the WebSocket, so the name is
/// an alias rather than a second type: one posture, three edges.
pub type ProxyAuth = WsProxyAuth;

#[derive(Clone)]
pub struct WsProxyAuth {
    pub(super) enforce: bool,
    pub(super) raw_lan_access: bool,
    pub(super) pairing_path: PathBuf,
    pub(super) cache: Arc<StdMutex<PairingCache>>,
}

pub(super) struct PairingCache {
    pub(super) loaded: Pairing,
    pub(super) at: Instant,
    pub(super) primed: bool,
}

/// The direct-proxy access rule for one classified caller, given the pairing
/// posture. See [`WsProxyAuth`].
pub(super) fn direct_access(
    pairing: &Pairing,
    caller: CallerClass,
    presented_key: Option<&str>,
) -> Access {
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

    /// Open the raw byte-stream edges (TCP, UDP) to off-box peers
    /// (`mavlink.raw_proxy_lan_access`). Those edges carry no credential
    /// channel, so an off-box, non-lifeline peer is always unauthorized; this
    /// opt-in is what serves it (a desktop ground station on the LAN). Without
    /// it a paired node's raw proxies are on-box only, and an unpaired node's
    /// serve on-box callers and lifelines unless `enforce` is off. Consulted
    /// only by [`Self::classify`]; the WebSocket authenticates its callers
    /// instead.
    pub fn with_raw_lan_access(mut self, open: bool) -> Self {
        self.raw_lan_access = open;
        self
    }

    /// The current pairing posture, reading `pairing.json` at most once per TTL.
    pub(super) fn current(&self) -> Pairing {
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
    pub(super) fn decide(&self, caller: CallerClass, presented_key: Option<&str>) -> Access {
        direct_access(&self.current(), caller, presented_key)
    }

    /// Whether the offered WebSocket subprotocols carry a valid `gs.mavlink_ws`
    /// ticket for the current paired key. A browser cannot set `X-ADOS-Key`, so
    /// it presents a short-lived HMAC ticket (minted by the native control
    /// surface, keyed off the same `pairing.json`) through the subprotocol list.
    /// Always false when unpaired (there is no key to verify against) or when
    /// no ticket is offered.
    pub(super) fn ticket_valid(&self, offered: &[String]) -> bool {
        let Pairing::Paired(key) = self.current() else {
            return false;
        };
        let Some(token) = extract_ticket(offered) else {
            return false;
        };
        // Spent on use: a ticket lifted from a log cannot open a second socket.
        WsTicketIssuer::from_api_key(&key)
            .verify_once(token, SCOPE_MAVLINK_WS, now_unix())
            .is_ok()
    }

    /// Classify a raw-socket peer by address alone and decide it.
    ///
    /// The byte-stream proxies have no header and no handshake, so the peer
    /// address is the only signal available and there is no key or ticket to
    /// present. Returns `(admit, access)`:
    ///
    /// - an authorized caller (on-box; a lifeline while unpaired) is admitted;
    /// - any other caller is admitted when the operator opened the raw edges
    ///   to the LAN;
    /// - otherwise, on an UNPAIRED node it is admitted only when enforcement
    ///   is off (observe-only), and on a PAIRED node never.
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
            (Access::Unauthorized, Pairing::Paired(_)) => self.raw_lan_access,
            (Access::Unauthorized, Pairing::Unpaired) => self.raw_lan_access || !self.enforce,
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
    pub(super) fn should_admit(
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
    pub(super) fn handshake_decision(
        &self,
        peer: std::net::IpAddr,
        local: Option<std::net::IpAddr>,
        headers: &tokio_tungstenite::tungstenite::http::HeaderMap,
    ) -> (bool, Access) {
        let caller = classify_caller(Some(peer), local, |h| headers.contains_key(h));
        let presented = headers.get(WS_KEY_HEADER).and_then(|v| v.to_str().ok());
        self.should_admit(caller, presented, &offered_subprotocols(headers))
    }

    /// What a live WebSocket session needs to re-check its admission: the
    /// caller class and key it presented, and, for a ticket-admitted session,
    /// the pairing posture the ticket was verified under. A ticket is spent on
    /// use, so it cannot be verified again; the session stays admitted while
    /// that posture is unchanged.
    pub(super) fn ws_session(
        &self,
        peer: std::net::IpAddr,
        local: Option<std::net::IpAddr>,
        headers: &tokio_tungstenite::tungstenite::http::HeaderMap,
        access: Access,
    ) -> WsSession {
        let caller = classify_caller(Some(peer), local, |h| headers.contains_key(h));
        let key = headers
            .get(WS_KEY_HEADER)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let by_ticket =
            access == Access::Accept && self.decide(caller, key.as_deref()) == Access::Unauthorized;
        WsSession {
            caller,
            key,
            ticket_posture: by_ticket.then(|| self.current()),
        }
    }

    /// Whether a live WebSocket session would still be admitted now.
    pub(super) fn ws_still_admitted(&self, session: &WsSession) -> bool {
        let pairing = self.current();
        let mut access = direct_access(&pairing, session.caller, session.key.as_deref());
        if access == Access::Unauthorized && session.ticket_posture.as_ref() == Some(&pairing) {
            access = Access::Accept;
        }
        match access {
            Access::Accept => true,
            Access::Unauthorized => !self.enforce,
        }
    }
}

/// Map an access decision to the provenance the send path records.
///
/// Deliberately not a permission: the byte path is unchanged either way. It
/// exists so the one fallback that radiates a client's bytes to a remote
/// aircraft can say when the client was anonymous.
pub(super) fn origin_of(access: Access) -> ClientOrigin {
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
pub(super) enum RawEdge {
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
pub(super) fn admit_raw_peer(
    auth: &ProxyAuth,
    peer: std::net::IpAddr,
    local: Option<std::net::IpAddr>,
    port: u16,
    edge: RawEdge,
) -> Option<ClientOrigin> {
    let (origin, access) = raw_peer_verdict(auth, peer, local);
    if access != Access::Accept {
        log_unauthorized_raw_peer(port, peer, origin.is_some(), edge, 0);
    }
    origin
}

/// The raw-peer verdict without the log line: `(Some(origin), access)` to serve
/// it, `(None, access)` to refuse it. The UDP loop caches this per source
/// address and rate-limits the log itself.
pub(super) fn raw_peer_verdict(
    auth: &ProxyAuth,
    peer: std::net::IpAddr,
    local: Option<std::net::IpAddr>,
) -> (Option<ClientOrigin>, Access) {
    let (admit, access) = auth.classify(peer, local);
    (admit.then(|| origin_of(access)), access)
}

/// The one log line per edge for an unauthorized raw peer. `suppressed` counts
/// datagrams from the same peer since its previous line (UDP only).
pub(super) fn log_unauthorized_raw_peer(
    port: u16,
    peer: std::net::IpAddr,
    admitted: bool,
    edge: RawEdge,
    suppressed: u64,
) {
    match edge {
        RawEdge::Tcp => tracing::warn!(port, peer = %peer, admitted, "tcp_proxy_unauthorized"),
        RawEdge::Udp => tracing::warn!(
            port,
            peer = %peer,
            admitted,
            suppressed,
            "udp_proxy_unauthorized"
        ),
    }
}
