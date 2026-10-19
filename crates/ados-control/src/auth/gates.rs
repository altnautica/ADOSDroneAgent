//! Path gates: the relay denylist, the public and operator-UI path sets, and
//! the unpaired-caller decision.

use super::*;

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
pub(super) fn path_covers(denied: &str, path: &str) -> bool {
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
    "/api/v1/ground-station/wfb/invite",
    "/api/mavlink/signing/disable-on-fc",
    "/api/v1/setup/reset",
    "/api/v1/setup/cloud-choice",
    "/api/v1/setup/apply",
    "/api/v1/setup/profile",
    "/api/v1/setup/remote-access/cloudflare",
    "/api/v1/ground-station/factory-reset",
    // Power-off leaves the node unreachable until someone is on site, so it
    // stays a panel-local or on-LAN action.
    "/api/v1/setup/shutdown",
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

/// A field tile read, `/api/v1/ground-station/tiles/{name}/{z}/{x}/{y}.mvt`:
/// authenticated like any data route, but not charged to the caller's request
/// budget, because one map pan fetches dozens of tiles at once. Only the exact
/// four-segment shape counts, with no empty or dot segment, so the exemption
/// cannot be stretched over another path by prefixing it.
pub fn is_tile_read(path: &str) -> bool {
    let Some(rest) = path.strip_prefix("/api/v1/ground-station/tiles/") else {
        return false;
    };
    let mut segments = 0;
    for s in rest.split('/') {
        if s.is_empty() || s == "." || s == ".." {
            return false;
        }
        segments += 1;
    }
    segments == 4
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
pub(super) const NON_UI_PREFIXES: [&str; 6] = ["/api", "/whep", "/hls", "/ws", "/healthz", "/docs"];

/// A dashboard client-side route: extension-less, under no data prefix, and
/// answered by the router with the dashboard bundle.
pub(super) fn is_spa_client_route(path: &str) -> bool {
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
pub(super) const CLAIM_PATH: &str = "/api/pairing/claim";

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
