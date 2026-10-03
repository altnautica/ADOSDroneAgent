//! The armed interlock: refuse the actions that can drop the aircraft's link or
//! change how it flies while the vehicle is armed.
//!
//! Rebooting the companion, restarting the MAVLink router, the radio or the
//! receive chain, hopping the radio channel, changing its TX power, unpairing a
//! radio, changing the node's profile or role, a factory reset, a flight
//! controller parameter write and an RC-module parameter write each either cut
//! the command and telemetry link or change flight behaviour. While the vehicle
//! is armed each one answers `409` with
//! `{"error":"E_ARMED","message":"Vehicle is armed","override":"force"}` unless
//! the caller sets the override: `"force": true` in the JSON body, or
//! `?force=1` in the query (the form for bodiless methods).
//!
//! The guard is one router layer, so it covers the routes this process serves
//! natively and the ones it proxies to the residual API alike, and it runs
//! before either. Matching is by method plus the decoded request path, so a
//! route that moves from the residual into this process stays covered without
//! any change here. The body is read only when the vehicle counts as armed and
//! the query carries no override, and it is forwarded unchanged.
//!
//! The armed source is the router's vehicle-state snapshot. A snapshot older
//! than [`ARMED_STATE_MAX_AGE`], or none at all, proves nothing about the
//! vehicle, so the node is then treated as armed. Profiles that carry no
//! vehicle (no MAVLink router runs there) are not guarded.

use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};

use crate::state::AppState;

/// How old the vehicle state may be before the vehicle is assumed armed.
pub const ARMED_STATE_MAX_AGE: Duration = Duration::from_secs(3);

/// The largest body the guard reads to look for the override. Every guarded
/// route takes a small JSON object.
const OVERRIDE_BODY_LIMIT: usize = 64 * 1024;

/// Exact `(method, path)` pairs refused while armed.
const GUARDED_EXACT: &[(&str, &str)] = &[
    // Companion reboot and the supervisor restart that cycles every service.
    ("POST", "/api/v1/setup/reboot"),
    ("POST", "/api/v1/system/restart-supervisor"),
    // Factory reset (drone and ground station) and the profile change.
    ("POST", "/api/v1/setup/reset"),
    ("POST", "/api/v1/ground-station/factory-reset"),
    ("POST", "/api/v1/setup/profile"),
    ("PUT", "/api/v1/ground-station/role"),
    // Radio channel hop and TX power, on the drone and the ground station.
    ("POST", "/api/wfb/channel"),
    ("PUT", "/api/wfb/tx-power"),
    ("PUT", "/api/v1/ground-station/wfb"),
    // Radio unpair: the drone's own, the station-wide one, and one fleet slot.
    ("POST", "/api/wfb/pair/unpair"),
    ("DELETE", "/api/v1/ground-station/wfb/pair"),
    // RC-module parameter write on the transmit lane.
    ("POST", "/api/v1/ground-station/crsf/params"),
];

/// Units whose restart drops the command, telemetry, video or RC link.
fn restart_drops_link(unit: &str) -> bool {
    matches!(
        unit,
        "ados-mavlink" | "ados-radio" | "ados-groundlink" | "ados-supervisor" | "ados-crsf"
    ) || unit.starts_with("ados-wfb")
}

/// Whether `method` on the decoded `path` is an action the interlock covers.
pub fn is_guarded(method: &str, path: &str) -> bool {
    let path = path
        .strip_suffix('/')
        .filter(|p| !p.is_empty())
        .unwrap_or(path);
    if GUARDED_EXACT
        .iter()
        .any(|(m, p)| *m == method && *p == path)
    {
        return true;
    }
    match method {
        // One fleet slot's release.
        "DELETE" => single_segment_after(path, "/api/v1/ground-station/wfb/pair/").is_some(),
        "POST" => {
            // A flight-controller parameter write.
            if single_segment_after(path, "/api/params/").is_some() {
                return true;
            }
            // A unit restart that drops a link.
            path.strip_prefix("/api/services/")
                .and_then(|rest| rest.strip_suffix("/restart"))
                .filter(|unit| !unit.contains('/'))
                .is_some_and(|unit| {
                    restart_drops_link(unit.strip_suffix(".service").unwrap_or(unit))
                })
        }
        _ => false,
    }
}

