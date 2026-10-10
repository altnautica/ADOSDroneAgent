//! Authentication and rate limiting for the LAN listener.
//!
//! The same Router is served on two edges. The trusted local Unix socket
//! carries no auth and no rate limit: anything on-box that can open the socket
//! is already inside the trust boundary. The LAN TCP edge follows the agent's
//! data-plane posture, built from the shared primitives in
//! [`ados_protocol::pairing_posture`] so the two edges cannot drift:
//!
//! - **Unpaired ⇒ first-boot reach only.** The local operator (a loopback peer
//!   that carries no forwarding header) and the first-boot lifelines are
//!   served. An operator-LAN caller is not: the agent's HTTP edge serves it data
//!   only behind a dashboard PIN session, which this surface does not take, and
//!   a remote caller (a public-WAN host, anything relayed by a proxy or tunnel)
//!   never reaches an unclaimed node's history.
//! - **Paired ⇒ `X-ADOS-Key` required** and must equal the stored pairing key.
//! - **Unreadable `pairing.json` ⇒ refused.** A file that exists but cannot be
//!   read or parsed may be a paired node's record on a failing card; reading it
//!   as unpaired would open the store to anyone.
//!
//! The pairing state is read fresh through a short-TTL cache so a pair/unpair
//! that happens while the daemon runs is honoured without a restart, while a
//! burst of requests does not stat the file every time.
//!
//! Two public endpoints (`/v1/healthz`, `/v1/openapi.json`) are open on both
//! edges so discovery and liveness probes always work.
//!
//! Two token-bucket rate limiters cap the TCP edge so a runaway client cannot
//! pin the box; the Unix edge is unlimited. Authentication runs first and picks
//! the bucket: an authorized request draws from the shared read budget, a
//! refused one from a small per-peer bucket ([`PeerRateLimiter`]). A flood of
//! bad or missing keys therefore spends only its own peers' budget and can never
//! starve a keyed caller.

use std::collections::HashMap;
use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use ados_protocol::pairing_posture::{constant_time_eq, load_pairing, CallerClass, Pairing};

/// Default pairing-state path: the agent's `pairing.json`.
pub const DEFAULT_PAIRING_PATH: &str = "/etc/ados/pairing.json";

/// How long a loaded pairing state is trusted before the file is re-read. Short
/// enough that a pair/unpair is honoured within a few requests, long enough that
/// a request burst does not stat the file every time.
const PAIRING_TTL: Duration = Duration::from_secs(2);

/// Reads `pairing.json` and answers the auth question, with a short-TTL cache so
/// the file is not stat-ed on every request. Cheap to clone (it is held behind
/// an `Arc` in the shared app state).
pub struct PairingState {
    path: PathBuf,
    cache: Mutex<Cache>,
}

struct Cache {
    loaded: Pairing,
    at: Instant,
    primed: bool,
}

impl PairingState {
    /// Build a pairing reader against the agent's standard path.
    pub fn new() -> Self {
        Self::with_path(PathBuf::from(DEFAULT_PAIRING_PATH))
    }

    /// Build a pairing reader against an explicit path (tests).
    pub fn with_path(path: PathBuf) -> Self {
        Self {
            path,
            cache: Mutex::new(Cache {
                loaded: Pairing::Unpaired,
                at: Instant::now(),
                primed: false,
            }),
        }
    }

    /// The current pairing posture, reading the file at most once per TTL.
    pub fn current(&self) -> Pairing {
        let mut cache = self.cache.lock().unwrap_or_else(|p| p.into_inner());
        if cache.primed && cache.at.elapsed() < PAIRING_TTL {
            return cache.loaded.clone();
        }
        let fresh = load_pairing(&self.path);
        cache.loaded = fresh.clone();
        cache.at = Instant::now();
        cache.primed = true;
        fresh
    }

