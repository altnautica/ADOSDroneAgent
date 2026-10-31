//! Authentication and rate limiting for the LAN listener.
//!
//! The same Router is served on two edges. The trusted local Unix socket
//! carries no auth and no rate limit: anything on-box that can open the socket
//! is already inside the trust boundary. The LAN TCP edge enforces the agent's
//! HTTP auth posture:
//!
//! - **Unpaired ⇒ all routes open.** A fresh agent has no key; physical presence
//!   on the LAN is the gate, the same stance the pairing-claim flow takes.
//! - **Paired ⇒ `X-ADOS-Key` required** and must equal the stored pairing key.
//!
//! On top of the pairing gate the edge applies two trust shortcuts:
//!
//! - **Public paths** ([`is_public`]) are open on both edges even when paired,
//!   so a fresh GCS can read `/api/version` and walk the pairing handshake
//!   before it holds a key, and a watchdog can hit `/healthz`.
//! - **On-box loopback trust** ([`CallerClass::OnBox`]): a request whose peer
//!   address is loopback and that carries no proxy-forwarding header is the
//!   local operator, who already holds shell-level privilege that exceeds API
//!   auth. This is free on the Unix socket (which never installs the gate); for
//!   the loopback-TCP case the edge classifies the caller once
//!   ([`ados_protocol::pairing_posture::classify_caller`]). A proxy or tunnel
//!   that terminates on 127.0.0.1 carries a forwarding header and is classified
//!   remote, so it can never impersonate an on-box caller to bypass
//!   authentication.
//!
//! The pairing state is the agent's `pairing.json` (`{ "paired": bool,
//! "api_key": "..." }`). It is read fresh on each request through a short-TTL
//! cache so a pair/unpair that happens while the daemon runs is honoured without
//! a restart, while a burst of requests does not stat the file every time.
//!
//! A token-bucket rate limiter caps the TCP edge so a runaway client cannot pin
//! the box; the Unix edge is unlimited.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::config::ControlSecurityConfig;
use crate::security_state::SecurityState;

// The pairing-posture primitives are shared with the direct MAVLink proxies, so
// they live once in the protocol crate. Re-exported here under the names this
// surface (and its callers) already use, so the HTTP edge keeps a single import
// point for the auth posture.
pub use ados_protocol::pairing_posture::{constant_time_eq, load_pairing, CallerClass, Pairing};

mod gates;
pub use gates::*;

/// Why a request path cannot be used to make an authorization decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathRejection {
    /// A `%` survived one round of decoding, i.e. the path was encoded twice.
    DoubleEncoded,
    /// A `%` sequence that is not two hex digits.
    MalformedEscape,
    /// A `.` or `..` segment.
    DotSegment,
    /// An empty segment (`//`), which collapses differently in different routers.
    EmptySegment,
    /// A control byte or a NUL, decoded or literal.
    ControlByte,
    /// The decoded bytes are not UTF-8.
    NotUtf8,
}

impl PathRejection {
    /// Operator-facing reason. Deliberately generic: it says the path was
    /// refused, not which check caught it, so the response is not a probe
    /// oracle for the normalizer itself.
    pub const MESSAGE: &'static str =
        "The request path is not in canonical form and cannot be authorized.";
}

