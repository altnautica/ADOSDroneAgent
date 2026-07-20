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
    let put = |path| NativeRoute {
        method: Method::PUT,
        path,
    };
    let delete = |path| NativeRoute {
        method: Method::DELETE,
        path,
    };
    vec![
        // Status + identity.
        get("/healthz"),
        get("/api/version"),
        get("/api/status"),
        get("/api/telemetry"),
        get("/api/time"),
        // Control-plane RTT echo + the FC-source picker enumeration.
        get("/api/ping"),
        get("/api/mavlink/ports"),
        // Pairing handshake.
        get("/api/pairing/info"),
        get("/api/pairing/code"),
        post("/api/pairing/claim"),
        post("/api/pairing/unpair"),
        // Command.
        post("/api/command"),
        get("/api/commands"),
        // CAN passthrough 501 stub.
        post("/api/can/passthrough"),
        // Operator cloud-export trigger: writes the push-request file the cloud
        // service consumes (a thin trigger; the response is the brief poll result).
        post("/api/logs/push"),
        // Vision designate (operator click-to-follow).
        post("/api/vision/designate"),
        // Vision engine status: the registered-model read-back for the GCS hub.
        get("/api/vision/status"),
        // Vision capabilities: perception-by-task read + single-capability resolve.
        get("/api/vision/capabilities"),
        // Vision detector selection (PUT pick / DELETE clear) + custom-model upload.
        put("/api/vision/detector"),
        delete("/api/vision/detector"),
        post("/api/vision/models/upload"),
        // Plugin per-drone config write (GCS skill toggle / settings → live host).
        put("/api/plugins/{plugin_id}/config"),
        // Plugin MCP-tool invocation (an MCP client runs a plugin's tool → live
        // host; a two-param {plugin_id}/{tool} template).
        post("/api/plugins/{plugin_id}/tools/{tool}/invoke"),
        // Plugin published-state read (a {plugin_id} template under the otherwise
        // permanent-Python /api/plugins prefix; only this exact GET is native).
        get("/api/plugins/{plugin_id}/state"),
        // Compute-node cluster status (read from the heartbeat sidecar).
        get("/api/compute/status"),
        // ADOS Atlas per-drone world-model capture: readiness read + the enable
        // config write + the live capture-session controls.
        get("/api/atlas/readiness"),
        put("/api/atlas/config"),
        post("/api/atlas/capture/start"),
        post("/api/atlas/capture/stop"),
        post("/api/atlas/capture/pause"),
        post("/api/atlas/capture/resume"),
        // WebSocket auth ticket mint.
        post("/api/_ws/ticket"),
        // Dashboard-access PIN gate (status/verify/set public-exempt at the edge,
        // clear normally-gated; see routes/dashboard_pin.rs + auth::is_public).
        get("/api/dashboard/pin/status"),
        post("/api/dashboard/pin/verify"),
        post("/api/dashboard/pin/set"),
        post("/api/dashboard/pin/clear"),
        // MCP-token management: the AI-control surface's mint/status/revoke. Native
        // so they keep the front's auth posture (mint additionally gates on-box/key
        // in-handler).
        get("/api/mcp/status"),
        post("/api/mcp/tokens"),
        post("/api/mcp/revoke"),
        // Params: the full list + the single-param read (a {name} template).
        get("/api/params"),
        get("/api/params/{name}"),
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
        // System resources snapshot (CPU/memory/swap/disk/temperatures).
        get("/api/system"),
        // Composite triage snapshot (LCD Diagnostics + GCS remote-display).
        get("/api/v1/diagnostics"),
        // Video reads.
        get("/api/video/latency"),
        get("/api/v1/video/air-pipeline"),
        get("/api/video/config"),
        // Camera roster read (declared + discovered + live, reconciled) + the
        // operator write (persists the leg list via the supervisor's video socket).
        // Distinct path from the legacy /api/video/cameras switchable enumeration.
        get("/api/video/roster"),
        put("/api/video/roster"),
        // Ground-station status + radio (profile-gated).
        get("/api/v1/ground-station/status"),
        get("/api/v1/ground-station/wfb"),
        get("/api/v1/ground-station/wfb/relay/status"),
        get("/api/v1/ground-station/wfb/atlas-relay/status"),
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
        // Ground-station reads ported in the read-tail wave (profile-gated).
        get("/api/v1/ground-station/recording/list"),
        get("/api/v1/ground-station/ui"),
        get("/api/v1/ground-station/display"),
        get("/api/v1/ground-station/gamepads"),
        get("/api/v1/ground-station/bluetooth/paired"),
        // Ground-station WebSocket relays (profile-gated): the uplink-matrix
        // change stream + the PIC arbiter transition stream + the mesh/pairing
        // event stream + the front-panel button stream. They are native so the
        // edge routes the upgrade to the front (not the proxy); the handlers do
        // their own WebSocket auth, and the paths are public-exempt so the edge
        // does not gate the keyless browser handshake.
        get("/api/v1/ground-station/ws/uplink"),
        get("/api/v1/ground-station/pic/events"),
        get("/api/v1/ground-station/ws/mesh"),
        get("/api/v1/ground-station/ws/buttons"),
        // Writes. The path-param routes use the {name} template the matcher
        // recognises; the require PUT shares its path with the require GET read.
        post("/api/params/{name}"),
        post("/api/services/{name}/restart"),
        post("/api/v1/system/restart-supervisor"),
        post("/api/mavlink/signing/enroll-fc"),
        post("/api/mavlink/signing/disable-on-fc"),
        put("/api/mavlink/signing/require"),
        // Wi-Fi client reads (profile-agnostic): live station status + saved NM
        // profiles. The scan stays proxied (its rescan is a side effect).
        get("/api/v1/network/client/status"),
        get("/api/v1/network/client/configured"),
        // MAC-pin read: the per-adapter stable-MAC verdicts from the state file.
        get("/api/v1/network/mac/adapters"),
        // Wi-Fi client writes: join (PUT) + leave (DELETE) + forget (DELETE, a
        // {name} template). Each forwards to the native uplink daemon's command
        // socket; the autoconnect toggle stays proxied.
        put("/api/v1/network/client/join"),
        delete("/api/v1/network/client"),
        delete("/api/v1/network/client/configured/{name}"),
        // MAC-pin writes: pin a stable MAC (POST) + clear the pin (DELETE, an
        // {iface} template). Each merges the mac_pin config + drives the shared
        // mac-pin engine for the .link removal and the gated live re-tag.
        post("/api/v1/network/mac/pin"),
        delete("/api/v1/network/mac/{iface}"),
        // WFB radio writes.
        post("/api/wfb/channel"),
        put("/api/wfb/tx-power"),
        // WFB auto-pair toggle (a surgical video.wfb config merge after a live
        // pair-status read; a re-arm on a paired rig is refused without a persist).
        put("/api/wfb/pair/auto-pair"),
        // Ground-station network priority write (PUT on the priority read's path).
        put("/api/v1/ground-station/network/priority"),
        // Ground-station WFB config write (PUT on the wfb read's path): a surgical
        // video.wfb config merge the radio/ground services pick up on their cadence.
        put("/api/v1/ground-station/wfb"),
        // Ground-station network writes (ap/share_uplink + autoconnect; ethernet +
        // modem PUTs share their read paths) forwarded to the ados-net command socket.
        put("/api/v1/ground-station/network/ap"),
        put("/api/v1/ground-station/network/ethernet"),
        put("/api/v1/ground-station/network/modem"),
        put("/api/v1/ground-station/network/share_uplink"),
        put("/api/v1/network/client/configured/{name}/autoconnect"),
        // Ground-station mesh + WFB-pair writes (role + mesh/config PUTs share their
        // read paths) forwarded to the ados-groundlink command socket.
        put("/api/v1/ground-station/role"),
        put("/api/v1/ground-station/mesh/gateway_preference"),
        put("/api/v1/ground-station/mesh/config"),
        post("/api/v1/ground-station/wfb/pair"),
        delete("/api/v1/ground-station/wfb/pair"),
        // Ground-station video writes: recording start/stop (ados-video) + the
        // camera-source switch (a MAVLink COMMAND_LONG to the FC socket).
        post("/api/v1/ground-station/recording/start"),
        post("/api/v1/ground-station/recording/stop"),
        post("/api/v1/ground-station/camera/switch"),
        // Ground-station UI config writes (display PUT shares its read path).
        put("/api/v1/ground-station/ui/oled"),
        put("/api/v1/ground-station/ui/buttons"),
        put("/api/v1/ground-station/ui/screens"),
        put("/api/v1/ground-station/display"),
        // Ground-station PIC arbiter + gamepad + Bluetooth writes (ados-hid socket).
        post("/api/v1/ground-station/pic/claim"),
        post("/api/v1/ground-station/pic/release"),
        post("/api/v1/ground-station/pic/confirm-token"),
        post("/api/v1/ground-station/pic/heartbeat"),
        put("/api/v1/ground-station/gamepads/primary"),
        post("/api/v1/ground-station/bluetooth/scan"),
        post("/api/v1/ground-station/bluetooth/pair"),
        delete("/api/v1/ground-station/bluetooth/{mac}"),
    ]
}