    /// Decide a TCP-edge request: `true` to pass, `false` to reject with 401. A
    /// public path is always allowed; an unpaired agent serves only first-boot
    /// reach; a paired agent requires the exact key; an unreadable pairing file
    /// serves nobody on this edge.
    pub fn authorize(&self, path: &str, caller: CallerClass, presented_key: Option<&str>) -> bool {
        if is_public(path) {
            return true;
        }
        match self.current() {
            Pairing::Unpaired => caller.is_first_boot_reach(),
            Pairing::Paired(expected) => presented_key.is_some_and(|presented| {
                constant_time_eq(presented.as_bytes(), expected.as_bytes())
            }),
            Pairing::Unreadable => false,
        }
    }
}

impl Default for PairingState {
    fn default() -> Self {
        Self::new()
    }
}

/// The endpoints that are public on both edges (no key, no rate limit even on
/// TCP) so liveness and discovery probes always answer.
pub fn is_public(path: &str) -> bool {
    path == "/v1/healthz" || path == "/v1/openapi.json"
}

/// A fixed-window token-bucket rate limiter for authorized TCP-edge requests.
/// Each refill window grants `capacity` tokens; a request consumes one. When the
/// bucket is empty within a window the request is rejected with 429. One shared
/// bucket guards the whole authorized read path (the read budget is per-agent,
/// not per-route), which is enough to stop a runaway client from pinning the
/// box. Only requests that passed authentication are charged here.
pub struct RateLimiter {
    capacity: u32,
    window: Duration,
    state: Mutex<RateState>,
}

/// One fixed-window bucket: the tokens left and when the window opened.
struct RateState {
    tokens: u32,
    window_start: Instant,
}

impl RateState {
    fn full(capacity: u32, now: Instant) -> Self {
        Self {
            tokens: capacity,
            window_start: now,
        }
    }

    /// Refill if the window has passed, then try to spend one token.
    fn take(&mut self, capacity: u32, window: Duration, now: Instant) -> bool {
        if now.saturating_duration_since(self.window_start) >= window {
            self.window_start = now;
            self.tokens = capacity;
        }
        if self.tokens > 0 {
            self.tokens -= 1;
            true
        } else {
            false
        }
    }
}

impl RateLimiter {
    /// A limiter granting `capacity` requests per `window`.
    pub fn new(capacity: u32, window: Duration) -> Self {
        Self {
            capacity,
            window,
            state: Mutex::new(RateState::full(capacity, Instant::now())),
        }
    }

    /// The default read budget: a generous per-second rate for an observability
    /// surface, matching the FastAPI read posture.
    pub fn default_read() -> Self {
        Self::new(30, Duration::from_secs(1))
    }

    /// Try to admit one request. Returns `true` when admitted, `false` when the
    /// window's budget is exhausted.
    pub fn check(&self) -> bool {
        let mut s = self.state.lock().unwrap_or_else(|p| p.into_inner());
        s.take(self.capacity, self.window, Instant::now())
    }
}

/// How many peers the unauthenticated limiter tracks at once. A full map
/// evicts its least-recently-seen peer, so memory stays bounded however many
/// addresses a flood comes from.
pub const MAX_TRACKED_PEERS: usize = 256;

/// Refused requests one peer may make per window before it gets 429 instead of
/// 401. Small: a legitimate client that gets its key wrong needs a handful of
/// attempts, not a stream.
pub const UNAUTH_PEER_CAPACITY: u32 = 10;

/// A per-peer fixed-window limiter for requests that failed authentication.
///
/// Each peer address gets its own bucket of `capacity` per `window`, held in a
/// map capped at `max_peers` entries with least-recently-used eviction. It is
/// charged only *after* authentication refused a request, so it bounds how fast
/// one peer can probe keys without ever touching the authorized budget.
pub struct PeerRateLimiter {
    capacity: u32,
    window: Duration,
    max_peers: usize,
    state: Mutex<PeerBuckets>,
}

