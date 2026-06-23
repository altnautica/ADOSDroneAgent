//! Authentication and rate limiting for the LAN listener.
//!
//! The same Router is served on two edges. The trusted local Unix socket
//! carries no auth and no rate limit: anything on-box that can open the socket
//! is already inside the trust boundary. The LAN TCP edge mirrors the agent's
//! HTTP auth posture exactly:
//!
//! - **Unpaired ⇒ all routes open.** A fresh agent has no key; physical presence
//!   on the LAN is the gate, the same stance the pairing-claim flow takes.
//! - **Paired ⇒ `X-ADOS-Key` required** and must equal the stored pairing key.
//!
//! On top of the pairing gate two trust shortcuts mirror the Python middleware:
//!
//! - **Public paths** ([`is_public`]) are open on both edges even when paired,
//!   so a fresh GCS can read `/api/version` and walk the pairing handshake
//!   before it holds a key, and a watchdog can hit `/healthz`.
//! - **On-box loopback trust** ([`is_on_box`]): a request whose peer address is
//!   loopback and that carries no proxy-forwarding header is the local operator,
//!   who already holds shell-level privilege that exceeds API auth. This is free
//!   on the Unix socket (which never installs the gate); for the loopback-TCP
//!   case the caller threads the peer address in. A proxy or tunnel that
//!   terminates on 127.0.0.1 is excluded by the forwarding-header check, so it
//!   can never impersonate an on-box caller to bypass authentication.
//!
//! The pairing state is the agent's `pairing.json` (`{ "paired": bool,
//! "api_key": "..." }`). It is read fresh on each request through a short-TTL
//! cache so a pair/unpair that happens while the daemon runs is honoured without
//! a restart, while a burst of requests does not stat the file every time.
//!
//! A token-bucket rate limiter caps the TCP edge so a runaway client cannot pin
//! the box; the Unix edge is unlimited.

use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

// The pairing-posture primitives are shared with the direct MAVLink WebSocket
// proxy, so they live once in the protocol crate. Re-exported here under the
// names this surface (and its callers) already use, so the HTTP edge keeps a
// single import point for the auth posture.
pub use ados_protocol::pairing_posture::{
    constant_time_eq, is_on_box, load_pairing, Pairing, FORWARDED_HEADERS,
};
use ados_protocol::pairing_posture::{data_plane_access, Access};

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

    /// The pairing-state file path this reader watches.
    pub fn path(&self) -> &std::path::Path {
        &self.path
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

    /// Decide a request: `true` to pass, `false` to reject with 401. A public
    /// path is always allowed; an unpaired agent allows everything; a paired
    /// agent requires the exact key. The on-box loopback shortcut is applied by
    /// the caller before this is consulted (it needs the peer address, which
    /// this reader does not carry), so this only models the unpaired-vs-keyed
    /// posture (`on_box = false`).
    pub fn authorize(&self, path: &str, presented_key: Option<&str>) -> bool {
        if is_public(path) {
            return true;
        }
        // The on-box shortcut is handled at the HTTP edge before this is reached;
        // here only the unpaired-or-keyed posture remains, so pass `on_box=false`.
        data_plane_access(&self.current(), false, presented_key) == Access::Accept
    }
}

impl Default for PairingState {
    fn default() -> Self {
        Self::new()
    }
}

/// The endpoints that are public on both edges (no key, no rate limit even on
/// TCP) so a fresh GCS can read the version, walk the local pairing handshake
/// before it holds a key, and a liveness probe can always hit `/healthz`. This
/// is the native surface's exempt set, narrower than the Python middleware's
/// (no setup/static paths live here). `/api/time` is deliberately NOT public.
///
/// The two ground-station WebSocket relays are exempt here too: a WebSocket
/// handshake is upgraded past the HTTP key gate, and a browser cannot set the
/// `X-ADOS-Key` header on it, so the edge must let the upgrade reach the handler,
/// which then enforces the WebSocket auth contract itself (a header key OR a
/// scoped one-shot ticket). Mirrors the residual handlers, which authenticated
/// inside the handler for the same reason.
pub fn is_public(path: &str) -> bool {
    matches!(
        path,
        "/healthz"
            | "/api/ping"
            | "/api/pairing/info"
            | "/api/pairing/code"
            | "/api/pairing/claim"
            | "/api/version"
            | "/api/v1/ground-station/ws/uplink"
            | "/api/v1/ground-station/pic/events"
    )
}

/// A fixed-window token-bucket rate limiter for the TCP edge. Each refill
/// window grants `capacity` tokens; a request consumes one. When the bucket is
/// empty within a window the request is rejected with 429. One shared bucket
/// guards the whole TCP edge (the budget is per-agent, not per-route), which is
/// enough to stop a runaway client from pinning the box.
pub struct RateLimiter {
    capacity: u32,
    window: Duration,
    state: Mutex<RateState>,
}

struct RateState {
    tokens: u32,
    window_start: Instant,
}

impl RateLimiter {
    /// A limiter granting `capacity` requests per `window`.
    pub fn new(capacity: u32, window: Duration) -> Self {
        Self {
            capacity,
            window,
            state: Mutex::new(RateState {
                tokens: capacity,
                window_start: Instant::now(),
            }),
        }
    }