/// Decode a request path ONCE into the single string every authorization
/// predicate is then asked about.
///
/// Why this exists. Every gate on this edge — the relay denylist, the
/// unpaired-node gate, the public-path list, the native/proxied split and the
/// proxied API-key decision — used to call `request.uri().path()` for itself.
/// That string is the RAW target from the request line, so `%61pi` is six
/// characters that match no literal in any of those lists. The request then
/// misses `is_native`, falls through to the reverse proxy, and the residual
/// FastAPI — which decodes before routing, as every WSGI/ASGI server does —
/// serves `/api/v1/setup/reboot` to a caller who presented no credential.
/// `//api/...` and `/api/%2e%2e/...` are the same bug wearing different hats.
///
/// So: decode exactly once, then REFUSE anything still ambiguous rather than
/// trying to canonicalize it. A path that needs a second decode, or that
/// carries a dot segment, an empty segment or a control byte, has no
/// legitimate caller — every real client emits a canonical path — and
/// refusing is the only answer that cannot differ from what the next hop
/// will do with it.
///
/// Returning a decoded string rather than validating in place matters: the
/// decision must be made on the bytes the LAST hop will route on, and the
/// caller must thread this one value everywhere so no predicate can quietly
/// re-read the raw path and disagree.
pub fn decision_path(raw: &str) -> Result<String, PathRejection> {
    let bytes = raw.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'%' {
            let hi = bytes
                .get(i + 1)
                .and_then(|c| (*c as char).to_digit(16))
                .ok_or(PathRejection::MalformedEscape)?;
            let lo = bytes
                .get(i + 2)
                .and_then(|c| (*c as char).to_digit(16))
                .ok_or(PathRejection::MalformedEscape)?;
            out.push(((hi << 4) | lo) as u8);
            i += 3;
        } else {
            out.push(b);
            i += 1;
        }
    }

    let decoded = String::from_utf8(out).map_err(|_| PathRejection::NotUtf8)?;

    // One decode only. A surviving `%` means the caller encoded twice, which
    // no client does by accident and which the next hop may well decode again.
    if decoded.contains('%') {
        return Err(PathRejection::DoubleEncoded);
    }
    if decoded.bytes().any(|b| b < 0x20 || b == 0x7f) {
        return Err(PathRejection::ControlByte);
    }
    for segment in decoded.split('/').skip(1) {
        if segment.is_empty() {
            // Trailing `/` is the one legitimate empty segment.
            if decoded.ends_with('/') && decoded.matches("//").count() == 0 {
                continue;
            }
            return Err(PathRejection::EmptySegment);
        }
        if segment == "." || segment == ".." {
            return Err(PathRejection::DotSegment);
        }
    }
    Ok(decoded)
}

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
    /// The live `security:` posture: its `api.api_key`, when set, is a second
    /// credential accepted everywhere the pairing key is. Read per request, so
    /// rotating it takes effect without a restart. `None` accepts none.
    security: Option<Arc<SecurityState>>,
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
            security: None,
        }
    }

    /// Also accept the operator-configured `security.api.api_key`, read live.
    pub fn with_security(mut self, security: Arc<SecurityState>) -> Self {
        self.security = Some(security);
        self
    }

    /// The live security config, all-defaults (no configured key) when none
    /// is attached or the config has never been readable.
    fn security_config(&self) -> Arc<ControlSecurityConfig> {
        self.security
            .as_ref()
            .map(|s| s.config_or_default())
            .unwrap_or_default()
    }

    /// Whether `presented` is a credential this node accepts: the pairing key
    /// or the configured key. Every surface that checks a key calls this, so a
    /// credential is never accepted on one route and refused on another.
    pub fn credential_valid(&self, presented: Option<&str>) -> bool {
        let configured = self.security_config();
        credential_matches(&self.current(), &configured.security.api.api_key, presented)
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

    /// Drop the cached posture so the next read goes to the file. The pairing
    /// writers (claim, accept, unpair) call this after a successful write, so
    /// the browser that just claimed is served with its new key at once and an
    /// unpaired key stops working at once, rather than up to one TTL later.
    pub fn invalidate(&self) {
        self.cache.lock().unwrap_or_else(|p| p.into_inner()).primed = false;
    }

    /// Decide a request: `true` to pass, `false` to reject with 401. A public
    /// path is always allowed; an unpaired agent allows everything (the
    /// unpaired caller gate, [`unpaired_decision`], has already run); a paired
    /// agent requires the exact key. The on-box shortcut is applied by the edge
    /// before this is consulted, so this only models a caller that is not
    /// on-box.
    pub fn authorize(&self, path: &str, presented_key: Option<&str>) -> bool {
        if is_public(path) {
            return true;
        }
        let pairing = self.current();
        if pairing == Pairing::Unpaired {
            return true;
        }
        let configured = self.security_config();
        credential_matches(&pairing, &configured.security.api.api_key, presented_key)
    }
}

