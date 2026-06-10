//! Authentication and rate limiting for the LAN listener.
//!
//! The same Router is served on two edges. The trusted local Unix socket
//! carries no auth and no rate limit: anything on-box that can open the socket
//! is already inside the trust boundary. The LAN TCP edge mirrors the agent's
//! HTTP auth posture exactly:
//!
//! - **Unpaired ⇒ all routes open.** A fresh agent has no key; being on the LAN
//!   is the gate, the same stance the pairing-claim flow takes.
//! - **Paired ⇒ `X-ADOS-Key` required** and must equal the stored pairing key.
//!
//! The pairing state is the agent's `pairing.json` (`{ "paired": bool,
//! "api_key": "..." }`). It is read fresh on each request through a short-TTL
//! cache so a pair/unpair that happens while the daemon runs is honoured without
//! a restart, while a burst of requests does not stat the file every time.
//!
//! Two public endpoints (`/v1/healthz`, `/v1/openapi.json`) are open on both
//! edges so discovery and liveness probes always work.
//!
//! A token-bucket rate limiter caps the TCP edge so a runaway client cannot pin
//! the box; the Unix edge is unlimited.

use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Default pairing-state path: the agent's `pairing.json`.
pub const DEFAULT_PAIRING_PATH: &str = "/etc/ados/pairing.json";

/// How long a loaded pairing state is trusted before the file is re-read. Short
/// enough that a pair/unpair is honoured within a few requests, long enough that
/// a request burst does not stat the file every time.
const PAIRING_TTL: Duration = Duration::from_secs(2);

/// The resolved pairing posture for the current request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Pairing {
    /// No pairing on file: all routes are open on the LAN edge.
    Unpaired,
    /// Paired with this exact key required in `X-ADOS-Key`.
    Paired(String),
}

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

    /// Decide a request: `true` to pass, `false` to reject with 401. A public
    /// path is always allowed; an unpaired agent allows everything; a paired
    /// agent requires the exact key.
    pub fn authorize(&self, path: &str, presented_key: Option<&str>) -> bool {
        if is_public(path) {
            return true;
        }
        match self.current() {
            Pairing::Unpaired => true,
            Pairing::Paired(expected) => match presented_key {
                Some(presented) => constant_time_eq(presented.as_bytes(), expected.as_bytes()),
                None => false,
            },
        }
    }
}

/// Compare two byte slices in time independent of where they first differ, so
/// the bearer-secret check on the LAN edge leaks no timing signal about a
/// partial match. A length mismatch is rejected up front (the length of the
/// stored key is not itself a secret); equal-length slices are then folded
/// together with a running difference accumulator that always visits every
/// byte. The compiler is told via `std::hint::black_box` not to short-circuit
/// the loop once a difference is seen.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff: u8 = 0;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    std::hint::black_box(diff) == 0
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

/// Load the pairing posture from a `pairing.json`. An absent file, an
/// unreadable file, or a state that is not `paired:true` with an `api_key` is
/// treated as unpaired (open), matching the agent: when not paired, access is
/// open.
fn load_pairing(path: &std::path::Path) -> Pairing {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Pairing::Unpaired;
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
        return Pairing::Unpaired;
    };
    let paired = value
        .get("paired")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let key = value.get("api_key").and_then(|v| v.as_str());
    match (paired, key) {
        (true, Some(k)) if !k.is_empty() => Pairing::Paired(k.to_string()),
        _ => Pairing::Unpaired,
    }
}

/// A fixed-window token-bucket rate limiter for the TCP edge. Each refill
/// window grants `capacity` tokens; a request consumes one. When the bucket is
/// empty within a window the request is rejected with 429. One shared bucket
/// guards the whole TCP edge (the read budget is per-agent, not per-route),
/// which is enough to stop a runaway client from pinning the box.
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

    /// The default read budget: a generous per-second rate for an observability
    /// surface, matching the FastAPI read posture.
    pub fn default_read() -> Self {
        Self::new(30, Duration::from_secs(1))
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
        assert!(state.authorize("/v1/query", None));
        assert!(state.authorize("/v1/query", Some("anything")));
    }

    #[test]
    fn paired_requires_the_exact_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_pairing(dir.path(), r#"{"paired": true, "api_key": "ados_secret"}"#);
        let state = PairingState::with_path(path);
        assert_eq!(state.current(), Pairing::Paired("ados_secret".to_string()));
        assert!(state.authorize("/v1/query", Some("ados_secret")));
        assert!(!state.authorize("/v1/query", Some("wrong")));
        assert!(!state.authorize("/v1/query", None));
    }

    #[test]
    fn public_paths_are_open_even_when_paired() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_pairing(dir.path(), r#"{"paired": true, "api_key": "k"}"#);
        let state = PairingState::with_path(path);
        assert!(state.authorize("/v1/healthz", None));
        assert!(state.authorize("/v1/openapi.json", None));
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
    fn paired_key_check_is_a_constant_time_comparison() {
        // The authorize() Paired branch goes through the constant-time compare:
        // the exact key passes, a same-length wrong key and a prefix match both
        // fail, and an absent key fails.
        let dir = tempfile::tempdir().unwrap();
        let path = write_pairing(dir.path(), r#"{"paired": true, "api_key": "ados_secret"}"#);
        let state = PairingState::with_path(path);
        assert!(state.authorize("/v1/query", Some("ados_secret")));
        assert!(!state.authorize("/v1/query", Some("ados_secre1")));
        assert!(!state.authorize("/v1/query", Some("ados_secret_with_more")));
        assert!(!state.authorize("/v1/query", Some("ados")));
        assert!(!state.authorize("/v1/query", None));
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
