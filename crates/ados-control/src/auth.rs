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
    /// path is always allowed; an unpaired agent allows everything (the
    /// unpaired caller gate, [`unpaired_decision`], has already run); a paired
    /// agent requires the exact key. The on-box shortcut is applied by the edge
    /// before this is consulted, so this only models a caller that is not
    /// on-box.
    pub fn authorize(&self, path: &str, presented_key: Option<&str>) -> bool {
        if is_public(path) {
            return true;
        }
        data_plane_access(&self.current(), CallerClass::Remote, presented_key) == Access::Accept
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
/// A relayed request arrives in the on-box posture, because the relay has no
/// credential to present: a fleet shares one radio key by design, and no
/// per-node API credential is distributed with it. That posture is workable for
/// the operating surface a linked ground agent is supposed to have — telemetry,
/// parameters, configuration, services — which is the authority the relay exists
/// to carry.
///
/// It is NOT workable for the paths below, because they do not merely use the
/// node's authority, they hand it out or give it away:
///
/// - **Pairing mutation.** `unpair` clears the node's pairing and mints a fresh
///   code, and `claim` is public by necessity (a fresh operator holds no key
///   yet). Reachable together, they convert radio range into a standing API key
///   that works from anywhere on the network, long after the caller is out of
///   radio range. That is the one escalation that outlives the lane it came
///   from, which is what makes it the important one.
/// - **Credential issuance** — scoped tokens and the dashboard PIN, each of
///   which is a second standing credential.
/// - **Radio pairing** — a caller reaching this over the radio can drop the
///   node off the very fleet key that let it in, or move it onto a different
///   fleet. The ground-station install route is listed for the same reason: it
///   is profile-gated, so a relayed call lands on a drone and 404s today, but
///   the denylist is the layer that must not depend on where a route happens
///   to be mounted.
/// - **Plugin install** — arbitrary code, self-granted permissions.
/// - **Destructive setup** — factory reset, setup reset, cloud re-posture, and
///   the two paths that take the node off the air outright: a reboot and a
///   supervisor restart. A caller in radio range must not be able to drop an
///   airborne aircraft's whole service stack.
///
/// Refused at the edge rather than per-handler so the rule holds for native and
/// proxied routes alike, and cannot be missed when a route moves between them.
///
/// Every literal here is asserted against the committed route table
/// (`docs/api-surface.md`) by
/// [`tests::every_denylisted_path_is_a_route_something_actually_serves`]. A
/// denylist entry that matches no served path is worse than no entry: it reads
/// as covered while the real path is wide open, which is exactly how
/// `/api/v1/ground-station/ui/factory-reset` — a path no router has ever
/// registered — sat here guarding nothing while
/// `/api/v1/ground-station/factory-reset` was relay-reachable.
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

/// Whether `denied` covers `path`: the same route, or anything beneath it.
///
/// `"/api/plugins/install"` covers `/api/plugins/install`,
/// `/api/plugins/install/` and `/api/plugins/install/resume`, but NOT
/// `/api/plugins/installer` — a prefix test without the boundary check would
/// deny an unrelated sibling and, worse, would let someone believe a subtree
/// is covered because its name happens to share a prefix.
fn path_covers(denied: &str, path: &str) -> bool {
    if !path.starts_with(denied) {
        return false;
    }
    matches!(path.as_bytes().get(denied.len()), None | Some(b'/'))
}

/// Every subtree [`relay_forbidden`] refuses, as data. The predicate reads
/// this list directly, so the two cannot drift; the route-table test still
/// enumerates it to assert each entry names a path something actually serves.
/// A denylist entry that matches no served path is worse than no entry: it
/// reads as covered while the real path is wide open.
pub const RELAY_FORBIDDEN_PATHS: &[&str] = &[
    "/api/pairing/unpair",
    "/api/pairing/accept",
    "/api/mcp/tokens",
    "/api/mcp/revoke",
    "/api/dashboard/pin/set",
    "/api/dashboard/pin/clear",
    "/api/wfb/pair/local-bind",
    "/api/wfb/pair/unpair",
    "/api/v1/ground-station/wfb/pair",
    "/api/plugins/install",
    "/api/plugins/install_from_url",
    "/api/plugins/capability-token",
    "/api/v1/setup/reset",
    "/api/v1/setup/reboot",
    "/api/v1/setup/cloud-choice",
    "/api/v1/setup/remote-access/cloudflare",
    "/api/v1/system/restart-supervisor",
    "/api/v1/ground-station/factory-reset",
];

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
/// Deliberately an allow-list rather than "anything outside `/api/`". `/whep` is
/// a live video stream and `/docs` enumerates the route surface; both sit
/// outside `/api/` and both stay refused while unpaired.
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
    matches!(
        path,
        "/index.html" | "/brand.svg" | "/favicon.ico" | "/manifest.webmanifest"
    )
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
pub fn unpaired_decision(path: &str, unpaired: bool, caller: CallerClass) -> UnpairedDecision {
    if !unpaired || is_operator_ui(path) {
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

    /// The escalation this list exists to break: unpair clears the pairing and
    /// mints a fresh code, and claim is public by necessity, so the two together
    /// turn radio range into a standing API key that keeps working long after
    /// the caller is out of range. Refusing unpair is what breaks the chain —
    /// claim on its own hands out nothing while a pairing is intact.
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

    #[test]
    fn a_relayed_caller_cannot_unpair_and_then_claim() {
        assert!(
            relay_forbidden("/api/pairing/unpair"),
            "unpair over the relay is the escalation root and must be refused"
        );
        assert!(
            is_public("/api/pairing/claim"),
            "claim stays public — a fresh operator holds no key yet"
        );
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
    fn destructive_and_code_loading_paths_are_refused_over_the_relay() {
        for path in [
            "/api/wfb/pair/unpair",
            "/api/wfb/pair/local-bind",
            "/api/v1/ground-station/wfb/pair",
            "/api/plugins/install",
            "/api/plugins/install_from_url",
            "/api/v1/setup/reset",
            "/api/v1/setup/cloud-choice",
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
        for path in [
            "/api/status",
            "/api/status/full",
            "/api/telemetry",
            "/api/config",
            "/api/params",
            "/api/services",
            "/api/logs",
            "/api/vision/detections/latest",
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
        // something: `/cockpit` must not vouch for a sibling that merely starts
        // with the same letters.
        for p in [
            "/cockpitfoo",
            "/cockpit-admin",
            "/api/cockpit",
            "/assetsfoo",
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
                unpaired_decision(p, true, lan),
                UnpairedDecision::Allow,
                "{p} must load so the operator has a surface at all"
            );
        }
        for p in ["/api/status", "/api/config", "/api/command", "/whep"] {
            assert_eq!(
                unpaired_decision(p, true, lan),
                UnpairedDecision::RequirePin,
                "{p} must be PIN-gated for a private-LAN peer while unpaired"
            );
        }
        // Claiming the device over its own LAN is the documented local-first
        // flow, so it stays open to this caller.
        assert_eq!(
            unpaired_decision("/api/pairing/claim", true, lan),
            UnpairedDecision::Allow
        );
        assert_eq!(
            unpaired_decision("/api/pairing/info", true, lan),
            UnpairedDecision::Allow
        );
    }

    /// A remote caller — a public-WAN host, a tunnelled internet request that
    /// arrives on loopback, or an unidentifiable peer — reaches neither the
    /// data plane nor the claim that would hand it the node's key.
    #[test]
    fn a_remote_caller_is_refused_data_and_the_claim_while_unpaired() {
        use crate::auth::UnpairedDecision;
        for p in ["/api/status", "/api/command", "/whep", "/api/pairing/claim"] {
            assert_eq!(
                unpaired_decision(p, true, CallerClass::Remote),
                UnpairedDecision::Refuse,
                "{p} must be refused to a remote caller while unpaired"
            );
        }
        // The shell and the non-issuing public routes are still served, so a
        // browser is never left with nothing to read.
        for p in ["/cockpit/", "/api/pairing/info", "/healthz"] {
            assert_eq!(
                unpaired_decision(p, true, CallerClass::Remote),
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
                    unpaired_decision(p, false, caller),
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
                    unpaired_decision(p, true, caller),
                    UnpairedDecision::Allow,
                    "{caller:?} {p}"
                );
            }
        }
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
