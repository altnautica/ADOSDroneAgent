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
use std::sync::Mutex;
use std::time::{Duration, Instant};

// The pairing-posture primitives are shared with the direct MAVLink proxies, so
// they live once in the protocol crate. Re-exported here under the names this
// surface (and its callers) already use, so the HTTP edge keeps a single import
// point for the auth posture.
pub use ados_protocol::pairing_posture::{constant_time_eq, load_pairing, CallerClass, Pairing};

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
    /// The operator-configured `security.api.api_key`, empty when none. A
    /// second credential accepted everywhere the pairing key is.
    configured_key: String,
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
            configured_key: String::new(),
        }
    }

    /// Also accept the operator-configured `security.api.api_key`.
    pub fn with_configured_key(mut self, key: impl Into<String>) -> Self {
        self.configured_key = key.into();
        self
    }

    /// Whether `presented` is a credential this node accepts: the pairing key
    /// or the configured key. Every surface that checks a key calls this, so a
    /// credential is never accepted on one route and refused on another.
    pub fn credential_valid(&self, presented: Option<&str>) -> bool {
        credential_matches(&self.current(), &self.configured_key, presented)
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
        credential_matches(&pairing, &self.configured_key, presented_key)
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

/// The header the relay stamps on a request that crossed the radio.
pub const RELAYED_HEADER: &str = "x-ados-relayed";

/// Paths a relayed caller may never reach, regardless of trust posture.
///
/// A relayed request carries a per-pair relay ticket bound to exactly that
/// request (see `ados_protocol::relay_ticket`), verified on the drone before it
/// reaches loopback. It therefore carries the linked ground station's full
/// authority over the node's operating surface: telemetry, parameters,
/// configuration, services, plugins, reboots. What protects an airborne
/// aircraft from an ill-timed restart is the armed interlock, which applies to
/// every caller, not this list.
///
/// This list holds only what changes WHO the node trusts — the paths that hand
/// out or replace a credential, or move the node's trust root:
///
/// - **Pairing.** `claim` mints the node's master LAN key, `code` publishes
///   what a claim needs, `unpair` clears the pairing, `accept` binds a cloud
///   account. Over the radio, together they convert radio range into a
///   standing API key that works from anywhere on the network long after the
///   caller is out of range.
/// - **Credential issuance** — scoped tokens, the dashboard PIN and plugin
///   capability tokens, each a second standing credential.
/// - **Radio pairing** — moving the node onto or off a fleet key.
/// - **Flight-controller signing** — removing the FC's link credential.
/// - **Trust-root setup** — factory and setup reset (which wipe pairing),
///   cloud posture, the combined setup apply and profile writes that reach it,
///   and remote access.
///
/// `PUT /api/config` stays reachable, but not for the keys that are trust
/// roots: see [`relay_config_key_forbidden`].
///
/// Refused at the edge rather than per-handler so the rule holds for native and
/// proxied routes alike, and cannot be missed when a route moves between them.
///
/// Every literal here is asserted against the committed route table
/// (`docs/api-surface.md`) by
/// [`tests::every_denylisted_path_is_a_route_something_actually_serves`]. A
/// denylist entry that matches no served path is worse than no entry: it reads
/// as covered while the real path is wide open.
/// Takes the normalized [`decision_path`], never `request.uri().path()`.
/// Matching the raw target here is how `/api/pairing/%75npair` walked past a
/// list that names `/api/pairing/unpair`.
///
/// Prefix, not equality. An exact `matches!` guards one spelling of a route
/// and nothing beneath it, so a sub-path a router happens to serve — a
/// trailing slash, a path parameter, a future `/api/plugins/install/resume` —
/// is open while the list reads as covering it. Every entry below names a
/// whole subtree that must not be reachable over the radio.
pub fn relay_forbidden(path: &str) -> bool {
    RELAY_FORBIDDEN_PATHS
        .iter()
        .any(|denied| path_covers(denied, path))
}

/// Whether `denied` covers `path`: the same route, or anything beneath it. A
/// `{name}` segment in `denied` matches any one non-empty segment, so a
/// path-parameter route is named once as its template.
///
/// `"/api/plugins/install"` covers `/api/plugins/install`,
/// `/api/plugins/install/` and `/api/plugins/install/resume`, but NOT
/// `/api/plugins/installer` — a prefix test without the boundary check would
/// deny an unrelated sibling and, worse, would let someone believe a subtree
/// is covered because its name happens to share a prefix.
fn path_covers(denied: &str, path: &str) -> bool {
    let mut actual = path.split('/');
    for want in denied.split('/') {
        let Some(seg) = actual.next() else {
            return false;
        };
        let is_param = want.len() >= 2 && want.starts_with('{') && want.ends_with('}');
        if is_param {
            if seg.is_empty() {
                return false;
            }
        } else if seg != want {
            return false;
        }
    }
    true
}

/// Every subtree [`relay_forbidden`] refuses, as data. The predicate reads
/// this list directly, so the two cannot drift; the route-table test still
/// enumerates it to assert each entry names a path something actually serves.
/// A denylist entry that matches no served path is worse than no entry: it
/// reads as covered while the real path is wide open.
pub const RELAY_FORBIDDEN_PATHS: &[&str] = &[
    "/api/pairing/claim",
    "/api/pairing/code",
    "/api/pairing/unpair",
    "/api/pairing/accept",
    "/api/mcp/tokens",
    "/api/mcp/revoke",
    "/api/dashboard/pin/set",
    "/api/dashboard/pin/clear",
    "/api/plugins/capability-token",
    "/api/wfb/pair/local-bind",
    "/api/wfb/pair/unpair",
    "/api/v1/ground-station/wfb/pair",
    "/api/mavlink/signing/disable-on-fc",
    "/api/v1/setup/reset",
    "/api/v1/setup/cloud-choice",
    "/api/v1/setup/apply",
    "/api/v1/setup/profile",
    "/api/v1/setup/remote-access/cloudflare",
    "/api/v1/ground-station/factory-reset",
];

/// The config write route whose body a relayed request is checked against.
pub const RELAY_CONFIG_WRITE_PATH: &str = "/api/config";

/// Whether a relayed `PUT /api/config` may not write `key`. One definition,
/// shared with the config tunnel, so both radio lanes refuse the same
/// trust-root keys.
pub use ados_protocol::pairing_posture::relay_config_key_forbidden;

/// The PIN login paths: public, but charged to the caller's request budget at
/// the edge, since they are the public paths a guesser loops on.
pub fn is_pin_login(path: &str) -> bool {
    matches!(path, "/api/dashboard/pin/verify" | "/api/dashboard/pin/set")
}

/// The endpoints that are public on both edges (no key, no rate limit even on
/// TCP) so a fresh GCS can read the version, walk the local pairing handshake
/// before it holds a key, and a liveness probe can always hit `/healthz`. This
/// is the native surface's exempt set, narrower than the Python middleware's
/// (no setup/static paths live here). `/api/time` is deliberately NOT public.
///
/// The ground-station WebSocket relays are exempt here too: a WebSocket
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
            // Public, but an unpaired node refuses it to a remote caller (see
            // `unpaired_decision`); a paired node answers it with a 409.
            | "/api/pairing/claim"
            | "/api/version"
            // Dashboard-access PIN gate: an off-box paired browser must reach the
            // status read + the verify (login) + the set (the handler decides
            // who may set a first PIN) before it holds any credential. `set`
            // authorizes IN THE HANDLER;
            // `verify` is rate-limited + lockout-throttled in the store. `clear`
            // is deliberately NOT here — it stays behind the normal gate so only
            // an on-box or key-bearing caller resets the PIN.
            | "/api/dashboard/pin/status"
            | "/api/dashboard/pin/verify"
            | "/api/dashboard/pin/set"
            | "/api/v1/ground-station/ws/uplink"
            | "/api/v1/ground-station/pic/events"
            | "/api/v1/ground-station/ws/mesh"
            | "/api/v1/ground-station/ws/buttons"
    )
}

