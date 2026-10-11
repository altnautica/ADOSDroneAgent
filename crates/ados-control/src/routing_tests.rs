use super::*;

/// The full native table (hid-rust writes included), built for this test
/// rather than read from the process-wide install, so the assertions do not
/// depend on which router another test built first.
fn table() -> NativeTable {
    NativeTable::new(crate::routes::describe(true))
}

#[test]
fn every_native_route_is_native() {
    let t = table();
    for r in t.routes() {
        assert!(
            t.is_native(&r.method, r.path),
            "{} {} should be native",
            r.method,
            r.path
        );
        assert_eq!(t.classify(&r.method, r.path), RouteMode::Native);
    }
}

/// No row may use axum 0.7's `:param` form: axum 0.8 refuses it when the router
/// is built, and the matcher would read it as a literal segment.
#[test]
fn no_native_template_uses_axum_colon_param_syntax() {
    let offenders: Vec<&str> = table()
        .routes()
        .iter()
        .filter(|r| r.path.split('/').any(|s| s.starts_with(':')))
        .map(|r| r.path)
        .collect();
    assert!(offenders.is_empty(), "colon-form templates: {offenders:?}");
}

/// The fleet-slot release is a param route: a concrete device id must classify
/// native so the DELETE takes the native auth lane like its siblings.
#[test]
fn the_fleet_slot_release_is_native_for_a_concrete_device_id() {
    let t = table();
    assert!(t.is_native(&Method::DELETE, "/api/v1/ground-station/wfb/pair/abc123"));
    assert!(t.is_native(&Method::DELETE, "/api/v1/ground-station/wfb/pair"));
    assert!(!t.is_native(
        &Method::DELETE,
        "/api/v1/ground-station/wfb/pair/abc123/extra"
    ));
}

#[test]
fn unknown_and_proxied_paths_are_not_native() {
    let t = table();
    assert!(!t.is_native(&Method::GET, "/api/vision/state"));
    assert!(!t.is_native(&Method::GET, "/api/vision/detections/latest"));
    assert!(!t.is_native(&Method::GET, "/api/does-not-exist"));
    assert!(!t.is_native(&Method::GET, "/api/status/extra"));
    assert!(!t.is_native(&Method::GET, "/api/pairing"));
}

#[test]
fn the_wrong_method_is_not_native() {
    let t = table();
    assert!(!t.is_native(&Method::POST, "/api/status"));
    assert!(!t.is_native(&Method::GET, "/api/command"));
}

#[test]
fn param_templates_match_a_single_non_empty_segment() {
    let t = table();
    assert!(t.is_native(&Method::GET, "/api/params/RC1_MIN"));
    assert!(t.is_native(&Method::POST, "/api/services/ados-mavlink/restart"));
    assert!(!t.is_native(&Method::GET, "/api/params/"));
    assert!(!t.is_native(&Method::GET, "/api/params/a/b"));
}

#[test]
fn the_fallback_dispatch_sends_each_path_to_its_owner() {
    let t = table();
    assert_eq!(
        t.classify(&Method::GET, "/api/vision/state"),
        RouteMode::Residual
    );
    assert_eq!(
        t.classify(&Method::GET, "/hls/main/index.m3u8"),
        RouteMode::Hls
    );
    assert_eq!(t.classify(&Method::GET, "/"), RouteMode::OperatorUi);
    assert_eq!(t.classify(&Method::GET, "/cockpit/"), RouteMode::OperatorUi);
    assert_eq!(
        t.classify(&Method::GET, "/setup/network"),
        RouteMode::OperatorUi
    );
    assert_eq!(
        t.classify(&Method::GET, "/api/flights"),
        RouteMode::Unrouted {
            other_method: false
        }
    );
    assert_eq!(
        t.classify(&Method::POST, "/api/status"),
        RouteMode::Unrouted { other_method: true }
    );
}

/// WebRTC playback is served by the front itself: the offer, the teardown and
/// the ICE restart are native, and nothing under `/whep` reaches Python.
#[test]
fn whep_is_native_not_residual() {
    let t = table();
    assert_eq!(t.classify(&Method::POST, "/whep"), RouteMode::Native);
    assert_eq!(
        t.classify(&Method::DELETE, "/whep/abc-123"),
        RouteMode::Native
    );
    assert_eq!(
        t.classify(&Method::PATCH, "/whep/abc-123"),
        RouteMode::Native
    );
    assert!(!is_permanent_python_path("/whep"));
    assert!(!is_permanent_python_path("/whep/abc-123"));
    // An unserved method under /whep is a 405, never Python or the dashboard.
    assert_eq!(
        t.classify(&Method::GET, "/whep"),
        RouteMode::Unrouted { other_method: true }
    );
}

