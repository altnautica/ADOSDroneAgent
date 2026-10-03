//! The front's route map: which paths it serves natively, which it forwards to
//! the residual Python, and what answers everything else.
//!
//! The LAN front owns the TCP port and answers a fixed set of routes itself.
//! A request that matches no native route goes to the router fallback, which
//! dispatches on [`classify`]: the permanent-Python prefixes
//! ([`PERMANENT_PYTHON_PREFIXES`]) are forwarded to the residual app over its
//! internal Unix socket ([`crate::proxy`]); `/hls/*` is relayed to mediamtx on
//! loopback; the operator UI bundles are served from disk; and any other `/api`
//! path is a 404 (or a 405 when the path is native under another method).
//!
//! [`is_native`] is the single source of truth the auth edge ([`crate::serve`])
//! consults. A native route takes the front's own auth lane — rate limiter,
//! pairing gate, MCP-scope admission. A non-native route is authenticated on the
//! proxied lane instead ([`crate::serve`]'s `proxied_auth_then_forward`: API key,
//! HMAC, dashboard session, WS ticket) before the fallback answers it, so it
//! never sees the native lane's rate limiter or scope admission. Classifying a
//! served route non-native by accident therefore drops it off those three
//! protections.

use std::sync::LazyLock;

use http::Method;

/// How the front handles a given request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouteMode {
    /// The front answers this route itself.
    Native,
    /// A permanent-Python prefix, forwarded to the residual app.
    Residual,
    /// The HLS playback fallback, relayed to mediamtx on loopback.
    Hls,
    /// The browser dashboard and the on-box cockpit, served from their bundles.
    OperatorUi,
    /// An `/api` path no route serves. `other_method` is true when the path is a
    /// native route under a different method (a 405 rather than a 404).
    Unrouted { other_method: bool },
}

/// One native route: a `(method, path)` template the front serves itself. A
/// template segment written `{name}` matches any single segment and `{*name}`
/// swallows the tail; see [`segments_match`]. Axum's `:name` form is NOT
/// understood here — a colon segment is compared literally and the route silently
/// falls off the native lane. `websocket` marks a GET that only answers an
/// upgrade, so the route table can tell it apart from a plain HTTP read.
struct NativeRoute {
    method: Method,
    path: &'static str,
    websocket: bool,
}