/// The one credential check: `presented` is non-empty and equals the pairing
/// key of a paired node or the operator-configured key, compared in constant
/// time.
///
/// An unpaired node has no pairing key, so on its own an arbitrary string is
/// never a credential: callers that open the unpaired data plane do so by
/// their own posture, not by treating every key as valid here.
pub fn credential_matches(
    pairing: &Pairing,
    configured_key: &str,
    presented: Option<&str>,
) -> bool {
    let Some(presented) = presented.filter(|k| !k.is_empty()) else {
        return false;
    };
    if !configured_key.is_empty()
        && constant_time_eq(presented.as_bytes(), configured_key.as_bytes())
    {
        return true;
    }
    match pairing {
        Pairing::Paired(key) => constant_time_eq(presented.as_bytes(), key.as_bytes()),
        Pairing::Unpaired | Pairing::Unreadable => false,
    }
}

impl Default for PairingState {
    fn default() -> Self {
        Self::new()
    }
}

/// Who a request is charged to: the caller's address, with a global IPv6
/// address reduced to its /64 (one host controls a whole /64, so keying on the
/// full address would hand it 2^64 fresh budgets) and an IPv4-mapped address
/// read as the IPv4 address. Link-local (`fe80::/10`) and unique-local
/// (`fc00::/7`) addresses are keyed per /128: every host on a LAN shares that
/// /64, so collapsing it let one host exhaust everyone's budget. `None` is a
/// caller with no address (the Unix socket).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PeerKey(Option<std::net::IpAddr>);

impl PeerKey {
    /// The key for a caller at `ip`.
    pub fn of(ip: Option<std::net::IpAddr>) -> Self {
        use std::net::{IpAddr, Ipv6Addr};
        Self(ip.map(|ip| match ip.to_canonical() {
            IpAddr::V6(v6) => {
                let s = v6.segments();
                let link_local = s[0] & 0xffc0 == 0xfe80;
                let unique_local = s[0] & 0xfe00 == 0xfc00;
                if link_local || unique_local {
                    IpAddr::V6(v6)
                } else {
                    IpAddr::V6(Ipv6Addr::new(s[0], s[1], s[2], s[3], 0, 0, 0, 0))
                }
            }
            v4 => v4,
        }))
    }
}

/// Past this many tracked callers, expired windows are dropped; a new caller
/// that still finds the table full is refused rather than growing it.
const MAX_RATE_PEERS: usize = 4096;

/// A fixed-window request budget per caller for the TCP edge. Each caller gets
/// `capacity` requests per `window`; past that it is answered 429 until its
/// window rolls over. Keyed per caller rather than one shared bucket, so one
/// host flooding the edge exhausts its own budget and nobody else's.
pub struct RateLimiter {
    capacity: u32,
    window: Duration,
    peers: Mutex<std::collections::HashMap<PeerKey, RateState>>,
}

struct RateState {
    tokens: u32,
    window_start: Instant,
}

impl RateLimiter {
    /// A limiter granting each caller `capacity` requests per `window`.
    pub fn new(capacity: u32, window: Duration) -> Self {
        Self {
            capacity,
            window,
            peers: Mutex::new(std::collections::HashMap::new()),
        }
    }

    /// The default control-surface budget per caller: a generous per-second
    /// rate. A GCS, a dashboard and a cockpit on one host fit under it together.
    pub fn default_control() -> Self {
        Self::new(60, Duration::from_secs(1))
    }

    /// Try to admit one request from `peer`. Returns `true` when admitted,
    /// `false` when that caller's window budget is exhausted.
    pub fn check(&self, peer: PeerKey) -> bool {
        let mut peers = self.peers.lock().unwrap_or_else(|p| p.into_inner());
        if !peers.contains_key(&peer) && peers.len() >= MAX_RATE_PEERS {
            let window = self.window;
            peers.retain(|_, s| s.window_start.elapsed() < window);
            if peers.len() >= MAX_RATE_PEERS {
                return false;
            }
        }
        let s = peers.entry(peer).or_insert_with(|| RateState {
            tokens: self.capacity,
            window_start: Instant::now(),
        });
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
mod tests;
