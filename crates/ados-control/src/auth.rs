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
/// - **Destructive setup** — factory reset, setup reset, cloud re-posture.
///
/// Refused at the edge rather than per-handler so the rule holds for native and
/// proxied routes alike, and cannot be missed when a route moves between them.
pub fn relay_forbidden(path: &str) -> bool {
    matches!(
        path,
        "/api/pairing/unpair"
            | "/api/pairing/accept"
            | "/api/mcp/tokens"
            | "/api/mcp/revoke"
            | "/api/dashboard/pin/set"
            | "/api/dashboard/pin/clear"
            | "/api/wfb/pair/local-bind"
            | "/api/wfb/pair/unpair"
            | "/api/v1/ground-station/wfb/pair"
            | "/api/plugins/install"
            | "/api/plugins/install_from_url"
            | "/api/plugins/capability-token"
            | "/api/v1/setup/reset"
            | "/api/v1/setup/cloud-choice"
            | "/api/v1/setup/remote-access/cloudflare"
            | "/api/v1/ground-station/ui/factory-reset"
    )
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
            | "/api/pairing/claim"
            | "/api/version"
            // Dashboard-access PIN gate: an off-box paired browser must reach the
            // status read + the verify (login) + the set (trust-on-first-use)
            // before it holds any credential. `set` authorizes IN THE HANDLER;
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
/// new private-LAN PIN scope: a private-LAN browser is no longer flatly refused on
/// a DATA route — it is trusted for the operator-UI scope and PIN-gated instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnpairedDecision {
    /// Serve the request: the node is paired, the route is public or operator UI,
    /// or the peer is a first-boot lifeline (loopback, link-local, AP/USB subnets).
    Allow,
    /// A trusted operator-LAN peer requesting a DATA route on an UNPAIRED node:
    /// served only when the caller presents a valid dashboard PIN session
    /// (minted via `/api/dashboard/pin/{set,verify}`), else refused.
    RequirePin,
    /// The peer is neither a lifeline nor a trusted operator-LAN peer (a public-WAN
    /// host, or an unidentifiable peer): refuse with 403, exactly as before.
    Refuse,
}

/// The unpaired-node gate: whether a request is served outright, requires a PIN
/// session, or is refused. Pure, so the decision is testable — it had no
/// behavioural coverage at all while it lived inline in the serve loop, which is a
/// poor place for a security gate to have none.
///
/// `peer` is `None` when the peer address could not be determined, which is
/// treated as not-allowed — an unidentifiable caller is exactly the one this gate
/// exists for.
///
/// The private-LAN operator scope rests on [`ados_protocol::pairing_posture::
/// trusted_operator_lan_peer`], layered on top of the (unchanged) first-boot
/// lifeline set so the direct MAVLink proxy keeps its lifeline-only unpaired
/// posture and the PIN cannot be bypassed off the HTTP surface. An ordinary private
/// LAN peer hits `RequirePin` on a DATA route; the caller (serve.rs) checks the
/// dashboard session and 403s without one.
pub fn unpaired_decision(
    path: &str,
    unpaired: bool,
    peer: Option<std::net::IpAddr>,
) -> UnpairedDecision {
    if !unpaired {
        return UnpairedDecision::Allow;
    }
    if is_public(path) || is_operator_ui(path) {
        return UnpairedDecision::Allow;
    }
    // A DATA route (status / telemetry / command / video): gate by peer.
    let Some(peer) = peer else {
        return UnpairedDecision::Refuse;
    };
    use ados_protocol::pairing_posture::{trusted_operator_lan_peer, unpaired_peer_allowed};
    if unpaired_peer_allowed(&peer) {
        // First-boot lifeline (loopback, link-local, AP/USB): served without a PIN,
        // unchanged — these are the surfaces the PIN is first created on.
        return UnpairedDecision::Allow;
    }
    if trusted_operator_lan_peer(&peer) {
        // A private-LAN browser: the new PIN-gated operator scope.
        return UnpairedDecision::RequirePin;
    }
    UnpairedDecision::Refuse
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
            "/api/v1/ground-station/ui/factory-reset",
        ] {
            assert!(relay_forbidden(path), "{path} must not cross the relay");
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
        use std::net::IpAddr;
        // An ordinary LAN peer — the case the founder hit. A private-LAN address:
        // trusted for the operator-UI scope, so it is no longer flatly refused;
        // instead its DATA calls now require a PIN session.
        let lan: Option<IpAddr> = Some("192.168.1.50".parse().unwrap());

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
        // Data routes from a private-LAN browser are PIN-gated, not flatly refused:
        // the operator's browser can reach the cockpit DATA with a PIN session.
        for p in ["/api/status", "/api/config", "/api/command", "/whep"] {
            assert_eq!(
                unpaired_decision(p, true, lan),
                UnpairedDecision::RequirePin,
                "{p} must be PIN-gated for a private-LAN peer while unpaired"
            );
        }
        // Claiming the device is how it stops being unpaired, so it stays open.
        assert_eq!(
            unpaired_decision("/api/pairing/claim", true, lan),
            UnpairedDecision::Allow
        );
        assert_eq!(
            unpaired_decision("/api/pairing/info", true, lan),
            UnpairedDecision::Allow
        );
    }

    #[test]
    fn a_public_wan_peer_is_still_refused_data_while_unpaired() {
        use crate::auth::UnpairedDecision;
        use std::net::IpAddr;
        // Public-WAN must stay closed: nothing about the PIN-gated scope loosens
        // for a non-private-LAN host.
        for ip in ["8.8.8.8", "203.0.113.5", "2001:db8::1"] {
            let peer: Option<IpAddr> = Some(ip.parse().unwrap());
            assert_eq!(
                unpaired_decision("/api/status", true, peer),
                UnpairedDecision::Refuse,
                "{ip} is public WAN and must stay refused"
            );
        }
    }

    #[test]
    fn pairing_the_device_opens_everything_the_gate_was_holding() {
        use crate::auth::UnpairedDecision;
        use std::net::IpAddr;
        let lan: Option<IpAddr> = Some("192.168.1.50".parse().unwrap());
        for p in ["/api/status", "/api/command", "/whep", "/cockpit/"] {
            assert_eq!(
                unpaired_decision(p, false, lan),
                UnpairedDecision::Allow,
                "{p} is not this gate's business once paired"
            );
        }
    }

    #[test]
    fn an_unidentifiable_peer_is_refused_while_unpaired() {
        use crate::auth::UnpairedDecision;
        // No peer address means the caller cannot be placed on a trusted link,
        // which is precisely who this gate exists to stop.
        assert_eq!(
            unpaired_decision("/api/status", true, None),
            UnpairedDecision::Refuse
        );
        // ...but the shell is still served, so a browser is never left with
        // nothing to read.
        assert_eq!(
            unpaired_decision("/cockpit/", true, None),
            UnpairedDecision::Allow
        );
    }

    #[test]
    fn the_reachable_peers_are_the_ones_a_fresh_device_is_reached_from() {
        use crate::auth::UnpairedDecision;
        use std::net::IpAddr;
        // The first-boot lifelines keep unrestricted unpaired data access (no PIN):
        // these are the surfaces the PIN is first created on.
        for ip in ["127.0.0.1", "192.168.4.10", "192.168.7.2"] {
            let peer: Option<IpAddr> = Some(ip.parse().unwrap());
            assert_eq!(
                unpaired_decision("/api/status", true, peer),
                UnpairedDecision::Allow,
                "{ip} is a direct link to the device"
            );
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