/// The operator's browser UIs and the static assets they are built from.
///
/// These carry no data of their own — they are the shell that then asks for it
/// through `/api/*`, and every one of those calls keeps exactly the posture it
/// had. Refusing the shell as well bought nothing and cost the operator the only
/// surface that could tell them what was wrong: an unpaired node returned a raw
/// JSON 403 to a browser navigation, so the page could not load, could not show
/// the pairing code it already serves publicly on `/api/pairing/info`, and could
/// not even be reloaded to pick up a newer build. A browser left holding an old
/// cached bundle had no way back, because the fetch that would replace it was
/// refused too.
///
/// Deliberately not "anything outside `/api/`". `/whep` is a live video stream,
/// `/hls/` is recorded video, `/ws*` are live streams and `/docs` enumerates the
/// route surface; all sit outside `/api/` and all stay refused while unpaired.
/// Beyond the fixed asset list, a dashboard client route (`/settings/network`)
/// is admitted so a browser reload or deep link loads the shell instead of a
/// raw JSON 403: a path counts as a client route only when it has no file
/// extension, sits under none of those data prefixes, and is not served by any
/// native route (the router would fall back to the dashboard bundle for it).
/// The method gate (`GET`/`HEAD` only) lives in [`unpaired_decision`].
pub fn is_operator_ui(path: &str) -> bool {
    // The on-box cockpit and everything under it.
    if path == "/cockpit" || path.starts_with("/cockpit/") {
        return true;
    }
    // The browser dashboard is mounted at the root, so its entry point is `/`
    // and its build output sits directly beneath.
    if path == "/" || path.starts_with("/assets/") {
        return true;
    }
    if matches!(
        path,
        "/index.html" | "/brand.svg" | "/favicon.ico" | "/manifest.webmanifest"
    ) {
        return true;
    }
    is_spa_client_route(path)
}

