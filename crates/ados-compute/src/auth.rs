//! Pairing auth for the compute job API.
//!
//! The compute node mirrors the agent's data-plane posture (the shared
//! [`ados_protocol::pairing_posture`] primitives the HTTP control surface and the
//! MAVLink WS proxy already use):
//!
//! - **Unpaired ⇒ open.** A fresh node has no key; LAN presence is the gate, the
//!   same stance the pairing flow takes, so a GCS can discover and claim it.
//! - **Paired + on-box ⇒ open.** A loopback caller that was not relayed by a
//!   proxy already holds shell-level privilege that exceeds API auth.
//! - **Paired + off-box ⇒ `X-ADOS-Key` required**, compared in constant time.
//!
//! The gate is applied uniformly to every route (including
//! `/api/compute/status`): there is no public exempt set, because an unpaired
//! node is already fully open and a paired node should not leak its job/cluster
//! state to an unauthenticated LAN caller. With this gate in place the daemon may
//! bind a non-loopback address safely.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::{ConnectInfo, Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::Json;

use ados_protocol::pairing_posture::{
    data_plane_access, is_on_box, load_pairing, Access, Pairing, FORWARDED_HEADERS,
};

/// Default pairing-state path: the agent's `pairing.json`.
pub const DEFAULT_PAIRING_PATH: &str = "/etc/ados/pairing.json";

/// The header an off-box caller presents the pairing key in.
const KEY_HEADER: &str = "x-ados-key";

/// How long a loaded pairing state is trusted before the file is re-read. Short
/// enough that a pair/unpair is honoured within a couple of requests, long enough
/// that a request burst does not stat the file every time.
const PAIRING_TTL: Duration = Duration::from_secs(2);

/// Reads `pairing.json` and answers the auth question, with a short-TTL cache so
/// the file is not stat-ed on every request. Held behind an `Arc` and shared by
/// the middleware.
pub struct PairingGate {
    path: PathBuf,
    cache: Mutex<Cache>,
}

struct Cache {
    loaded: Pairing,
    at: Instant,
    primed: bool,
}

impl PairingGate {
    /// Build a gate against the agent's standard pairing path.
    pub fn new() -> Self {
        Self::with_path(PathBuf::from(DEFAULT_PAIRING_PATH))
    }

    /// Build a gate against an explicit path (the daemon's env override, tests).
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
}

impl Default for PairingGate {
    fn default() -> Self {
        Self::new()
    }
}

/// A fixed-window token bucket that guards the off-box (LAN) edge so a runaway
/// caller cannot pin the node. On-box callers are never limited (they already
/// hold shell-level privilege); one shared bucket guards the whole edge, mirroring
/// the agent's HTTP control surface.
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

    /// The default off-box budget: a generous per-second rate that status polling
    /// and job bursts both fit under.
    pub fn default_control() -> Self {
        Self::new(120, Duration::from_secs(1))
    }

    /// Try to admit one request; `false` when the window's budget is exhausted.
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

/// The auth state the job-API middleware carries: the pairing gate plus the
/// off-box rate limiter.
pub struct ComputeAuth {
    pub gate: PairingGate,
    pub limiter: RateLimiter,
}

impl ComputeAuth {
    /// Build the auth state against a pairing-state path, with the default
    /// off-box rate budget.
    pub fn new(pairing_path: PathBuf) -> Self {
        Self {
            gate: PairingGate::with_path(pairing_path),
            limiter: RateLimiter::default_control(),
        }
    }
}

/// Axum middleware: admit a request per the data-plane pairing posture, else 401
/// (or 429 when the off-box budget is spent).
///
/// The peer address is read from [`ConnectInfo`] (present when the router is
/// served with `into_make_service_with_connect_info`); when absent (e.g. a
/// `oneshot` test that injects no peer) the caller is treated as off-box, the
/// conservative default. A proxy/tunnel that SETS one of [`FORWARDED_HEADERS`]
/// (any HTTP reverse proxy or Cloudflare-style tunnel) is denied loopback trust
/// so it cannot impersonate an on-box caller. A raw L4 tunnel that sets no such
/// header is, like a co-resident local process, on-box by the same trust model:
/// reaching loopback already implies host access that exceeds API auth.
pub async fn require_pairing(
    State(auth): State<Arc<ComputeAuth>>,
    req: Request,
    next: Next,
) -> Response {
    let peer_is_loopback = req
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ci| ci.0.ip().is_loopback())
        .unwrap_or(false);
    let has_forwarding_header = FORWARDED_HEADERS
        .iter()
        .any(|h| req.headers().contains_key(*h));
    let on_box = is_on_box(peer_is_loopback, has_forwarding_header);

    // Rate-limit the off-box edge only; on-box callers are trusted + unlimited.
    if !on_box && !auth.limiter.check() {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            Json(serde_json::json!({ "error": "rate limited" })),
        )
            .into_response();
    }

    let presented_key = req.headers().get(KEY_HEADER).and_then(|v| v.to_str().ok());
    match data_plane_access(&auth.gate.current(), on_box, presented_key) {
        Access::Accept => next.run(req).await,
        // A terse, state-independent body: the 401 itself is the only signal.
        Access::Unauthorized => (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({ "error": "unauthorized" })),
        )
            .into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn paired_gate(dir: &std::path::Path) -> Arc<PairingGate> {
        let path = dir.join("pairing.json");
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(br#"{"paired": true, "api_key": "ados_secret"}"#)
            .unwrap();
        Arc::new(PairingGate::with_path(path))
    }

    #[test]
    fn unpaired_gate_reads_open() {
        let dir = tempfile::tempdir().unwrap();
        let gate = PairingGate::with_path(dir.path().join("absent.json"));
        assert_eq!(gate.current(), Pairing::Unpaired);
    }

    #[test]
    fn paired_gate_reads_the_key() {
        let dir = tempfile::tempdir().unwrap();
        let gate = paired_gate(dir.path());
        assert_eq!(gate.current(), Pairing::Paired("ados_secret".into()));
    }
}