/// The one path segment after `prefix`, when `path` is exactly `prefix` plus a
/// non-empty segment.
fn single_segment_after<'a>(path: &'a str, prefix: &str) -> Option<&'a str> {
    path.strip_prefix(prefix)
        .filter(|seg| !seg.is_empty() && !seg.contains('/'))
}

/// Whether the vehicle should be treated as armed: the latest state says so,
/// or there is no state newer than [`ARMED_STATE_MAX_AGE`] to say otherwise.
pub fn vehicle_treated_as_armed(snapshot: Option<(Instant, Value)>) -> bool {
    match snapshot {
        Some((at, state)) if at.elapsed() <= ARMED_STATE_MAX_AGE => {
            state.get("armed").and_then(Value::as_bool) != Some(false)
        }
        _ => true,
    }
}

/// Whether the query string carries `force=1` (or `force=true`).
fn query_forces(query: Option<&str>) -> bool {
    query.is_some_and(|q| {
        q.split('&')
            .any(|pair| matches!(pair, "force=1" | "force=true"))
    })
}

/// Whether a JSON body carries `"force": true`.
fn body_forces(body: &[u8]) -> bool {
    serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|v| v.get("force").and_then(Value::as_bool))
        == Some(true)
}

/// Whether this node's profile carries a vehicle (and so runs the router whose
/// state the guard reads).
fn profile_has_vehicle(state: &AppState) -> bool {
    let cfg = crate::config::PairingConfig::load_from(&state.pairing_paths.config);
    let (profile, _role) = crate::profile::current_profile_and_role_at(
        &cfg.agent.profile,
        &state.pairing_paths.profile_conf,
        &state.pairing_paths.mesh_role,
    );
    matches!(profile.as_str(), "drone" | "ground-station")
}

/// The refusal every guarded action answers while armed.
pub fn armed_refusal() -> Response {
    (
        StatusCode::CONFLICT,
        Json(json!({
            "error": "E_ARMED",
            "message": "Vehicle is armed",
            "override": "force",
        })),
    )
        .into_response()
}