/// Path prefixes outside `/api/` that carry data and are never the shell.
const NON_UI_PREFIXES: [&str; 6] = ["/api", "/whep", "/hls", "/ws", "/healthz", "/docs"];

/// A dashboard client-side route: extension-less, under no data prefix, and
/// answered by the router with the dashboard bundle.
fn is_spa_client_route(path: &str) -> bool {
    if !path.starts_with('/') {
        return false;
    }
    let under_data_prefix = NON_UI_PREFIXES.iter().any(|prefix| {
        path.strip_prefix(prefix)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
    });
    if under_data_prefix {
        return false;
    }
    let last = path.rsplit('/').next().unwrap_or("");
    if last.contains('.') {
        return false;
    }
    crate::routing::classify(&http::Method::GET, path) == crate::routing::RouteMode::OperatorUi
}

/// The unpaired-node gate's outcome for a request, granular enough to express the
/// private-LAN PIN scope: a private-LAN browser is not flatly refused on a DATA
/// route — it is trusted for the operator-UI scope and PIN-gated instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnpairedDecision {
    /// Serve the request: the node is paired, the route is operator UI or a
    /// public route the caller may reach, or the caller is on-box or a
    /// first-boot lifeline.
    Allow,
    /// An operator-LAN caller requesting a DATA route on an UNPAIRED node:
    /// served only when the caller presents a valid dashboard PIN session
    /// (minted via `/api/dashboard/pin/{set,verify}`), else refused.
    RequirePin,
    /// A remote caller (a public-WAN host, anything relayed through a proxy or
    /// tunnel, or an unidentifiable peer): refuse with 403.
    Refuse,
}

/// The pairing claim path: public (a fresh operator holds no key yet), but it
/// hands out the node's master key, so an unpaired node answers it only for a
/// caller on the device's own networks.
const CLAIM_PATH: &str = "/api/pairing/claim";

/// The unpaired-node gate: whether a request is served outright, requires a PIN
/// session, or is refused. A pure function of the path, the pairing state and
/// the [`CallerClass`] the edge computed, so the decision is testable.
///
/// While unpaired:
///
/// - the operator UI shell is served to anyone (it carries no data);
/// - the pairing claim is served to every caller except a remote one: it mints
///   the key that makes the caller the node's owner, and a remote caller (an
///   internet request arriving through a tunnel, a public-WAN host) must not be
///   able to claim a unit its operator has not reached yet;
/// - the other public routes are served to anyone;
/// - a DATA route is served to the local operator and the first-boot
///   lifelines, PIN-gated for an operator-LAN caller, and refused otherwise.
pub fn unpaired_decision(
    method: &http::Method,
    path: &str,
    unpaired: bool,
    caller: CallerClass,
) -> UnpairedDecision {
    if !unpaired {
        return UnpairedDecision::Allow;
    }
    if (method == http::Method::GET || method == http::Method::HEAD) && is_operator_ui(path) {
        return UnpairedDecision::Allow;
    }
    if path == CLAIM_PATH {
        return match caller {
            CallerClass::Remote => UnpairedDecision::Refuse,
            CallerClass::OnBox | CallerClass::Lifeline | CallerClass::OperatorLan => {
                UnpairedDecision::Allow
            }
        };
    }
    if is_public(path) {
        return UnpairedDecision::Allow;
    }
    match caller {
        CallerClass::OnBox | CallerClass::Lifeline => UnpairedDecision::Allow,
        CallerClass::OperatorLan => UnpairedDecision::RequirePin,
        CallerClass::Remote => UnpairedDecision::Refuse,
    }
}

