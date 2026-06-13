//! The front's route map: which paths it serves natively vs reverse-proxies.
//!
//! The LAN front owns the TCP port and answers a fixed set of routes itself,
//! byte-identically to the FastAPI surface. Everything else falls through to the
//! reverse proxy ([`crate::proxy`]) and is served by the residual Python over its
//! internal Unix socket. As more routes move into the front, they leave the
//! proxied set and join the native set here.
//!
//! [`is_native`] is the single source of truth the auth edge ([`crate::serve`])
//! and the proxy fallback ([`crate::proxy`]) both consult: a native route keeps
//! the front's auth posture, a non-native route is forwarded with the front's
//! auth skipped (the residual surface applies its own). The proxied-prefix table
//! is documentation/diagnostics only — it records which prefixes are known
//! features that simply have not migrated, so a graceful-degradation reply can
//! distinguish "this feature is absent on this profile" (a permanent-Python
//! feature, served `501` when the upstream is gone) from "no such route" (`404`).

use http::Method;

/// How the front handles a given route.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouteMode {
    /// The front answers this route itself.
    Native,
    /// The front forwards this route to the residual Python. `permanent` marks a
    /// prefix the agent keeps in Python by design (vision/plugins/setup/…), so a
    /// graceful-degradation reply can return `501` (feature absent on this
    /// profile) rather than `404` when the upstream is gone.
    Proxied { permanent: bool },
}

/// One native route: an exact `(method, path)` the front serves itself. The
/// current native routes carry no path params, so an exact path match is
/// sufficient; a future param route would need a matcher, not a literal.
struct NativeRoute {
    method: Method,
    path: &'static str,
}

/// The exact `(method, path)` set the front serves natively — the same routes
/// [`crate::routes::build_router`] registers. Kept in lockstep with that router:
/// a route added there is added here so the auth edge keeps its native posture
/// rather than proxying it.
fn native_routes() -> Vec<NativeRoute> {
    // Small constructors keep the list scannable as it grows route by route.
    let get = |path| NativeRoute {
        method: Method::GET,
        path,
    };
    let post = |path| NativeRoute {
        method: Method::POST,
        path,
    };
    vec![
        // Status + identity.
        get("/healthz"),
        get("/api/version"),
        get("/api/status"),
        get("/api/telemetry"),
        get("/api/time"),
        // Pairing handshake.
        get("/api/pairing/info"),
        get("/api/pairing/code"),
        post("/api/pairing/claim"),
        post("/api/pairing/unpair"),
        // Command.
        post("/api/command"),
        get("/api/commands"),
        // Params (full list; the single-param path-param route stays proxied).
        get("/api/params"),
        // Services inventory.
        get("/api/services"),
        // Fleet roster.
        get("/api/fleet/enrollment"),
        get("/api/fleet/peers"),
        // MAVLink v2 signing reads.
        get("/api/mavlink/signing/capability"),
        get("/api/mavlink/signing/require"),
        get("/api/mavlink/signing/counters"),
        // WFB radio reads.
        get("/api/wfb"),
        get("/api/wfb/history"),
        get("/api/wfb/pair"),
        get("/api/wfb/pair/failover-status"),
        // Consolidated status.
        get("/api/status/full"),
        // Video reads.
        get("/api/video/latency"),
        get("/api/v1/video/air-pipeline"),
        get("/api/video/config"),
        // Ground-station status + radio (profile-gated).
        get("/api/v1/ground-station/status"),
        get("/api/v1/ground-station/wfb"),
        get("/api/v1/ground-station/wfb/relay/status"),
        get("/api/v1/ground-station/wfb/receiver/relays"),
        get("/api/v1/ground-station/wfb/receiver/combined"),
        // Ground-station mesh (profile-gated).
        get("/api/v1/ground-station/role"),
        get("/api/v1/ground-station/mesh"),
        get("/api/v1/ground-station/mesh/neighbors"),
        get("/api/v1/ground-station/mesh/routes"),
        get("/api/v1/ground-station/mesh/gateways"),
        get("/api/v1/ground-station/mesh/config"),
        // Ground-station network uplink (profile-gated).
        get("/api/v1/ground-station/network"),
        get("/api/v1/ground-station/network/ethernet"),
        get("/api/v1/ground-station/network/client/scan"),
        get("/api/v1/ground-station/network/modem"),
        get("/api/v1/ground-station/network/priority"),
        get("/api/v1/ground-station/modem-status"),
        // Ground-station pairing / PIC / captive token (profile-gated).
        get("/api/v1/ground-station/pair/pending"),
        get("/api/v1/ground-station/pic"),
        get("/api/v1/ground-station/captive-token"),
    ]
}

/// The path prefixes the agent keeps in Python by design — the ecosystem-bound
/// features (vision/AI, the plugin runtime, the setup facade, OTA, peripherals,
/// the WebRTC playback endpoint, the LCD/OLED display surface, calibration). A
/// request under one of these is a known feature that has not migrated, NOT an
/// unknown path: when the residual upstream is gone (the zero-Python headless
/// profile), the proxy answers `501` for these rather than `404`.
pub const PERMANENT_PYTHON_PREFIXES: [&str; 8] = [
    "/api/vision",
    "/api/plugins",
    "/api/setup",
    "/api/ota",
    "/api/peripherals",
    "/whep",
    "/api/display",
    "/api/calibrate",
];

/// How the front handles a `(method, path)`: native, a known permanent-Python
/// prefix, or an other proxied path. The auth edge and the proxy fallback consult
/// [`is_native`]; this richer view is for diagnostics and the graceful-degradation
/// status choice.
pub fn classify(method: &Method, path: &str) -> RouteMode {
    if is_native(method, path) {
        return RouteMode::Native;
    }
    RouteMode::Proxied {
        permanent: is_permanent_python_path(path),
    }
}