/// The exact `(method, path)` set the front serves natively — the same routes
/// [`crate::routes::build_router`] registers. Kept in lockstep with that router:
/// a route added there is added here so the auth edge keeps its native posture
/// rather than proxying it.
fn native_routes() -> Vec<NativeRoute> {
    // Small constructors keep the list scannable as it grows route by route.
    let route = |method, path| NativeRoute {
        method,
        path,
        websocket: false,
    };
    let get = |path| route(Method::GET, path);
    let post = |path| route(Method::POST, path);
    let put = |path| route(Method::PUT, path);
    let delete = |path| route(Method::DELETE, path);
    let patch = |path| route(Method::PATCH, path);
    // The upgrade request is a GET, which is what the auth edge matches on.
    let ws = |path| NativeRoute {
        method: Method::GET,
        path,
        websocket: true,
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
        // Agent-config JSON Schema (the committed, build-time-embedded asset).
        get("/api/config/schema"),
        get("/api/v2/observability/{*upstream_path}"),
        // Agent config read + one-key write.
        get("/api/config"),
        put("/api/config"),
        // Pairing handshake.
        get("/api/pairing/info"),
        get("/api/pairing/code"),
        post("/api/pairing/claim"),
        post("/api/pairing/unpair"),
        post("/api/pairing/accept"),
        // Command.
        post("/api/command"),
        get("/api/commands"),
        // CAN passthrough 501 stub.
        post("/api/can/passthrough"),
        // Operator cloud-export trigger: writes the push-request file the cloud
        // service consumes (a thin trigger; the response is the brief poll result).
        get("/api/logs"),
        get("/api/logs/stream"),
        post("/api/logs/push"),
        // The per-pair relay credential a ground station offers over the radio.
        // Reachable over the relay on purpose: it is the only path to a drone
        // paired by radio alone, and it must work before any credential exists.
        post("/api/relay/peer-secret"),
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
        // Plugin per-drone config write (GCS skill toggle / settings → live host)
        // and its read-back.
        put("/api/plugins/{plugin_id}/config"),
        get("/api/plugins/{plugin_id}/config"),
        // Plugin MCP-tool invocation (an MCP client runs a plugin's tool → live
        // host; a two-param {plugin_id}/{tool} template).
        post("/api/plugins/{plugin_id}/tools/{tool}/invoke"),
        // Plugin published-state read.
        get("/api/plugins/{plugin_id}/state"),
        // The plugin lifecycle: catalog, list, detail and its reads, parse and
        // install, the per-plugin lifecycle writes, the capability token and the
        // install-job progress stream.
        get("/api/v1/plugins/catalog"),
        get("/api/plugins"),
        post("/api/plugins/parse"),
        post("/api/plugins/install"),
        post("/api/plugins/parse_from_url"),
        post("/api/plugins/install_from_url"),
        post("/api/plugins/install_builtin"),
        post("/api/plugins/capability-token"),
        ws("/api/plugins/jobs/{job_id}"),
        get("/api/plugins/{plugin_id}"),
        delete("/api/plugins/{plugin_id}"),
        get("/api/plugins/{plugin_id}/gcs/{*asset_path}"),
        get("/api/plugins/{plugin_id}/manifest"),
        get("/api/plugins/{plugin_id}/attestation"),
        get("/api/plugins/{plugin_id}/readiness"),
        post("/api/plugins/{plugin_id}/grant"),
        delete("/api/plugins/{plugin_id}/perms/{permission_id}"),
        post("/api/plugins/{plugin_id}/enable"),
        post("/api/plugins/{plugin_id}/disable"),
        post("/api/plugins/{plugin_id}/pin"),
        post("/api/plugins/{plugin_id}/unpin"),
        post("/api/plugins/{plugin_id}/auto-update"),
        // A plugin's own HTTP API, every method (a WebSocket upgrade is a GET).
        get("/api/plugins/{plugin_id}/x/{*rest}"),
        post("/api/plugins/{plugin_id}/x/{*rest}"),
        put("/api/plugins/{plugin_id}/x/{*rest}"),
        patch("/api/plugins/{plugin_id}/x/{*rest}"),
        delete("/api/plugins/{plugin_id}/x/{*rest}"),
        // Cloud relay link state (read from the cloud-link sidecar).
        get("/api/cloud/link"),
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
        get("/api/mavlink/signing/counters"),
        // WFB radio reads.
        get("/api/wfb"),
        get("/api/wfb/history"),
        get("/api/wfb/pair"),
        get("/api/wfb/pair/failover-status"),
        get("/api/wfb/pair/local-bind"),
        post("/api/wfb/pair/local-bind"),
        post("/api/wfb/pair/unpair"),
        // Consolidated status.
        get("/api/status/full"),
        // The agent webapp's one-pager poll.
        get("/api/v1/dashboard/snapshot"),
        // The swarm neighbour table (profile-agnostic: served on drones too).
        get("/api/swarm/neighbors"),
        // Per-pack battery health (the battery engine's read model).
        get("/api/v1/battery"),
        // System resources snapshot (CPU/memory/swap/disk/temperatures).
        get("/api/system"),
        // Composite triage snapshot (LCD Diagnostics + GCS remote-display).
        get("/api/v1/diagnostics"),
        // Per-hop video-pipeline verifier (samples reliable counters over a window).
        get("/api/diag/video"),
        // Storage-wear verdict (write-counter delta, sticky throttle bits, store
        // footprint).
        get("/api/diag/storage"),
        // Video pipeline: composite status, the discovered-camera enumeration, a
        // one-shot still, latency, and the config read + link-tuning write.
        get("/api/video"),
        get("/api/video/cameras"),
        get("/api/video/snapshot"),
        get("/api/video/latency"),
        get("/api/video/config"),
        post("/api/video/config"),
        // Camera roster read (declared + discovered + live, reconciled) + the
        // operator write (persists the leg list via the supervisor's video socket).
        // Distinct path from the flat /api/video/cameras enumeration.
        get("/api/video/roster"),
        put("/api/video/roster"),
        // Drone attention-profile write: hero / thumbnail. Retargets the local
        // encoder through ados-video's command socket.
        post("/api/video/profile"),
        // Node-local recording (any profile), the ground-station recorder.
        post("/api/video/record/start"),
        post("/api/video/record/stop"),
        // Ground-station status + radio (profile-gated).
        get("/api/v1/ground-station/status"),
        get("/api/v1/ground-station/wfb"),
        get("/api/v1/ground-station/wfb/relay/status"),
        get("/api/v1/ground-station/wfb/receiver/relays"),
        get("/api/v1/ground-station/wfb/receiver/combined"),
        // Relay-proxy to a WFB-linked drone (profile-gated). A wildcard tail:
        // whatever path the drone's own API serves rides through unchanged.
        // Registering it here is what puts a lane that spends radio airtime
        // behind the same auth edge and rate limiter as its siblings.
        get("/api/v1/ground-station/relay-proxy/{peer_device_id}/{*path}"),
        post("/api/v1/ground-station/relay-proxy/{peer_device_id}/{*path}"),
        put("/api/v1/ground-station/relay-proxy/{peer_device_id}/{*path}"),
        delete("/api/v1/ground-station/relay-proxy/{peer_device_id}/{*path}"),
        patch("/api/v1/ground-station/relay-proxy/{peer_device_id}/{*path}"),
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
        post("/api/v1/ground-station/pair/accept"),
        post("/api/v1/ground-station/pair/close"),
        post("/api/v1/ground-station/pair/approve/{device_id}"),
        post("/api/v1/ground-station/pair/revoke/{device_id}"),
        post("/api/v1/ground-station/pair/join"),
        get("/api/v1/ground-station/pic"),
        get("/api/v1/ground-station/captive-token"),
        // Ground-station CRSF RC lane (profile-gated): the staleness-gated
        // state read + the channel-injection and parameter-write forwards to
        // the lane daemon's command socket.
        get("/api/v1/ground-station/crsf"),
        post("/api/v1/ground-station/crsf/channels"),
        post("/api/v1/ground-station/crsf/params"),
        // Config-over-radio (relayed config): status read + request forward.
        get("/api/v1/ground-station/relayed/config"),
        get("/api/v1/ground-station/relayed/status"),
        post("/api/v1/ground-station/relayed/config"),
        // Ground-station reads ported in the read-tail wave (profile-gated).
        get("/api/v1/ground-station/recording/list"),
        // Recordings playback reads: mediamtx's segment inventory and the fMP4
        // clip cut, both proxied off its loopback playback server so the media
        // server never faces the network itself.
        get("/api/v1/ground-station/recording/segments"),
        get("/api/v1/ground-station/recording/clip"),
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
        ws("/api/v1/ground-station/ws/uplink"),
        ws("/api/v1/ground-station/pic/events"),
        ws("/api/v1/ground-station/ws/mesh"),
        ws("/api/v1/ground-station/ws/buttons"),
        // Writes. The path-param routes use the {name} template the matcher
        // recognises.
        post("/api/params/{name}"),
        post("/api/services/{name}/restart"),
        post("/api/v1/system/restart-supervisor"),
        post("/api/mavlink/signing/enroll-fc"),
        post("/api/mavlink/signing/disable-on-fc"),
        // Wi-Fi client reads (profile-agnostic): live station status, saved NM
        // profiles, and a nearby-network scan on the station interface.
        get("/api/v1/network/client/status"),
        get("/api/v1/network/client/configured"),
        get("/api/v1/network/client/scan"),
        // MAC-pin read: the per-adapter stable-MAC verdicts from the state file.
        get("/api/v1/network/mac/adapters"),
        // Wi-Fi client writes: join (PUT) + leave (DELETE) + forget (DELETE, a
        // {name} template) + the saved-profile autoconnect toggle (PUT). Each
        // forwards to the native uplink daemon's command socket.
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
        // The ground station's Wi-Fi client join/leave, forwarded to the same
        // command socket. The last two ground-station network writes to leave the
        // residual Python surface.
        put("/api/v1/ground-station/network/client/join"),
        delete("/api/v1/ground-station/network/client"),
        put("/api/v1/network/client/configured/{name}/autoconnect"),
        // Ground-station mesh + WFB-pair writes (role + mesh/config PUTs share their
        // read paths) forwarded to the ados-groundlink command socket.
        put("/api/v1/ground-station/role"),
        put("/api/v1/ground-station/mesh/gateway_preference"),
        put("/api/v1/ground-station/mesh/config"),
        post("/api/v1/ground-station/wfb/pair"),
        delete("/api/v1/ground-station/wfb/pair"),
        // Release one drone's fleet slot, without touching the shared radio keys.
        delete("/api/v1/ground-station/wfb/pair/{device_id}"),
        // Return the station to first-boot posture (on-box callers only).
        post("/api/v1/ground-station/factory-reset"),
        // Ground-station video writes: recording start/stop (ados-video) + the
        // camera-source switch (a MAVLink COMMAND_LONG to the FC socket).
        post("/api/v1/ground-station/recording/start"),
        post("/api/v1/ground-station/recording/stop"),
        post("/api/v1/ground-station/camera/switch"),
        // Remove one recording segment. A `{segment}` template, so it is
        // method-scoped: the GET siblings above keep their own literal templates
        // and a DELETE is the only method this one claims.
        delete("/api/v1/ground-station/recording/{segment}"),
        // Fleet attention: promote one drone to the full video profile and
        // demote every other registered slot, in one operation.
        post("/api/v1/ground-station/fleet/hero"),
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

/// The native route set as plain data, for the tools that have to enumerate it
/// rather than query it: the `docs/api-surface.md` generator and the CI check
/// that every client path literal resolves against a real route. The first
/// field is the HTTP method, or `WS` for a route that only answers a WebSocket
/// upgrade, so the client check cannot clear a plain GET against one.
///
/// There is no second list — this projects [`native_routes`], so a route added
/// to the router and the auth edge appears in the table and the check without
/// anyone remembering to update a third place.
pub fn native_route_table() -> Vec<(String, &'static str)> {
    native_routes()
        .into_iter()
        .map(|r| {
            let label = if r.websocket {
                "WS".to_owned()
            } else {
                r.method.to_string()
            };
            (label, r.path)
        })
        .collect()
}

/// The path prefixes the agent keeps in Python by design — the ecosystem-bound
/// features (vision/AI, the setup facade, peripherals, the WebRTC playback
/// endpoint, the LCD/OLED display surface). These are the ONLY paths the front
/// forwards to the residual app; every other non-native path is answered here
/// ([`classify`]). When the residual upstream is gone (the zero-Python headless
/// profile), the proxy answers `501` for these: the feature is absent on this
/// profile, not an unknown path.
///
/// These are the paths as MOUNTED, not as the feature is named. The FastAPI app
/// includes each router under `/api` and several routers carry their own `/v1`
/// prefix, so the served path is `/api/v1/setup`, not `/api/setup`. The
/// unversioned `/api/peripherals` (the hardware scan) and `/api/v1/peripherals`
/// (the peripheral plugin registry) are different surfaces and are listed
/// separately. Touch-panel calibration needs no entry of its own: those routes
/// hang off the display and setup routers (`/api/v1/display/calibrate/*`,
/// `/api/v1/setup/display/calibrate/*`) and are already covered.
pub const PERMANENT_PYTHON_PREFIXES: [&str; 6] = [
    "/api/vision",
    "/api/v1/setup",
    "/api/peripherals",
    "/api/v1/peripherals",
    "/whep",
    "/api/v1/display",
];

/// How the front handles a `(method, path)`. The auth edge consults
/// [`is_native`] alone; this is what the router fallback dispatches on for every
/// request that is not a native route.
pub fn classify(method: &Method, path: &str) -> RouteMode {
    if is_native(method, path) {
        return RouteMode::Native;
    }
    if is_permanent_python_path(path) {
        return RouteMode::Residual;
    }
    if path == "/hls" || path.starts_with("/hls/") {
        return RouteMode::Hls;
    }
    if path == "/api" || path.starts_with("/api/") {
        // A native path under the wrong method is a 405, any other API path a
        // 404. Never the dashboard: a typo'd endpoint must fail crisply.
        let other_method = NATIVE_TABLE
            .iter()
            .any(|r| segments_match(&r.segments, path));
        return RouteMode::Unrouted { other_method };
    }
    RouteMode::OperatorUi
}

/// One pre-split template segment.
enum Segment {
    Literal(&'static str),
    /// `{name}`: any single non-empty segment.
    Param,
    /// A final `{*name}`: one or more remaining segments, not all empty.
    Tail,
}

/// A native route with its template split once, so a request is matched by
/// walking its own segments with no allocation.
struct CompiledRoute {
    method: Method,
    segments: Vec<Segment>,
}

/// The native table, built and split once. `is_native` runs on every inbound
/// request, so rebuilding the table there cost a full allocation of it per
/// poll.
static NATIVE_TABLE: LazyLock<Vec<CompiledRoute>> = LazyLock::new(|| {
    native_routes()
        .into_iter()
        .map(|r| CompiledRoute {
            method: r.method,
            segments: compile_template(r.path),
        })
        .collect()
});

fn compile_template(template: &'static str) -> Vec<Segment> {
    let parts: Vec<&'static str> = template.split('/').collect();
    let last = parts.len() - 1;
    parts
        .into_iter()
        .enumerate()
        .map(|(i, s)| {
            if i == last && s.starts_with("{*") && s.ends_with('}') {
                Segment::Tail
            } else if s.starts_with('{') && s.ends_with('}') && s.len() >= 2 {
                Segment::Param
            } else {
                Segment::Literal(s)
            }
        })
        .collect()
}

/// True iff the front serves this exact `(method, path)` itself. The auth edge
/// keeps its native posture for these and proxies everything else; the proxy
/// fallback never fires for a native route (axum routes it first). The method
/// must match exactly — a `POST` to a `GET`-only native path is NOT native, so it
/// falls through to the proxy, which lets the residual surface answer with its
/// own `405`/`404`. The path matches against the native template: a `{param}`
/// segment matches any single non-empty segment, every other segment literally
/// (see [`segments_match`]), so a path-param route like
/// `/api/services/{name}/restart` is recognized as native and keeps its auth.
pub fn is_native(method: &Method, path: &str) -> bool {
    NATIVE_TABLE
        .iter()
        .any(|r| r.method == method && segments_match(&r.segments, path))
}

/// Match a request path against a pre-split native-route template. A `{param}`
/// segment matches any single non-empty segment; a final `{*name}` segment
/// matches one or more remaining segments, not all empty; every other segment
/// must match literally. A param-free template reduces to literal equality.
/// Mirrors how axum's router matches `{param}` and `{*wildcard}` placeholders,
/// so the auth gate and the router agree on what is native.
fn segments_match(template: &[Segment], actual: &str) -> bool {
    let mut rest = actual.split('/');
    for segment in template {
        match segment {
            Segment::Tail => {
                let mut any_non_empty = false;
                let mut any = false;
                for s in rest.by_ref() {
                    any = true;
                    any_non_empty |= !s.is_empty();
                }
                return any && any_non_empty;
            }
            Segment::Param => match rest.next() {
                Some(s) if !s.is_empty() => {}
                _ => return false,
            },
            Segment::Literal(lit) => match rest.next() {
                Some(s) if s == *lit => {}
                _ => return false,
            },
        }
    }
    rest.next().is_none()
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

    fn path_matches_template(template: &'static str, actual: &str) -> bool {
        segments_match(&compile_template(template), actual)
    }

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

    /// No table entry may use axum's `:param` form. `segments_match` treats only
    /// `{name}` as a wildcard, so a colon segment is compared literally: the route
    /// is still served by axum (0.7 is where `:param` is correct) but classified
    /// non-native, which silently drops it off the native auth lane — rate
    /// limiter, pairing gate, MCP-scope admission.
    ///
    /// `every_native_route_is_native` above cannot catch this: a literal template
    /// matches itself, so the malformed entry passes there. That is why the
    /// colon form survived in this table until it was found by hand.
    #[test]
    fn no_native_template_uses_axum_colon_param_syntax() {
        let offenders: Vec<&str> = native_routes()
            .iter()
            .filter(|r| r.path.split('/').any(|s| s.starts_with(':')))
            .map(|r| r.path)
            .collect();
        assert!(
            offenders.is_empty(),
            "these templates use axum's `:param` form, which this table's matcher \
             reads as a literal segment, so the route is served but not classified \
             native: {offenders:?}"
        );
    }

    /// The fleet-slot release is a param route: a concrete device id must classify
    /// native so the DELETE takes the native auth lane like its siblings.
    #[test]
    fn the_fleet_slot_release_is_native_for_a_concrete_device_id() {
        assert!(is_native(
            &Method::DELETE,
            "/api/v1/ground-station/wfb/pair/abc123"
        ));
        // The bare collection DELETE is a separate, also-native route.
        assert!(is_native(
            &Method::DELETE,
            "/api/v1/ground-station/wfb/pair"
        ));
        // Two segments below the template is not a match.
        assert!(!is_native(
            &Method::DELETE,
            "/api/v1/ground-station/wfb/pair/abc123/extra"
        ));
    }

    #[test]
    fn unknown_and_proxied_paths_are_not_native() {
        // A permanent-Python feature path.
        assert!(!is_native(&Method::GET, "/api/vision/state"));
        assert!(!is_native(&Method::GET, "/api/vision/detections/latest"));
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
    fn the_fallback_dispatch_sends_each_path_to_its_owner() {
        assert_eq!(
            classify(&Method::GET, "/api/vision/state"),
            RouteMode::Residual
        );
        assert_eq!(classify(&Method::POST, "/whep"), RouteMode::Residual);
        assert_eq!(
            classify(&Method::GET, "/hls/main/index.m3u8"),
            RouteMode::Hls
        );
        assert_eq!(classify(&Method::GET, "/"), RouteMode::OperatorUi);
        assert_eq!(classify(&Method::GET, "/cockpit/"), RouteMode::OperatorUi);
        assert_eq!(
            classify(&Method::GET, "/setup/network"),
            RouteMode::OperatorUi
        );
        // An API path nothing serves is a 404, never the dashboard.
        assert_eq!(
            classify(&Method::GET, "/api/flights"),
            RouteMode::Unrouted {
                other_method: false
            }
        );
        // A native path under the wrong method is a 405.
        assert_eq!(
            classify(&Method::POST, "/api/status"),
            RouteMode::Unrouted { other_method: true }
        );
    }

    /// The residual app is reached only for the permanent prefixes. Every route
    /// it used to serve outside them is now answered by the front: a request for
    /// one of these paths never reaches Python, whatever the residual mounts.
    #[test]
    fn nothing_outside_the_permanent_prefixes_reaches_python() {
        for (method, path) in [
            (Method::GET, "/api/video"),
            (Method::GET, "/api/video/cameras"),
            (Method::POST, "/api/video/config"),
            (Method::GET, "/api/video/snapshot"),
            (Method::GET, "/api/video/snapshot.jpg"),
            (Method::POST, "/api/video/camera/switch"),
            (Method::POST, "/api/pairing/accept"),
            (Method::GET, "/api/v1/dashboard/snapshot"),
            (Method::GET, "/api/v1/network/client/scan"),
            (Method::POST, "/api/v1/ground-station/factory-reset"),
            (Method::GET, "/hls/main/index.m3u8"),
            (Method::GET, "/"),
            (Method::GET, "/index.html"),
            (Method::GET, "/cockpit"),
            (Method::GET, "/cockpit/assets/app.js"),
            (Method::GET, "/docs"),
            (Method::GET, "/openapi.json"),
        ] {
            assert_ne!(
                classify(&method, path),
                RouteMode::Residual,
                "{method} {path} must not be forwarded to the residual app"
            );
        }
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
    fn a_wildcard_tail_template_swallows_the_remaining_segments() {
        let t = "/api/v1/ground-station/relay-proxy/{peer_device_id}/{*path}";
        // One tail segment and many both match; the peer id is a single segment.
        assert!(path_matches_template(
            t,
            "/api/v1/ground-station/relay-proxy/0a1b2c3d4e5f/healthz"
        ));
        assert!(path_matches_template(
            t,
            "/api/v1/ground-station/relay-proxy/0a1b2c3d4e5f/api/status/full"
        ));
        // The tail must exist: without it the route is not this one, and a
        // wildcard that matched nothing would hand the auth edge a path axum
        // never routes here.
        assert!(!path_matches_template(
            t,
            "/api/v1/ground-station/relay-proxy/0a1b2c3d4e5f"
        ));
        assert!(!path_matches_template(
            t,
            "/api/v1/ground-station/relay-proxy/0a1b2c3d4e5f/"
        ));
        // A different prefix is not this route.
        assert!(!path_matches_template(
            t,
            "/api/v1/ground-station/status/0a1b2c3d4e5f/healthz"
        ));
    }

    #[test]
    fn the_relay_proxy_lane_is_native_under_every_method_it_serves() {
        // Missing from the native set, this lane would be served with the
        // front's auth SKIPPED and outside the rate limiter its siblings share.
        let p = "/api/v1/ground-station/relay-proxy/0a1b2c3d4e5f/api/services";
        for m in [
            Method::GET,
            Method::POST,
            Method::PUT,
            Method::DELETE,
            Method::PATCH,
        ] {
            assert!(is_native(&m, p), "{m} {p} must be native");
        }
        assert!(!is_native(&Method::OPTIONS, p));
    }

    /// The plugin lifecycle is native: every route keeps the front's auth lane
    /// and none of them reaches the residual Python, and the passthrough to a
    /// plugin's own API is native under each method it forwards.
    #[test]
    fn the_plugin_lifecycle_is_native_not_residual() {
        for (m, p) in [
            (Method::GET, "/api/plugins"),
            (Method::GET, "/api/v1/plugins/catalog"),
            (Method::POST, "/api/plugins/install"),
            (Method::POST, "/api/plugins/install_from_url"),
            (Method::GET, "/api/plugins/com.example.web"),
            (Method::DELETE, "/api/plugins/com.example.web"),
            (
                Method::GET,
                "/api/plugins/com.example.web/gcs/assets/app.js",
            ),
            (Method::GET, "/api/plugins/com.example.web/attestation"),
            (
                Method::DELETE,
                "/api/plugins/com.example.web/perms/event.publish",
            ),
            (Method::GET, "/api/plugins/jobs/3f2a9c1e"),
        ] {
            assert!(is_native(&m, p), "{m} {p} must be native");
            assert!(
                !is_permanent_python_path(p),
                "{p} must not read as residual"
            );
        }
        let passthrough = "/api/plugins/com.example.web/x/api/live";
        for m in [
            Method::GET,
            Method::POST,
            Method::PUT,
            Method::PATCH,
            Method::DELETE,
        ] {
            assert!(
                is_native(&m, passthrough),
                "{m} {passthrough} must be native"
            );
        }
        assert!(!is_native(&Method::GET, "/api/plugins/com.example.web/x"));
    }

    #[test]
    fn permanent_prefix_match_needs_a_segment_boundary() {
        // The exact prefix and a child path match.
        assert!(is_permanent_python_path("/api/vision"));
        assert!(is_permanent_python_path("/api/vision/detections"));
        // A path that only shares the prefix as a substring does NOT match.
        assert!(!is_permanent_python_path("/api/visionary"));
        assert!(!is_permanent_python_path("/api/v1/setupwizard"));
    }

    #[test]
    fn permanent_prefixes_are_the_paths_python_actually_serves() {
        // Each entry below is a route the FastAPI app really mounts. The app
        // includes every router under `/api` (api/server.py) and several routers
        // declare their own `/v1` prefix, so the served path carries both
        // segments. A prefix written as the feature is NAMED rather than as it is
        // MOUNTED matches nothing, and the headless profile then answers 404 —
        // "no such path" — for a feature that exists and is simply absent from
        // this build. 501 is the truthful answer.
        for path in [
            // routes/setup/__init__.py: APIRouter(prefix="/v1/setup")
            "/api/v1/setup/state",
            "/api/v1/setup/cloud",
            // routes/display.py: APIRouter(prefix="/v1/display")
            "/api/v1/display/page",
            // ...and calibration hangs off display + setup, so it needs no entry.
            "/api/v1/display/calibrate/start",
            "/api/v1/setup/display/calibrate/start",
            // routes/peripherals_v1.py: APIRouter(prefix="/v1/peripherals")
            "/api/v1/peripherals",
            // routes/peripherals.py: unprefixed, mounted under /api
            "/api/peripherals/scan",
            // routes/vision_models.py + vision_detections.py: unprefixed
            "/api/vision/models",
            "/api/vision/detections/latest",
            // routes/whep.py: mounted at the root, no /api
            "/whep",
            "/whep/abc123",
        ] {
            assert!(
                is_permanent_python_path(path),
                "{path} is served by residual Python but is not covered by \
                 PERMANENT_PYTHON_PREFIXES, so the front never forwards it"
            );
            assert_eq!(
                classify(&Method::GET, path),
                RouteMode::Residual,
                "{path} should be forwarded to the residual app"
            );
        }
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
            199,
            "native route count drifted from build_router"
        );
        let has = |m: Method, p: &str| routes.iter().any(|r| r.method == m && r.path == p);
        // Every ported read route must be native (else auth-skipped on a paired
        // agent).
        for p in [
            "/api/config/schema",
            "/api/params",
            "/api/services",
            "/api/fleet/enrollment",
            "/api/fleet/peers",
            "/api/mavlink/signing/capability",
            "/api/mavlink/signing/counters",
            "/api/wfb",
            "/api/wfb/history",
            "/api/wfb/pair",
            "/api/wfb/pair/failover-status",
            "/api/status/full",
            "/api/swarm/neighbors",
            "/api/v1/battery",
            "/api/video/latency",
            "/api/video/config",
            "/api/video/roster",
            "/api/video",
            "/api/video/cameras",
            "/api/video/snapshot",
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
            "/api/v1/ground-station/crsf",
            "/api/params/{name}",
            "/api/v1/network/client/status",
            "/api/v1/network/client/configured",
            "/api/v1/network/client/scan",
            "/api/v1/network/mac/adapters",
            "/api/plugins/{plugin_id}/state",
            "/api/cloud/link",
            "/api/logs",
            "/api/logs/stream",
        ] {
            assert!(has(Method::GET, p), "{p} must be in the native set");
        }
        // The write routes must be native under their own methods (else
        // auth-skipped). The path-param routes are templates the matcher resolves.
        assert!(has(Method::POST, "/api/params/{name}"));
        // The CAN passthrough 501 stub is native (POST).
        assert!(has(Method::POST, "/api/can/passthrough"));
        assert!(has(Method::POST, "/api/services/{name}/restart"));
        assert!(has(Method::POST, "/api/v1/system/restart-supervisor"));
        assert!(has(Method::POST, "/api/v1/ground-station/factory-reset"));
        assert!(has(Method::POST, "/api/mavlink/signing/enroll-fc"));
        assert!(has(Method::POST, "/api/mavlink/signing/disable-on-fc"));
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
        // The drone attention-profile write (hero / thumbnail).
        assert!(has(Method::POST, "/api/video/profile"));
        // The video link-tuning write.
        assert!(has(Method::POST, "/api/video/config"));
        assert!(has(Method::POST, "/api/video/record/start"));
        assert!(has(Method::POST, "/api/video/record/stop"));
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
        // The ground station's own Wi-Fi client join/leave (its uplink onto an
        // existing network), distinct from the drone-side `/api/v1/network/client`
        // pair above.
        assert!(has(
            Method::PUT,
            "/api/v1/ground-station/network/client/join"
        ));
        assert!(has(Method::DELETE, "/api/v1/ground-station/network/client"));
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
        // Fleet hero selection: it spends radio airtime on up to 24 targeted
        // RPCs, so it must sit behind the same auth edge and rate limiter.
        assert!(has(Method::POST, "/api/v1/ground-station/fleet/hero"));
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
        // The CRSF RC-lane injection + parameter writes must be native (else
        // a flight-critical channel injection would be served auth-skipped).
        assert!(has(Method::POST, "/api/v1/ground-station/crsf/channels"));
        assert!(has(Method::POST, "/api/v1/ground-station/crsf/params"));
        assert!(has(Method::GET, "/api/v1/ground-station/relayed/config"));
        assert!(has(Method::GET, "/api/v1/ground-station/relayed/status"));
        assert!(has(Method::POST, "/api/v1/ground-station/relayed/config"));
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
        assert!(has(Method::GET, "/api/plugins/{plugin_id}/config"));
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
        // The external-code pairing accept and the webapp snapshot poll.
        assert!(has(Method::POST, "/api/pairing/accept"));
        assert!(has(Method::GET, "/api/v1/dashboard/snapshot"));
    }
}