/// Who a request is charged to: the caller's address, with an IPv6 address
/// reduced to its /64 (one host controls a whole /64, so keying on the full
/// address would hand it 2^64 fresh budgets) and an IPv4-mapped address read
/// as the IPv4 address. `None` is a caller with no address (the Unix socket).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PeerKey(Option<std::net::IpAddr>);

impl PeerKey {
    /// The key for a caller at `ip`.
    pub fn of(ip: Option<std::net::IpAddr>) -> Self {
        use std::net::{IpAddr, Ipv6Addr};
        Self(ip.map(|ip| match ip.to_canonical() {
            IpAddr::V6(v6) => {
                let s = v6.segments();
                IpAddr::V6(Ipv6Addr::new(s[0], s[1], s[2], s[3], 0, 0, 0, 0))
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
mod tests {
    use super::*;
    use std::io::Write;

    const GET: http::Method = http::Method::GET;
    const HEAD: http::Method = http::Method::HEAD;
    const POST: http::Method = http::Method::POST;

    /// Claiming a device over the LAN is the documented local-first flow, so
    /// the pairing handshake must stay public. The unpaired-peer filter refuses
    /// non-public routes from an ordinary LAN peer; if these were gated too, a
    /// fresh device could not be paired from the network it sits on - trading an
    /// exposure for an unpairable unit.
    #[test]
    fn the_pairing_handshake_stays_public_so_lan_claiming_still_works() {
        for path in [
            "/api/pairing/info",
            "/api/pairing/code",
            "/api/pairing/claim",
            "/healthz",
        ] {
            assert!(
                is_public(path),
                "{path} must stay reachable to claim a device"
            );
        }
        // The routes that actually command the aircraft are NOT public, so on an
        // unpaired device they fall to the peer filter.
        assert!(!is_public("/api/command"));
        assert!(!is_public("/api/config"));
    }

    /// A radio-only drone has no LAN pairing, so its claim would hand a
    /// relayed caller the master key outright; every pairing mutation and the
    /// code a claim needs stay off the relay.
    #[test]
    fn a_relayed_caller_reaches_no_pairing_route_that_mints_or_clears_a_key() {
        for path in [
            "/api/pairing/claim",
            "/api/pairing/code",
            "/api/pairing/unpair",
            "/api/pairing/accept",
        ] {
            assert!(relay_forbidden(path), "{path}");
        }
        assert!(
            is_public("/api/pairing/claim"),
            "claim stays public on the LAN — a fresh operator holds no key yet"
        );
        assert!(!relay_forbidden("/api/pairing/info"));
    }

    #[test]
    fn credential_issuing_paths_are_refused_over_the_relay() {
        for path in [
            "/api/mcp/tokens",
            "/api/mcp/revoke",
            "/api/dashboard/pin/set",
            "/api/dashboard/pin/clear",
            "/api/plugins/capability-token",
        ] {
            assert!(relay_forbidden(path), "{path} hands out a credential");
        }
    }

    #[test]
    fn trust_root_paths_are_refused_over_the_relay() {
        for path in [
            "/api/wfb/pair/unpair",
            "/api/wfb/pair/local-bind",
            "/api/v1/ground-station/wfb/pair",
            "/api/v1/setup/reset",
            "/api/v1/setup/cloud-choice",
            // `apply` carries a cloud choice and a profile with restart in one
            // body, so it reaches everything `cloud-choice` does.
            "/api/v1/setup/apply",
            "/api/v1/setup/profile",
            "/api/v1/setup/remote-access/cloudflare",
            // The path the ground-station router actually registers
            // (`APIRouter(prefix="/v1/ground-station")` + `@router.post(
            // "/factory-reset")`, mounted under `/api`). The literal used to
            // read `.../ui/factory-reset`, which no router has ever served —
            // so this assertion passed while the real route was reachable
            // over the radio with full on-box authority.
            "/api/v1/ground-station/factory-reset",
        ] {
            assert!(relay_forbidden(path), "{path} must not cross the relay");
        }
    }

    /// The old literal must stay refused-by-absence: it names nothing, so if it
    /// ever comes back as a denylist entry the route-table guard below fails.
    /// Asserting it here as well documents that the typo is gone rather than
    /// merely moved.
    #[test]
    fn the_mounted_factory_reset_path_is_the_one_guarded() {
        assert!(relay_forbidden("/api/v1/ground-station/factory-reset"));
        assert!(
            !RELAY_FORBIDDEN_PATHS.contains(&"/api/v1/ground-station/ui/factory-reset"),
            "the unmounted `/ui/` spelling guarded nothing and must not return"
        );
    }

    /// A relayed caller must be refused the reset outright. This exercises the
    /// edge's actual decision — presence of the relay header plus the denylist
    /// — rather than the predicate alone, because the predicate being right is
    /// worth nothing if the edge stops consulting it.
    #[test]
    fn factory_reset_is_refused_over_the_relay_at_the_edge() {
        let mut headers = axum::http::HeaderMap::new();
        headers.insert(RELAYED_HEADER, "1".parse().unwrap());
        let path = "/api/v1/ground-station/factory-reset";
        let is_relayed = headers.contains_key(RELAYED_HEADER);
        assert!(
            is_relayed && relay_forbidden(path),
            "a request carrying {RELAYED_HEADER} must be refused {path}"
        );
        // The same path with no relay header is ordinary on-LAN operation and
        // stays reachable — the refusal is about the lane, not the route.
        let direct = axum::http::HeaderMap::new();
        assert!(!direct.contains_key(RELAYED_HEADER));
    }

    /// A denylist entry that matches no served path is the defect this guard
    /// exists for: it reads as protection while the real route is open. Every
    /// literal must resolve against the committed route table, which covers
    /// both the native surface and the residual FastAPI one.
    #[test]
    fn every_denylisted_path_is_a_route_something_actually_serves() {
        let table = include_str!("../../../docs/api-surface.md");
        let served: std::collections::HashSet<&str> = table
            .lines()
            .filter_map(|line| {
                // Table rows are `| METHOD | `/path` | … |`; the path is the
                // only backticked cell that starts with a slash.
                let mut cells = line.split('|').map(str::trim);
                cells.next()?;
                cells.next()?;
                let path = cells.next()?.trim_matches('`');
                path.starts_with('/').then_some(path)
            })
            .collect();
        assert!(
            served.len() > 100,
            "route table parsed only {} paths — the parser or the table shape drifted",
            served.len()
        );
        for path in RELAY_FORBIDDEN_PATHS {
            assert!(
                served.contains(path),
                "relay_forbidden guards {path}, which no router serves — \
                 either the literal is wrong or the route was removed. \
                 A denylist entry matching nothing is worse than none."
            );
        }
    }

    #[test]
    fn the_predicate_and_the_enumeration_agree() {
        for path in RELAY_FORBIDDEN_PATHS {
            assert!(relay_forbidden(path), "{path} is listed but not matched");
        }
    }

    /// The operating surface the relay exists to carry stays reachable. A list
    /// that quietly grew to cover ordinary operation would break the lane's
    /// whole purpose, so pin the paths that must keep working.
    #[test]
    fn the_ordinary_operating_surface_still_crosses_the_relay() {
        // A ticket bound to the request carries the linked ground station's
        // authority, so plugin, service and reboot operations cross too; the
        // armed interlock, not this list, guards them in flight.
        for path in [
            "/api/status",
            "/api/status/full",
            "/api/telemetry",
            "/api/config",
            "/api/params",
            "/api/services",
            "/api/logs",
            "/api/vision/detections/latest",
            "/api/plugins/install",
            "/api/plugins/install_from_url",
            "/api/plugins/com.example.tool/enable",
            "/api/plugins/com.example.tool/grant",
            "/api/services/ados-mavlink/restart",
            "/api/v1/setup/reboot",
            "/api/v1/system/restart-supervisor",
        ] {
            assert!(
                !relay_forbidden(path),
                "{path} is ordinary operation and must still cross"
            );
        }
    }

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
            // The dashboard-PIN gate a keyless off-box browser must reach.
            "/api/dashboard/pin/status",
            "/api/dashboard/pin/verify",
            "/api/dashboard/pin/set",
            // The ground-station WebSocket relays: the upgrade bypasses the HTTP
            // key gate, so the handler does its own ticket/header auth.
            "/api/v1/ground-station/ws/uplink",
            "/api/v1/ground-station/pic/events",
            "/api/v1/ground-station/ws/mesh",
            "/api/v1/ground-station/ws/buttons",
        ] {
            assert!(is_public(p), "{p} should be public");
        }
        for p in [
            "/api/time",
            "/api/status",
            "/api/command",
            "/api/pairing/unpair",
            // PIN reset is NOT public — it stays behind the normal on-box/key gate.
            "/api/dashboard/pin/clear",
            "/v1/openapi.json",
        ] {
            assert!(!is_public(p), "{p} should NOT be public");
        }
    }

    #[test]
    fn the_operator_ui_is_reachable_while_unpaired() {
        // The shell an operator has to load before they can do anything at all,
        // including read the pairing code that would let them pair.
        for p in [
            "/cockpit",
            "/cockpit/",
            "/cockpit/assets/index-abc123.js",
            "/cockpit/assets/index-abc123.css",
            "/cockpit/brand.svg",
            "/",
            "/index.html",
            "/assets/index-def456.js",
            "/brand.svg",
            "/favicon.ico",
        ] {
            assert!(is_operator_ui(p), "{p} is the operator's own UI");
        }
    }

    #[test]
    fn serving_the_ui_does_not_open_the_data_behind_it() {
        // The whole point: the shell loads, everything it asks for stays shut.
        // A regression here would hand an unpaired node's telemetry, config and
        // command surface to any peer on the network.
        for p in [
            "/api/status",
            "/api/telemetry",
            "/api/config",
            "/api/command",
            "/api/services",
            "/api/v1/ground-station/status",
            // Live video is not UI. It sits outside `/api/` and must not be
            // swept in by a "anything that is not an API path" shortcut.
            "/whep",
            // Neither is the route-surface documentation.
            "/docs",
            "/docs/oauth2-redirect",
        ] {
            assert!(!is_operator_ui(p), "{p} must NOT be served while unpaired");
        }
    }

    #[test]
    fn a_cockpit_lookalike_path_is_not_the_cockpit() {
        // Prefix matching is easy to get wrong in the direction that opens
        // something: `/cockpit` and `/assets` must not vouch for a sibling that
        // merely starts with the same letters. An extension-less sibling is a
        // dashboard client route and gets the shell, but a file under one is
        // not a bundle asset, and a data prefix never vouches for anything.
        for p in [
            "/cockpitfoo/payload.bin",
            "/cockpit-admin/dump.json",
            "/api/cockpit",
            "/assetsfoo/index.js",
            "/whep/cockpit",
            "/docs/cockpit",
        ] {
            assert!(!is_operator_ui(p), "{p} is not the cockpit");
        }
    }

    #[test]
    fn the_unpaired_gate_pins_private_lan_data_and_admits_the_shell() {
        use crate::auth::UnpairedDecision;
        // An ordinary private-LAN browser: trusted for the operator-UI scope,
        // so it is not flatly refused; its DATA calls require a PIN session.
        let lan = CallerClass::OperatorLan;

        // The shell loads, so the operator can see the node and its pairing code.
        for p in [
            "/cockpit/",
            "/cockpit/assets/index-abc.js",
            "/",
            "/brand.svg",
        ] {
            assert_eq!(
                unpaired_decision(&GET, p, true, lan),
                UnpairedDecision::Allow,
                "{p} must load so the operator has a surface at all"
            );
        }
        for p in ["/api/status", "/api/config", "/api/command", "/whep"] {
            assert_eq!(
                unpaired_decision(&GET, p, true, lan),
                UnpairedDecision::RequirePin,
                "{p} must be PIN-gated for a private-LAN peer while unpaired"
            );
        }
        // Claiming the device over its own LAN is the documented local-first
        // flow, so it stays open to this caller.
        assert_eq!(
            unpaired_decision(&POST, "/api/pairing/claim", true, lan),
            UnpairedDecision::Allow
        );
        assert_eq!(
            unpaired_decision(&GET, "/api/pairing/info", true, lan),
            UnpairedDecision::Allow
        );

        // A dashboard client route reloaded or deep-linked loads the shell
        // rather than a JSON refusal; the data it then asks for stays gated.
        for p in ["/settings/network", "/plugins", "/logs/flight"] {
            assert_eq!(
                unpaired_decision(&GET, p, true, lan),
                UnpairedDecision::Allow,
                "{p} is a client route of the dashboard"
            );
            assert_eq!(
                unpaired_decision(&HEAD, p, true, lan),
                UnpairedDecision::Allow,
                "{p}"
            );
            assert_eq!(
                unpaired_decision(&POST, p, true, lan),
                UnpairedDecision::RequirePin,
                "a non-read on {p} is not the shell"
            );
        }
        for p in [
            "/api/status",
            "/whep/abc",
            "/hls/main/index.m3u8",
            "/ws",
            "/ws/telemetry",
            "/healthz/deep",
            "/docs",
            "/settings/dump.json",
        ] {
            assert_eq!(
                unpaired_decision(&GET, p, true, lan),
                UnpairedDecision::RequirePin,
                "{p} is not a client route"
            );
        }
    }

    /// A remote caller — a public-WAN host, a tunnelled internet request that
    /// arrives on loopback, or an unidentifiable peer — reaches neither the
    /// data plane nor the claim that would hand it the node's key.
    #[test]
    fn a_remote_caller_is_refused_data_and_the_claim_while_unpaired() {
        use crate::auth::UnpairedDecision;
        for p in ["/api/status", "/api/command", "/whep", "/api/pairing/claim"] {
            assert_eq!(
                unpaired_decision(&GET, p, true, CallerClass::Remote),
                UnpairedDecision::Refuse,
                "{p} must be refused to a remote caller while unpaired"
            );
        }
        // The shell and the non-issuing public routes are still served, so a
        // browser is never left with nothing to read.
        for p in ["/cockpit/", "/api/pairing/info", "/healthz"] {
            assert_eq!(
                unpaired_decision(&GET, p, true, CallerClass::Remote),
                UnpairedDecision::Allow,
                "{p}"
            );
        }
    }

    #[test]
    fn pairing_the_device_opens_everything_the_gate_was_holding() {
        use crate::auth::UnpairedDecision;
        for caller in [CallerClass::OperatorLan, CallerClass::Remote] {
            for p in ["/api/status", "/api/command", "/whep", "/cockpit/"] {
                assert_eq!(
                    unpaired_decision(&GET, p, false, caller),
                    UnpairedDecision::Allow,
                    "{p} is not this gate's business once paired"
                );
            }
        }
    }

    #[test]
    fn the_reachable_callers_are_the_ones_a_fresh_device_is_reached_from() {
        use crate::auth::UnpairedDecision;
        // The local operator and the first-boot lifelines keep unrestricted
        // unpaired data access (no PIN): these are the surfaces the PIN is first
        // created on, and they may claim the device.
        for caller in [CallerClass::OnBox, CallerClass::Lifeline] {
            for p in ["/api/status", "/api/pairing/claim"] {
                assert_eq!(
                    unpaired_decision(&GET, p, true, caller),
                    UnpairedDecision::Allow,
                    "{caller:?} {p}"
                );
            }
        }
    }

    #[test]
    fn a_paired_state_without_a_key_fails_closed() {
        let dir = tempfile::tempdir().unwrap();
        // paired:true with no key is a damaged record, not an unclaimed node.
        let path = write_pairing(dir.path(), r#"{"paired": true, "api_key": ""}"#);
        let state = PairingState::with_path(path);
        assert_eq!(state.current(), Pairing::Unreadable);
        assert!(!state.authorize("/api/status", Some("")));
    }

    /// A corrupt file must not open the data plane: that is exactly the state
    /// in which the next claim would mint a fresh key for whoever asks.
    #[test]
    fn a_malformed_pairing_file_fails_closed() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_pairing(dir.path(), "this is not json");
        let state = PairingState::with_path(path);
        assert_eq!(state.current(), Pairing::Unreadable);
        assert!(!state.authorize("/api/status", None));
        assert!(!state.authorize("/api/status", Some("anything")));
        // The public handshake paths still answer (the handler reports the fault).
        assert!(state.authorize("/api/pairing/info", None));
    }

    #[test]
    fn the_configured_key_and_the_pairing_key_are_both_credentials() {
        let paired = Pairing::Paired("pair-key".into());
        assert!(credential_matches(&paired, "", Some("pair-key")));
        assert!(credential_matches(&paired, "cfg-key", Some("cfg-key")));
        assert!(!credential_matches(&paired, "cfg-key", Some("other")));
        assert!(!credential_matches(&paired, "", Some("")));
        // An unconfigured key never matches an empty presentation.
        assert!(!credential_matches(&Pairing::Unpaired, "", Some("")));
    }

    #[test]
    fn an_unpaired_node_treats_no_arbitrary_string_as_a_credential() {
        // Cloud-posture routes demand a real credential even while unpaired;
        // "any non-empty key" was not one.
        assert!(!credential_matches(&Pairing::Unpaired, "", Some("x")));
        assert!(!credential_matches(&Pairing::Unreadable, "", Some("x")));
        assert!(credential_matches(
            &Pairing::Unpaired,
            "cfg-key",
            Some("cfg-key")
        ));
    }

    #[test]
    fn the_native_edge_accepts_the_configured_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_pairing(dir.path(), r#"{"paired": true, "api_key": "pair-key"}"#);
        let state = PairingState::with_path(path).with_configured_key("cfg-key");
        assert!(state.authorize("/api/status", Some("cfg-key")));
        assert!(state.authorize("/api/status", Some("pair-key")));
        assert!(!state.authorize("/api/status", Some("nope")));
        assert!(state.credential_valid(Some("cfg-key")));
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

    fn peer(ip: &str) -> PeerKey {
        PeerKey::of(Some(ip.parse().unwrap()))
    }

    #[test]
    fn rate_limiter_admits_up_to_capacity_then_rejects() {
        let limiter = RateLimiter::new(3, Duration::from_secs(60));
        let a = peer("192.168.1.50");
        assert!(limiter.check(a));
        assert!(limiter.check(a));
        assert!(limiter.check(a));
        // Fourth in the same window is rejected.
        assert!(!limiter.check(a));
    }

    /// One host exhausting its budget leaves every other caller's untouched: a
    /// single shared bucket let any LAN host 429 the paired operator.
    #[test]
    fn one_caller_exhausting_its_budget_does_not_starve_another() {
        let limiter = RateLimiter::new(2, Duration::from_secs(60));
        let flooder = peer("192.168.1.66");
        while limiter.check(flooder) {}
        assert!(
            limiter.check(peer("192.168.1.50")),
            "the operator is served"
        );
        // Addresses within one IPv6 /64 are one caller.
        let v6 = peer("2001:db8:1:2::5");
        assert!(limiter.check(v6));
        assert!(limiter.check(peer("2001:db8:1:2::6")));
        assert!(!limiter.check(peer("2001:db8:1:2:ffff::1")));
    }

    #[test]
    fn rate_limiter_refills_after_the_window() {
        let limiter = RateLimiter::new(1, Duration::from_millis(20));
        let a = peer("192.168.1.50");
        assert!(limiter.check(a));
        assert!(!limiter.check(a));
        std::thread::sleep(Duration::from_millis(30));
        assert!(limiter.check(a), "the window refilled");
    }

    #[test]
    fn signing_disable_and_credential_issuance_are_refused_over_the_relay() {
        for path in [
            "/api/mavlink/signing/disable-on-fc",
            "/api/plugins/capability-token",
        ] {
            assert!(relay_forbidden(path), "{path} must not cross the relay");
        }
        for path in [
            "/api/services",
            "/api/plugins/com.example.tool",
            "/api/plugins/com.example.tool/disable",
            "/api/mavlink/signing/capability",
        ] {
            assert!(!relay_forbidden(path), "{path} stays reachable");
        }
    }

    #[test]
    fn relayed_config_writes_refuse_trust_root_keys() {
        for key in [
            "security.api.api_key",
            "security",
            "server.cloud.mqtt_broker",
            "server.mode",
            "server.self_hosted.url",
            "pairing.convex_url",
            "remote_access.cloudflare.token_path",
        ] {
            assert!(relay_config_key_forbidden(key), "{key}");
        }
        for key in [
            "video.wfb.fleet_slot",
            "swarm.enabled",
            "mavlink.endpoints",
            "network.hotspot.enabled",
            "serverx",
            "securityx.y",
        ] {
            assert!(!relay_config_key_forbidden(key), "{key}");
        }
    }
}