/// True iff the front serves this exact `(method, path)` itself. The auth edge
/// keeps its native posture for these and proxies everything else; the proxy
/// fallback never fires for a native route (axum routes it first). An exact path
/// match (the native routes have no params) and an exact method match — a `POST`
/// to a `GET`-only native path is NOT native, so it falls through to the proxy,
/// which lets the residual surface answer with its own `405`/`404`.
pub fn is_native(method: &Method, path: &str) -> bool {
    native_routes()
        .iter()
        .any(|r| r.method == method && r.path == path)
}

/// True when a path sits under a known permanent-Python prefix. Used only to pick
/// `501` over `404` in the graceful-degradation reply when the upstream is gone.
pub fn is_permanent_python_path(path: &str) -> bool {
    PERMANENT_PYTHON_PREFIXES
        .iter()
        .any(|p| path == *p || path.starts_with(&format!("{p}/")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_native_route_is_native() {
        for r in native_routes() {
            assert!(
                is_native(&r.method, r.path),
                "{} {} should be native",
                r.method,
                r.path
            );
            assert_eq!(classify(&r.method, r.path), RouteMode::Native);
        }
    }

    #[test]
    fn unknown_and_proxied_paths_are_not_native() {
        // A permanent-Python feature path.
        assert!(!is_native(&Method::GET, "/api/vision/state"));
        assert!(!is_native(&Method::POST, "/api/plugins/install"));
        // An unknown path entirely.
        assert!(!is_native(&Method::GET, "/api/does-not-exist"));
        // A path that merely shares a native prefix is not an exact match.
        assert!(!is_native(&Method::GET, "/api/status/extra"));
        assert!(!is_native(&Method::GET, "/api/pairing"));
    }

    #[test]
    fn the_wrong_method_is_not_native() {
        // /api/status is GET-native; a POST to it is not native (falls to proxy).
        assert!(!is_native(&Method::POST, "/api/status"));
        // /api/command is POST-native; a GET to it is not native.
        assert!(!is_native(&Method::GET, "/api/command"));
    }

    #[test]
    fn classify_marks_permanent_prefixes() {
        assert_eq!(
            classify(&Method::GET, "/api/vision/state"),
            RouteMode::Proxied { permanent: true }
        );
        assert_eq!(
            classify(&Method::GET, "/whep"),
            RouteMode::Proxied { permanent: true }
        );
        // An ordinary proxied path (not under a permanent prefix) is not permanent.
        assert_eq!(
            classify(&Method::GET, "/api/flights"),
            RouteMode::Proxied { permanent: false }
        );
    }

    #[test]
    fn permanent_prefix_match_needs_a_segment_boundary() {
        // The exact prefix and a child path match.
        assert!(is_permanent_python_path("/api/vision"));
        assert!(is_permanent_python_path("/api/vision/detections"));
        // A path that only shares the prefix as a substring does NOT match.
        assert!(!is_permanent_python_path("/api/visionary"));
        assert!(!is_permanent_python_path("/api/setupwizard"));
    }

    #[test]
    fn native_set_covers_every_registered_route() {
        // INVARIANT: this set must list exactly the (method, path) pairs
        // build_router registers (routes/mod.rs). The LAN-edge auth applies its
        // posture only to native paths, so a route served by build_router but
        // missing here would be served with auth SKIPPED. Adding a route is a
        // two-place edit (build_router + here); this pins the count + the entries
        // so a drift is caught at test time, not at the bench.
        let routes = native_routes();
        assert_eq!(
            routes.len(),
            46,
            "native route count drifted from build_router"
        );
        let has = |m: Method, p: &str| routes.iter().any(|r| r.method == m && r.path == p);
        // Every ported read route must be native (else auth-skipped on a paired
        // agent).
        for p in [
            "/api/params",
            "/api/services",
            "/api/fleet/enrollment",
            "/api/fleet/peers",
            "/api/mavlink/signing/capability",
            "/api/mavlink/signing/require",
            "/api/mavlink/signing/counters",
            "/api/wfb",
            "/api/wfb/history",
            "/api/wfb/pair",
            "/api/wfb/pair/failover-status",
            "/api/status/full",
            "/api/video/latency",
            "/api/v1/video/air-pipeline",
            "/api/video/config",
            "/api/v1/ground-station/status",
            "/api/v1/ground-station/wfb",
            "/api/v1/ground-station/wfb/relay/status",
            "/api/v1/ground-station/wfb/receiver/relays",
            "/api/v1/ground-station/wfb/receiver/combined",
            "/api/v1/ground-station/role",
            "/api/v1/ground-station/mesh",
            "/api/v1/ground-station/mesh/neighbors",
            "/api/v1/ground-station/mesh/routes",
            "/api/v1/ground-station/mesh/gateways",
            "/api/v1/ground-station/mesh/config",
            "/api/v1/ground-station/network",
            "/api/v1/ground-station/network/ethernet",
            "/api/v1/ground-station/network/client/scan",
            "/api/v1/ground-station/network/modem",
            "/api/v1/ground-station/network/priority",
            "/api/v1/ground-station/modem-status",
            "/api/v1/ground-station/pair/pending",
            "/api/v1/ground-station/pic",
            "/api/v1/ground-station/captive-token",
        ] {
            assert!(has(Method::GET, p), "{p} must be in the native set");
        }
        // The original surface stays native.
        assert!(has(Method::GET, "/healthz"));
        assert!(has(Method::POST, "/api/command"));
    }
}