struct PeerBuckets {
    buckets: HashMap<IpAddr, PeerBucket>,
    /// A logical clock bumped on every check; a bucket's `last_seen` is the
    /// tick of its latest use, so the smallest tick is the LRU peer.
    tick: u64,
}

struct PeerBucket {
    bucket: RateState,
    last_seen: u64,
}

impl PeerRateLimiter {
    /// A limiter granting each peer `capacity` refused requests per `window`,
    /// tracking at most `max_peers` peers (at least one).
    pub fn new(capacity: u32, window: Duration, max_peers: usize) -> Self {
        let max_peers = max_peers.max(1);
        Self {
            capacity,
            window,
            max_peers,
            state: Mutex::new(PeerBuckets {
                buckets: HashMap::with_capacity(max_peers),
                tick: 0,
            }),
        }
    }

    /// The default unauthenticated budget: [`UNAUTH_PEER_CAPACITY`] per second
    /// per peer, [`MAX_TRACKED_PEERS`] peers.
    pub fn default_unauth() -> Self {
        Self::new(
            UNAUTH_PEER_CAPACITY,
            Duration::from_secs(1),
            MAX_TRACKED_PEERS,
        )
    }

    /// Charge one refused request to `peer`. Returns `true` while the peer's
    /// window still has budget, `false` once it is spent.
    pub fn check(&self, peer: IpAddr) -> bool {
        let now = Instant::now();
        let mut s = self.state.lock().unwrap_or_else(|p| p.into_inner());
        s.tick += 1;
        let tick = s.tick;
        if !s.buckets.contains_key(&peer) && s.buckets.len() >= self.max_peers {
            let lru = s
                .buckets
                .iter()
                .min_by_key(|(_, b)| b.last_seen)
                .map(|(ip, _)| *ip);
            if let Some(lru) = lru {
                s.buckets.remove(&lru);
            }
        }
        let capacity = self.capacity;
        let entry = s.buckets.entry(peer).or_insert_with(|| PeerBucket {
            bucket: RateState::full(capacity, now),
            last_seen: tick,
        });
        entry.last_seen = tick;
        entry.bucket.take(capacity, self.window, now)
    }