/// The router layer. See the module docs.
pub async fn armed_guard(State(state): State<AppState>, request: Request, next: Next) -> Response {
    let raw_path = request.uri().path();
    let path = crate::auth::decision_path(raw_path).unwrap_or_else(|_| raw_path.to_string());
    if !is_guarded(request.method().as_str(), &path) {
        return next.run(request).await;
    }
    if query_forces(request.uri().query()) {
        return next.run(request).await;
    }
    if !vehicle_treated_as_armed(state.state.snapshot_at()) || !profile_has_vehicle(&state) {
        return next.run(request).await;
    }

    let (parts, body) = request.into_parts();
    let bytes = match axum::body::to_bytes(body, OVERRIDE_BODY_LIMIT).await {
        Ok(bytes) => bytes,
        Err(_) => {
            return crate::routes::detail(
                StatusCode::PAYLOAD_TOO_LARGE,
                "Request body too large for this action.",
            )
        }
    };
    if !body_forces(&bytes) {
        return armed_refusal();
    }
    next.run(Request::from_parts(parts, Body::from(bytes)))
        .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    use axum::body::Body;
    use axum::http::Request as HttpRequest;
    use axum::routing::post;
    use axum::Router;
    use tower::util::ServiceExt;

    fn state_for_profile(profile: &str) -> (tempfile::TempDir, AppState) {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.yaml");
        std::fs::write(&config, format!("agent:\n  profile: {profile}\n")).unwrap();
        let pairing_json = dir.path().join("pairing.json");
        std::fs::write(&pairing_json, r#"{"paired": false}"#).unwrap();
        let paths = crate::state::PairingPaths {
            config,
            pairing_json: pairing_json.clone(),
            wfb_key_dir: dir.path().join("wfb"),
            bind_state: dir.path().join("bind-state.json"),
            profile_conf: dir.path().join("profile.conf"),
            mesh_role: dir.path().join("mesh-role"),
            relay_secret: dir.path().join("relay-peer-secret"),
        };
        let state = AppState::new(
            Arc::new(crate::auth::PairingState::with_path(pairing_json)),
            crate::ipc::StateIpcClient::disconnected(),
            crate::ipc::MavlinkIpcClient::new(dir.path().join("mavlink.sock")),
            crate::ipc::LogdQueryClient::new(dir.path().join("logd-query.sock")),
            dir.path().join("board.json"),
            paths,
            Arc::new(crate::dashboard_pin::DashboardPin::with_path(
                dir.path().join("dashboard-pin.json"),
            )),
            Arc::new(crate::mcp::McpTokenStore::with_path(
                dir.path().join("mcp-token.json"),
            )),
        );
        (dir, state)
    }

    /// A router with the guard over one native route and a fallback standing in
    /// for the proxy, each counting how often it ran.
    fn guarded_router(state: AppState, hits: Arc<AtomicUsize>) -> Router {
        let native_hits = Arc::clone(&hits);
        let fallback_hits = hits;
        Router::new()
            .route(
                "/api/v1/system/restart-supervisor",
                post(move || {
                    let hits = Arc::clone(&native_hits);
                    async move {
                        hits.fetch_add(1, Ordering::SeqCst);
                        "restarted"
                    }
                }),
            )
            .fallback(move || {
                let hits = Arc::clone(&fallback_hits);
                async move {
                    hits.fetch_add(1, Ordering::SeqCst);
                    "proxied"
                }
            })
            .layer(axum::middleware::from_fn_with_state(
                state.clone(),
                armed_guard,
            ))
            .with_state(state)
    }

    async fn send(router: Router, method: &str, uri: &str, body: &str) -> (StatusCode, Value) {
        let request = HttpRequest::builder()
            .method(method)
            .uri(uri)
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        let resp = router.oneshot(request).await.unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let value = serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()));
        (status, value)
    }

    #[tokio::test]
    async fn armed_supervisor_restart_is_refused_until_forced() {
        let (_dir, state) = state_for_profile("drone");
        state.state.set_snapshot_for_test(json!({"armed": true}));
        let hits = Arc::new(AtomicUsize::new(0));
        let router = guarded_router(state, Arc::clone(&hits));

        let (status, body) = send(
            router.clone(),
            "POST",
            "/api/v1/system/restart-supervisor",
            "",
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(body["error"], "E_ARMED");
        assert_eq!(body["override"], "force");
        assert_eq!(hits.load(Ordering::SeqCst), 0, "the handler must not run");

        let (status, _) = send(
            router.clone(),
            "POST",
            "/api/v1/system/restart-supervisor",
            r#"{"force":true}"#,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(hits.load(Ordering::SeqCst), 1);

        let (status, _) = send(
            router,
            "POST",
            "/api/v1/system/restart-supervisor?force=1",
            "",
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(hits.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn armed_proxied_reboot_is_refused_before_the_proxy() {
        let (_dir, state) = state_for_profile("drone");
        state.state.set_snapshot_for_test(json!({"armed": true}));
        let hits = Arc::new(AtomicUsize::new(0));
        let router = guarded_router(state, Arc::clone(&hits));

        let (status, body) = send(router.clone(), "POST", "/api/v1/setup/reboot", "").await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(body["error"], "E_ARMED");
        assert_eq!(
            hits.load(Ordering::SeqCst),
            0,
            "the proxy must not be reached"
        );

        // A percent-encoded spelling of the same path is the same action.
        let (status, _) = send(router, "POST", "/api/v1/setup/%72eboot", "").await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(hits.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn a_stale_or_missing_state_counts_as_armed() {
        let (_dir, state) = state_for_profile("drone");
        state.state.set_aged_snapshot_for_test(
            json!({"armed": false}),
            ARMED_STATE_MAX_AGE + Duration::from_secs(1),
        );
        let hits = Arc::new(AtomicUsize::new(0));
        let router = guarded_router(state.clone(), Arc::clone(&hits));
        let (status, _) = send(router, "POST", "/api/v1/system/restart-supervisor", "").await;
        assert_eq!(status, StatusCode::CONFLICT);

        let (_dir2, empty) = state_for_profile("ground_station");
        let router = guarded_router(empty, Arc::clone(&hits));
        let (status, _) = send(router, "POST", "/api/v1/setup/reboot", "").await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(hits.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn a_disarmed_vehicle_and_unguarded_routes_pass_through() {
        let (_dir, state) = state_for_profile("drone");
        state.state.set_snapshot_for_test(json!({"armed": false}));
        let hits = Arc::new(AtomicUsize::new(0));
        let router = guarded_router(state, Arc::clone(&hits));
        let (status, _) = send(
            router.clone(),
            "POST",
            "/api/v1/system/restart-supervisor",
            "",
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        let (_dir2, armed) = state_for_profile("drone");
        armed.state.set_snapshot_for_test(json!({"armed": true}));
        let router = guarded_router(armed, Arc::clone(&hits));
        let (status, _) = send(router, "POST", "/api/services/ados-video/restart", "").await;
        assert_eq!(
            status,
            StatusCode::OK,
            "a video restart does not drop the link"
        );
        assert_eq!(hits.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn an_armed_unknown_or_stale_vehicle_counts_as_armed() {
        let now = Instant::now();
        assert!(vehicle_treated_as_armed(None), "no state fails closed");
        assert!(vehicle_treated_as_armed(Some((
            now,
            json!({"armed": true})
        ))));
        assert!(!vehicle_treated_as_armed(Some((
            now,
            json!({"armed": false})
        ))));
        assert!(vehicle_treated_as_armed(Some((now, json!({})))));
        let stale = now - ARMED_STATE_MAX_AGE - Duration::from_secs(1);
        assert!(
            vehicle_treated_as_armed(Some((stale, json!({"armed": false})))),
            "state older than the bound proves nothing"
        );
    }

    #[tokio::test]
    async fn a_node_without_a_vehicle_is_not_guarded() {
        let (_dir, state) = state_for_profile("compute");
        let hits = Arc::new(AtomicUsize::new(0));
        let router = guarded_router(state, Arc::clone(&hits));
        let (status, _) = send(router, "POST", "/api/v1/setup/reboot", "").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(hits.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn the_real_router_refuses_an_armed_proxied_reboot() {
        let (_dir, state) = state_for_profile("drone");
        state.state.set_snapshot_for_test(json!({"armed": true}));
        let router = crate::routes::build_router(state, false);
        let (status, body) = send(router, "POST", "/api/v1/setup/reboot", "").await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(body["error"], "E_ARMED");
    }

    #[test]
    fn the_guarded_set_covers_each_link_dropping_action() {
        for (method, path) in [
            ("POST", "/api/v1/setup/reboot"),
            ("POST", "/api/v1/setup/reboot/"),
            ("POST", "/api/v1/system/restart-supervisor"),
            ("POST", "/api/services/ados-mavlink/restart"),
            ("POST", "/api/services/ados-wfb-rx/restart"),
            ("POST", "/api/services/ados-radio.service/restart"),
            ("POST", "/api/wfb/channel"),
            ("PUT", "/api/wfb/tx-power"),
            ("PUT", "/api/v1/ground-station/wfb"),
            ("DELETE", "/api/v1/ground-station/wfb/pair"),
            ("DELETE", "/api/v1/ground-station/wfb/pair/drone-1"),
            ("POST", "/api/v1/setup/reset"),
            ("POST", "/api/v1/ground-station/factory-reset"),
            ("POST", "/api/v1/setup/profile"),
            ("POST", "/api/params/FRAME_CLASS"),
            ("POST", "/api/v1/ground-station/crsf/params"),
        ] {
            assert!(is_guarded(method, path), "{method} {path} must be guarded");
        }
        for (method, path) in [
            ("GET", "/api/params/FRAME_CLASS"),
            ("GET", "/api/v1/ground-station/wfb"),
            ("POST", "/api/services/ados-video/restart"),
            ("POST", "/api/v1/ground-station/crsf/channels"),
            ("GET", "/api/wfb/pair"),
        ] {
            assert!(
                !is_guarded(method, path),
                "{method} {path} must not be guarded"
            );
        }
    }
}