/// The path prefixes the agent keeps in Python by design — the ecosystem-bound
/// features (vision/AI, the plugin runtime, the setup facade, peripherals,
/// the WebRTC playback endpoint, the LCD/OLED display surface, calibration). A
/// request under one of these is a known feature that has not migrated, NOT an
/// unknown path: when the residual upstream is gone (the zero-Python headless
/// profile), the proxy answers `501` for these rather than `404`.
pub const PERMANENT_PYTHON_PREFIXES: [&str; 7] = [
    "/api/vision",
    "/api/plugins",
    "/api/setup",
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
/// fallback never fires for a native route (axum routes it first). The method
/// must match exactly — a `POST` to a `GET`-only native path is NOT native, so it
/// falls through to the proxy, which lets the residual surface answer with its
/// own `405`/`404`. The path matches against the native template: a `{param}`
/// segment matches any single non-empty segment, every other segment literally
/// (see [`path_matches_template`]), so a path-param route like
/// `/api/services/{name}/restart` is recognized as native and keeps its auth.
pub fn is_native(method: &Method, path: &str) -> bool {
    native_routes()
        .iter()
        .any(|r| r.method == method && path_matches_template(r.path, path))
}

/// Match a request path against a native-route template. A segment wrapped in
/// `{...}` matches any single non-empty segment; every other segment must match
/// literally, and both must have the same number of segments. A param-free
/// template reduces to literal equality, so the existing exact routes are
/// unaffected. Mirrors how axum's router matches `{param}` placeholders, so the
/// auth gate and the router agree on what is native.
fn path_matches_template(template: &str, actual: &str) -> bool {
    let t = template.split('/');
    let a = actual.split('/');
    let (tc, ac): (Vec<&str>, Vec<&str>) = (t.collect(), a.collect());
    if tc.len() != ac.len() {
        return false;
    }
    tc.iter().zip(ac.iter()).all(|(ts, seg)| {
        if ts.starts_with('{') && ts.ends_with('}') && ts.len() >= 2 {
            !seg.is_empty()
        } else {
            ts == seg
        }
    })
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
    fn path_param_templates_match_a_single_segment() {
        // A param-free template still matches only its exact path.
        assert!(path_matches_template("/api/status", "/api/status"));
        assert!(!path_matches_template("/api/status", "/api/status/full"));
        // A {param} segment matches any single non-empty segment.
        assert!(path_matches_template(
            "/api/params/{name}",
            "/api/params/RC1_MIN"
        ));
        assert!(path_matches_template(
            "/api/services/{name}/restart",
            "/api/services/ados-mavlink/restart"
        ));
        // Same segment count required; an empty placeholder segment does not match.
        assert!(!path_matches_template("/api/params/{name}", "/api/params"));
        assert!(!path_matches_template("/api/params/{name}", "/api/params/"));
        assert!(!path_matches_template(
            "/api/params/{name}",
            "/api/params/a/b"
        ));
        // A literal segment must still match literally.
        assert!(!path_matches_template("/api/params/{name}", "/api/other/x"));
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
            133,
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
            "/api/video/roster",
            "/api/v1/ground-station/status",
            "/api/v1/ground-station/wfb",
            "/api/v1/ground-station/wfb/relay/status",
            "/api/v1/ground-station/wfb/atlas-relay/status",
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
            "/api/params/{name}",
            "/api/v1/network/client/status",
            "/api/v1/network/client/configured",
            "/api/v1/network/mac/adapters",
            "/api/plugins/{plugin_id}/state",
            "/api/compute/status",
            "/api/atlas/readiness",
        ] {
            assert!(has(Method::GET, p), "{p} must be in the native set");
        }
        // The write routes must be native under their own methods (else
        // auth-skipped). The path-param routes are templates the matcher resolves.
        assert!(has(Method::POST, "/api/params/{name}"));
        // The CAN passthrough 501 stub is native (POST).
        assert!(has(Method::POST, "/api/can/passthrough"));
        assert!(has(Method::POST, "/api/services/{name}/restart"));
        // The ADOS Atlas capture control writes.
        assert!(has(Method::PUT, "/api/atlas/config"));
        assert!(has(Method::POST, "/api/atlas/capture/start"));
        assert!(has(Method::POST, "/api/atlas/capture/stop"));
        assert!(has(Method::POST, "/api/atlas/capture/pause"));
        assert!(has(Method::POST, "/api/atlas/capture/resume"));
        assert!(has(Method::POST, "/api/v1/system/restart-supervisor"));
        assert!(has(Method::POST, "/api/mavlink/signing/enroll-fc"));
        assert!(has(Method::POST, "/api/mavlink/signing/disable-on-fc"));
        assert!(has(Method::PUT, "/api/mavlink/signing/require"));
        // The Wi-Fi client writes: a PUT join + two DELETEs (leave + the {name}
        // forget template).
        assert!(has(Method::PUT, "/api/v1/network/client/join"));
        assert!(has(Method::DELETE, "/api/v1/network/client"));
        assert!(has(
            Method::DELETE,
            "/api/v1/network/client/configured/{name}"
        ));
        // The MAC-pin writes: a POST pin + a DELETE {iface} clear template.
        assert!(has(Method::POST, "/api/v1/network/mac/pin"));
        assert!(has(Method::DELETE, "/api/v1/network/mac/{iface}"));
        // The operator camera-roster write.
        assert!(has(Method::PUT, "/api/video/roster"));
        // The WFB radio writes + the GS network priority + GS wfb config writes.
        assert!(has(Method::POST, "/api/wfb/channel"));
        assert!(has(Method::PUT, "/api/wfb/tx-power"));
        // The WFB auto-pair toggle is native (PUT).
        assert!(has(Method::PUT, "/api/wfb/pair/auto-pair"));
        // The operator cloud-export trigger is native (POST).
        assert!(has(Method::POST, "/api/logs/push"));
        assert!(has(Method::PUT, "/api/v1/ground-station/network/priority"));
        assert!(has(Method::PUT, "/api/v1/ground-station/wfb"));
        // The ground-station write surge: network/mesh/video/UI/PIC/Bluetooth writes.
        assert!(has(Method::PUT, "/api/v1/ground-station/network/ap"));
        assert!(has(Method::PUT, "/api/v1/ground-station/network/ethernet"));
        assert!(has(Method::PUT, "/api/v1/ground-station/network/modem"));
        assert!(has(
            Method::PUT,
            "/api/v1/ground-station/network/share_uplink"
        ));
        assert!(has(
            Method::PUT,
            "/api/v1/network/client/configured/{name}/autoconnect"
        ));
        assert!(has(Method::PUT, "/api/v1/ground-station/role"));
        assert!(has(
            Method::PUT,
            "/api/v1/ground-station/mesh/gateway_preference"
        ));
        assert!(has(Method::PUT, "/api/v1/ground-station/mesh/config"));
        assert!(has(Method::POST, "/api/v1/ground-station/wfb/pair"));
        assert!(has(Method::DELETE, "/api/v1/ground-station/wfb/pair"));
        assert!(has(Method::POST, "/api/v1/ground-station/recording/start"));
        assert!(has(Method::POST, "/api/v1/ground-station/recording/stop"));
        assert!(has(Method::POST, "/api/v1/ground-station/camera/switch"));
        assert!(has(Method::PUT, "/api/v1/ground-station/ui/oled"));
        assert!(has(Method::PUT, "/api/v1/ground-station/ui/buttons"));
        assert!(has(Method::PUT, "/api/v1/ground-station/ui/screens"));
        assert!(has(Method::PUT, "/api/v1/ground-station/display"));
        assert!(has(Method::POST, "/api/v1/ground-station/pic/claim"));
        assert!(has(Method::POST, "/api/v1/ground-station/pic/release"));
        assert!(has(
            Method::POST,
            "/api/v1/ground-station/pic/confirm-token"
        ));
        assert!(has(Method::POST, "/api/v1/ground-station/pic/heartbeat"));
        assert!(has(Method::PUT, "/api/v1/ground-station/gamepads/primary"));
        assert!(has(Method::POST, "/api/v1/ground-station/bluetooth/scan"));
        assert!(has(Method::POST, "/api/v1/ground-station/bluetooth/pair"));
        assert!(has(
            Method::DELETE,
            "/api/v1/ground-station/bluetooth/{mac}"
        ));
        // The original surface stays native.
        assert!(has(Method::GET, "/healthz"));
        assert!(has(Method::POST, "/api/command"));
        // The control-plane ping + the FC-source picker enumeration.
        assert!(has(Method::GET, "/api/ping"));
        assert!(has(Method::GET, "/api/mavlink/ports"));
        // The WS-ticket mint is native (replaces the proxied Python route).
        assert!(has(Method::POST, "/api/_ws/ticket"));
        // The dashboard-access PIN gate: the status read + the verify/set/clear
        // writes. status/verify/set are public-exempt at the edge; all four are
        // native (else the writes would be auth-skipped).
        assert!(has(Method::GET, "/api/dashboard/pin/status"));
        assert!(has(Method::POST, "/api/dashboard/pin/verify"));
        assert!(has(Method::POST, "/api/dashboard/pin/set"));
        assert!(has(Method::POST, "/api/dashboard/pin/clear"));
        // The MCP-token management routes are native (else the mint/revoke writes
        // would be auth-skipped).
        assert!(has(Method::GET, "/api/mcp/status"));
        assert!(has(Method::POST, "/api/mcp/tokens"));
        assert!(has(Method::POST, "/api/mcp/revoke"));
        // The plugin per-drone config write is native (a control-plane write, so
        // it stays off the residual Python plugin surface).
        assert!(has(Method::PUT, "/api/plugins/{plugin_id}/config"));
        assert!(has(
            Method::POST,
            "/api/plugins/{plugin_id}/tools/{tool}/invoke"
        ));
        // The vision detector selection (PUT/DELETE) + custom-model upload (POST)
        // are native control-plane writes under the otherwise permanent-Python
        // /api/vision prefix (only these exact routes are served natively).
        assert!(has(Method::PUT, "/api/vision/detector"));
        assert!(has(Method::DELETE, "/api/vision/detector"));
        assert!(has(Method::POST, "/api/vision/models/upload"));
        // The engine-status read-back (registered model set) is native.
        assert!(has(Method::GET, "/api/vision/status"));
        // The perception-capabilities read (grouped + single-resolve) is native.
        assert!(has(Method::GET, "/api/vision/capabilities"));
        // The system-resources snapshot is native.
        assert!(has(Method::GET, "/api/system"));
        // The read-tail wave: composite diagnostics + the GS recording/ui/input reads.
        assert!(has(Method::GET, "/api/v1/diagnostics"));
        assert!(has(Method::GET, "/api/v1/ground-station/recording/list"));
        assert!(has(Method::GET, "/api/v1/ground-station/ui"));
        assert!(has(Method::GET, "/api/v1/ground-station/display"));
        assert!(has(Method::GET, "/api/v1/ground-station/gamepads"));
        assert!(has(Method::GET, "/api/v1/ground-station/bluetooth/paired"));
        // The native WebSocket relays (the upgrade is a GET).
        assert!(has(Method::GET, "/api/v1/ground-station/ws/uplink"));
        assert!(has(Method::GET, "/api/v1/ground-station/pic/events"));
        assert!(has(Method::GET, "/api/v1/ground-station/ws/mesh"));
        assert!(has(Method::GET, "/api/v1/ground-station/ws/buttons"));
    }
}