    /// How many peers are tracked right now.
    #[cfg(test)]
    fn tracked_peers(&self) -> usize {
        self.state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .buckets
            .len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_pairing(dir: &std::path::Path, body: &str) -> PathBuf {
        let path = dir.join("pairing.json");
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(body.as_bytes()).unwrap();
        path
    }

    #[test]
    fn unpaired_serves_first_boot_reach_only() {
        let dir = tempfile::tempdir().unwrap();
        // No pairing file at all → unpaired.
        let state = PairingState::with_path(dir.path().join("absent.json"));
        assert_eq!(state.current(), Pairing::Unpaired);
        assert!(state.authorize("/v1/query", CallerClass::OnBox, None));
        assert!(state.authorize("/v1/query", CallerClass::Lifeline, None));
        // The history of an unclaimed node is not served to the wider LAN
        // (the agent's HTTP edge wants a PIN session there) or to anything
        // remote, key or not.
        assert!(!state.authorize("/v1/query", CallerClass::OperatorLan, None));
        assert!(!state.authorize("/v1/query", CallerClass::Remote, Some("anything")));
    }

    #[test]
    fn paired_requires_the_exact_key_from_every_caller() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_pairing(dir.path(), r#"{"paired": true, "api_key": "ados_secret"}"#);
        let state = PairingState::with_path(path);
        assert_eq!(state.current(), Pairing::Paired("ados_secret".to_string()));
        for caller in [
            CallerClass::OnBox,
            CallerClass::Lifeline,
            CallerClass::OperatorLan,
            CallerClass::Remote,
        ] {
            assert!(state.authorize("/v1/query", caller, Some("ados_secret")));
            assert!(!state.authorize("/v1/query", caller, Some("ados_secre1")));
            assert!(!state.authorize("/v1/query", caller, Some("ados")));
            assert!(!state.authorize("/v1/query", caller, None));
        }
    }

    #[test]
    fn public_paths_are_open_even_when_paired() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_pairing(dir.path(), r#"{"paired": true, "api_key": "k"}"#);
        let state = PairingState::with_path(path);
        assert!(state.authorize("/v1/healthz", CallerClass::Remote, None));
        assert!(state.authorize("/v1/openapi.json", CallerClass::Remote, None));
    }

    /// A pairing file that exists but cannot be trusted may be a paired node's
    /// record on a failing card. Reading it as unpaired would open the store.
    #[test]
    fn an_unreadable_pairing_file_fails_closed() {
        for body in ["this is not json", r#"{"paired": true, "api_key": ""}"#] {
            let dir = tempfile::tempdir().unwrap();
            let path = write_pairing(dir.path(), body);
            let state = PairingState::with_path(path);
            assert_eq!(state.current(), Pairing::Unreadable, "{body}");
            assert!(!state.authorize("/v1/query", CallerClass::OnBox, None));
            assert!(!state.authorize("/v1/query", CallerClass::Lifeline, Some("k")));
        }
    }

    #[test]
    fn rate_limiter_admits_up_to_capacity_then_rejects() {
        let limiter = RateLimiter::new(3, Duration::from_secs(60));
        assert!(limiter.check());
        assert!(limiter.check());
        assert!(limiter.check());
        // Fourth in the same window is rejected.
        assert!(!limiter.check());
    }

    #[test]
    fn rate_limiter_refills_after_the_window() {
        let limiter = RateLimiter::new(1, Duration::from_millis(20));
        assert!(limiter.check());
        assert!(!limiter.check());
        std::thread::sleep(Duration::from_millis(30));
        assert!(limiter.check(), "the window refilled");
    }

    fn ip(n: u32) -> IpAddr {
        IpAddr::V4(std::net::Ipv4Addr::from(0x0a00_0000 + n))
    }

    #[test]
    fn per_ip_map_never_exceeds_256_entries() {
        let limiter = PeerRateLimiter::new(1, Duration::from_secs(3600), MAX_TRACKED_PEERS);
        assert_eq!(MAX_TRACKED_PEERS, 256);
        for n in 0..1_000 {
            limiter.check(ip(n));
            assert!(
                limiter.tracked_peers() <= MAX_TRACKED_PEERS,
                "map grew past the cap at peer {n}"
            );
        }
        assert_eq!(limiter.tracked_peers(), MAX_TRACKED_PEERS);
    }

    #[test]
    fn the_least_recently_seen_peer_is_evicted_first() {
        let limiter = PeerRateLimiter::new(1, Duration::from_secs(3600), 3);
        for n in 0..3 {
            assert!(limiter.check(ip(n)), "first request from a peer has budget");
        }
        // Peer 0 is seen again (and is now out of budget), so peer 1 is the LRU.
        assert!(!limiter.check(ip(0)));
        // A fourth peer evicts peer 1, not peer 0.
        assert!(limiter.check(ip(3)));
        assert_eq!(limiter.tracked_peers(), 3);
        assert!(!limiter.check(ip(0)), "peer 0 kept its spent bucket");
        // Peer 1 was evicted; re-admitting it evicts the LRU again (peer 2).
        assert!(limiter.check(ip(1)), "peer 1 was evicted and starts fresh");
        assert_eq!(limiter.tracked_peers(), 3);
    }

    #[test]
    fn peers_have_independent_buckets() {
        let limiter = PeerRateLimiter::new(2, Duration::from_secs(3600), 8);
        assert!(limiter.check(ip(1)));
        assert!(limiter.check(ip(1)));
        assert!(!limiter.check(ip(1)), "peer 1 spent its budget");
        assert!(limiter.check(ip(2)), "peer 2 is unaffected");
    }
}