/// The residual app is reached only for the permanent prefixes.
#[test]
fn nothing_outside_the_permanent_prefixes_reaches_python() {
    let t = table();
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
        (Method::GET, "/api/v1/ground-station/captive-token"),
        (Method::POST, "/whep"),
        (Method::GET, "/hls/main/index.m3u8"),
        (Method::GET, "/"),
        (Method::GET, "/index.html"),
        (Method::GET, "/cockpit"),
        (Method::GET, "/cockpit/assets/app.js"),
        (Method::GET, "/docs"),
        (Method::GET, "/openapi.json"),
    ] {
        assert_ne!(
            t.classify(&method, path),
            RouteMode::Residual,
            "{method} {path} must not be forwarded to the residual app"
        );
    }
}

#[test]
fn a_wildcard_tail_needs_a_non_empty_tail() {
    let t = table();
    let base = "/api/v1/ground-station/relay-proxy/0a1b2c3d4e5f";
    assert!(t.is_native(&Method::GET, &format!("{base}/healthz")));
    assert!(t.is_native(&Method::GET, &format!("{base}/api/status/full")));
    assert!(!t.is_native(&Method::GET, base));
    assert!(!t.is_native(&Method::GET, &format!("{base}/")));
}

#[test]
fn the_relay_proxy_lane_is_native_under_every_method_it_serves() {
    let t = table();
    let p = "/api/v1/ground-station/relay-proxy/0a1b2c3d4e5f/api/services";
    for m in [
        Method::GET,
        Method::POST,
        Method::PUT,
        Method::DELETE,
        Method::PATCH,
    ] {
        assert!(t.is_native(&m, p), "{m} {p} must be native");
    }
    assert!(!t.is_native(&Method::OPTIONS, p));
}

/// The plugin lifecycle is native, and the passthrough to a plugin's own API is
/// native under each method it forwards.
#[test]
fn the_plugin_lifecycle_is_native_not_residual() {
    let t = table();
    for (m, p) in [
        (Method::GET, "/api/plugins"),
        (Method::GET, "/api/v1/plugins/catalog"),
        (Method::POST, "/api/plugins/install"),
        (Method::GET, "/api/plugins/com.example.web"),
        (Method::DELETE, "/api/plugins/com.example.web"),
        (
            Method::GET,
            "/api/plugins/com.example.web/gcs/assets/app.js",
        ),
        (
            Method::DELETE,
            "/api/plugins/com.example.web/perms/event.publish",
        ),
        (Method::GET, "/api/plugins/jobs/3f2a9c1e"),
    ] {
        assert!(t.is_native(&m, p), "{m} {p} must be native");
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
        assert!(t.is_native(&m, passthrough), "{m} {passthrough}");
    }
    assert!(!t.is_native(&Method::GET, "/api/plugins/com.example.web/x"));
}

#[test]
fn permanent_prefix_match_needs_a_segment_boundary() {
    assert!(is_permanent_python_path("/api/vision"));
    assert!(is_permanent_python_path("/api/vision/detections"));
    assert!(!is_permanent_python_path("/api/visionary"));
    assert!(!is_permanent_python_path("/api/v1/setupwizard"));
}

#[test]
fn permanent_prefixes_are_the_paths_python_actually_serves() {
    // Each entry is a route the FastAPI app really mounts, as MOUNTED: every
    // router sits under `/api` and several carry their own `/v1`. A prefix
    // written as the feature is named matches nothing, and the headless profile
    // then answers 404 for a feature that is simply absent from this build.
    let t = table();
    for path in [
        "/api/v1/setup/state",
        "/api/v1/setup/cloud",
        "/api/v1/display/page",
        "/api/v1/display/calibrate/start",
        "/api/v1/setup/display/calibrate/start",
        "/api/v1/peripherals",
        "/api/peripherals/scan",
        "/api/vision/models",
        "/api/vision/detections/latest",
    ] {
        assert!(is_permanent_python_path(path), "{path} is not covered");
        assert_eq!(
            t.classify(&Method::GET, path),
            RouteMode::Residual,
            "{path} should be forwarded to the residual app"
        );
    }
}

#[test]
fn the_route_table_projection_labels_websockets() {
    let rows = native_route_table();
    assert!(rows
        .iter()
        .any(|(m, p)| m == "WS" && *p == "/api/v1/ground-station/ws/uplink"));
    assert!(rows.iter().any(|(m, p)| m == "POST" && *p == "/whep"));
    assert!(!rows
        .iter()
        .any(|(_, p)| *p == "/api/v1/ground-station/captive-token"));
}