    /// The default control-surface budget: a generous per-second rate, matching
    /// the FastAPI posture. Status polling and command bursts both fit under it.
    pub fn default_control() -> Self {
        Self::new(60, Duration::from_secs(1))
    }

    /// Try to admit one request. Returns `true` when admitted, `false` when the
    /// window's budget is exhausted.
    pub fn check(&self) -> bool {
        let mut s = self.state.lock().unwrap_or_else(|p| p.into_inner());
        if s.window_start.elapsed() >= self.window {
            s.window_start = Instant::now();
            s.tokens = self.capacity;
        }
        if s.tokens > 0 {
            s.tokens -= 1;
            true
        } else {
            false
        }
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
    fn unpaired_opens_every_route() {
        let dir = tempfile::tempdir().unwrap();
        // No pairing file at all → unpaired.
        let state = PairingState::with_path(dir.path().join("absent.json"));
        assert_eq!(state.current(), Pairing::Unpaired);
        assert!(state.authorize("/api/status", None));
        assert!(state.authorize("/api/status", Some("anything")));
    }

    #[test]
    fn paired_requires_the_exact_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_pairing(dir.path(), r#"{"paired": true, "api_key": "ados_secret"}"#);
        let state = PairingState::with_path(path);
        assert_eq!(state.current(), Pairing::Paired("ados_secret".to_string()));
        assert!(state.authorize("/api/status", Some("ados_secret")));
        assert!(!state.authorize("/api/status", Some("wrong")));
        assert!(!state.authorize("/api/status", None));
    }

    #[test]
    fn the_native_exempt_set_is_open_even_when_paired() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_pairing(dir.path(), r#"{"paired": true, "api_key": "k"}"#);
        let state = PairingState::with_path(path);
        // The exact public set for this surface.
        assert!(state.authorize("/healthz", None));
        assert!(state.authorize("/api/version", None));
        assert!(state.authorize("/api/pairing/info", None));
        assert!(state.authorize("/api/pairing/code", None));
        assert!(state.authorize("/api/pairing/claim", None));
        // /api/time is NOT exempt, so a paired agent gates it.
        assert!(!state.authorize("/api/time", None));
        // A non-exempt route still needs the key.
        assert!(!state.authorize("/api/status", None));
        assert!(state.authorize("/api/status", Some("k")));
    }

    #[test]
    fn is_public_is_exactly_the_exempt_paths() {
        for p in [
            "/healthz",
            "/api/ping",
            "/api/version",
            "/api/pairing/info",
            "/api/pairing/code",
            "/api/pairing/claim",
            // The two ground-station WebSocket relays: the upgrade bypasses the
            // HTTP key gate, so the handler does its own ticket/header auth.
            "/api/v1/ground-station/ws/uplink",
            "/api/v1/ground-station/pic/events",
        ] {
            assert!(is_public(p), "{p} should be public");
        }
        for p in [
            "/api/time",
            "/api/status",
            "/api/command",
            "/api/pairing/unpair",
            "/v1/openapi.json",
            // The mesh WebSocket stays proxied to the residual, NOT public.
            "/api/v1/ground-station/ws/mesh",
        ] {
            assert!(!is_public(p), "{p} should NOT be public");
        }
    }

    #[test]
    fn on_box_trust_is_loopback_and_no_forwarding_header() {
        // Loopback peer, no proxy header → trusted.
        assert!(is_on_box(true, false));
        // Loopback peer but a forwarding header present → a tunnel terminating
        // on loopback, NOT trusted.
        assert!(!is_on_box(true, true));
        // Off-box peer → never trusted regardless of headers.
        assert!(!is_on_box(false, false));
        assert!(!is_on_box(false, true));
    }

    #[test]
    fn a_paired_state_without_a_key_reads_as_unpaired() {
        let dir = tempfile::tempdir().unwrap();
        // paired:true but no api_key, or empty → open (matches the agent's
        // "no key on file means open" stance).
        let path = write_pairing(dir.path(), r#"{"paired": true, "api_key": ""}"#);
        let state = PairingState::with_path(path);
        assert_eq!(state.current(), Pairing::Unpaired);
    }

    #[test]
    fn malformed_pairing_file_reads_as_unpaired() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_pairing(dir.path(), "this is not json");
        let state = PairingState::with_path(path);
        assert_eq!(state.current(), Pairing::Unpaired);
    }

    #[test]
    fn constant_time_eq_matches_byte_equality() {
        // Equal slices compare equal; any single-byte or length difference is
        // rejected, exactly as `==` would, only without the early exit.
        assert!(constant_time_eq(b"ados_secret", b"ados_secret"));
        assert!(!constant_time_eq(b"ados_secret", b"ados_secre1"));
        assert!(!constant_time_eq(b"ados_secret", b"xdos_secret"));
        assert!(!constant_time_eq(b"ados_secret", b"ados_secret_longer"));
        assert!(!constant_time_eq(b"ados_secret", b"short"));
        assert!(constant_time_eq(b"", b""));
        assert!(!constant_time_eq(b"", b"x"));
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
}
