//! The armed interlock: refuse the actions that can drop the aircraft's link or
//! change how it flies while the vehicle is armed.
//!
//! The covered actions, each of which either cuts the command, telemetry, video
//! or RC link or changes flight behaviour:
//!
//! - rebooting the companion and restarting the supervisor;
//! - restarting a link-carrying unit (a unit-table row with `drops_link`),
//!   directly or through a peripheral action;
//! - hopping the radio channel, changing its TX power, unpairing a radio, and
//!   opening a local radio bind;
//! - changing the node's profile or role, and a ground-station factory reset;
//! - enrolling or disabling MAVLink signing on the flight controller;
//! - a flight-controller parameter write and an RC-module parameter write;
//! - joining, leaving or forgetting a Wi-Fi network, re-tagging an adapter's
//!   MAC, and the ground station's AP, uplink-share, client, ethernet and mesh
//!   writes;
//! - installing, enabling, disabling or removing a plugin (a plugin may feed
//!   pose or setpoints to the flight controller);
//! - body-scoped: `POST /api/v1/setup/apply` carrying a `profile`, `wfb`,
//!   `network` or `regulatory` section, and `PUT /api/config` of a profile,
//!   role, radio or regulatory key.
//!
//! While the vehicle is armed each one answers `409` with
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
//! The armed source is the router's vehicle-state snapshot, judged by
//! [`vehicle_treated_as_armed`]: a snapshot older than
//! [`ados_protocol::armed::ARMED_STATE_MAX_AGE`],
//! or none at all, proves nothing about the vehicle, so the node is then
//! treated as armed. Profiles that carry no vehicle (no MAVLink router runs
//! there) are not guarded.

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};

use ados_protocol::armed::vehicle_treated_as_armed;

use crate::state::AppState;

/// The largest body the guard reads to look for the override. Every guarded
/// route takes a small JSON object.
const OVERRIDE_BODY_LIMIT: usize = 64 * 1024;

/// Exact `(method, path)` pairs refused while armed.
const GUARDED_EXACT: &[(&str, &str)] = &[
    // Companion reboot and power-off, and the supervisor restart that cycles
    // every service.
    ("POST", "/api/v1/setup/reboot"),
    ("POST", "/api/v1/setup/shutdown"),
    ("POST", "/api/v1/system/restart-supervisor"),
    // Ground-station factory reset and the profile and role changes.
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
    // A local bind stops the normal radio unit for the bind window.
    ("POST", "/api/wfb/pair/local-bind"),
    // Flight-controller link signing.
    ("POST", "/api/mavlink/signing/enroll-fc"),
    ("POST", "/api/mavlink/signing/disable-on-fc"),
    // RC-module parameter write on the transmit lane.
    ("POST", "/api/v1/ground-station/crsf/params"),
    // Leaving the Wi-Fi client network.
    ("DELETE", "/api/v1/network/client"),
    // Ground-station network and mesh writes.
    ("PUT", "/api/v1/ground-station/network/ap"),
    ("PUT", "/api/v1/ground-station/network/share_uplink"),
    ("PUT", "/api/v1/ground-station/network/client/join"),
    ("PUT", "/api/v1/ground-station/network/ethernet"),
    ("PUT", "/api/v1/ground-station/mesh/config"),
    ("PUT", "/api/v1/ground-station/mesh/gateway_preference"),
    // Plugin installs (an install replaces a running plugin).
    ("POST", "/api/plugins/install"),
    ("POST", "/api/plugins/install_from_url"),
    ("POST", "/api/plugins/install_builtin"),
];

/// Whether restarting `unit` drops the command, telemetry, video, RC, mesh,
/// access-point or cloud link: the unit table's `drops_link` column. The name
/// is normalized the way the restart route normalizes it (a bare `wfb` is
/// `ados-wfb`), so the guard and the route always judge the same unit.
fn restart_drops_link(unit: &str) -> bool {
    let unit = unit.strip_suffix(".service").unwrap_or(unit);
    let found = if unit.starts_with("ados-") {
        ados_protocol::units::find(unit)
    } else {
        ados_protocol::units::find(&format!("ados-{unit}"))
    };
    found.is_some_and(|u| u.drops_link)
}

/// Config keys whose write switches the profile or role, or retunes the radio
/// or its regulatory domain.
const GUARDED_CONFIG_KEYS: &[&str] = &[
    "agent.profile",
    "ground_station.role",
    "video.wfb.channel",
    "video.wfb.tx_power_dbm",
    "video.wfb.mcs",
    "network.regulatory",
    "lora.region",
];

/// Setup-apply sections that restart the supervisor or retune a link.
const GUARDED_APPLY_SECTIONS: &[&str] = &["profile", "wfb", "network", "regulatory"];

fn apply_body_is_guarded(body: &Value) -> bool {
    GUARDED_APPLY_SECTIONS
        .iter()
        .any(|section| body.get(*section).is_some_and(|v| !v.is_null()))
}

fn config_body_is_guarded(body: &Value) -> bool {
    body.get("key")
        .and_then(Value::as_str)
        .is_some_and(|key| GUARDED_CONFIG_KEYS.contains(&key))
}

/// A LoRa write that changes the region (the plan the lane transmits under).
fn lora_body_is_guarded(body: &Value) -> bool {
    body.get("region").is_some()
}

/// The body predicate for a route guarded only for some bodies.
fn body_rule(method: &str, path: &str) -> Option<fn(&Value) -> bool> {
    match (method, trim_trailing_slash(path)) {
        ("POST", "/api/v1/setup/apply") => Some(apply_body_is_guarded),
        ("PUT", "/api/config") => Some(config_body_is_guarded),
        ("PUT", "/api/v1/lora") => Some(lora_body_is_guarded),
        _ => None,
    }
}

fn trim_trailing_slash(path: &str) -> &str {
    path.strip_suffix('/')
        .filter(|p| !p.is_empty())
        .unwrap_or(path)
}

/// Whether `method` on the decoded `path` is an action the interlock covers
/// regardless of its body.
pub fn is_guarded(method: &str, path: &str) -> bool {
    let path = trim_trailing_slash(path);
    if GUARDED_EXACT
        .iter()
        .any(|(m, p)| *m == method && *p == path)
    {
        return true;
    }
    match method {
        "DELETE" => {
            // One fleet slot's release.
            single_segment_after(path, "/api/v1/ground-station/wfb/pair/").is_some()
                // Forgetting a saved network, and a MAC re-tag's removal.
                || any_depth_after(path, "/api/v1/network/client/").is_some()
                || any_depth_after(path, "/api/v1/network/mac/").is_some()
                // Removing a plugin.
                || single_segment_after(path, "/api/plugins/").is_some()
        }
        "PUT" => any_depth_after(path, "/api/v1/network/client/").is_some(),
        "POST" => {
            // A flight-controller parameter write.
            if single_segment_after(path, "/api/params/").is_some() {
                return true;
            }
            // A MAC re-tag.
            if any_depth_after(path, "/api/v1/network/mac/").is_some() {
                return true;
            }
            // Enabling or disabling a plugin.
            if let Some(rest) = path.strip_prefix("/api/plugins/") {
                if let Some((id, verb)) = rest.split_once('/') {
                    if !id.is_empty() && matches!(verb, "enable" | "disable") {
                        return true;
                    }
                }
            }
            // A peripheral action (a radio restart among them).
            if let Some(rest) = path.strip_prefix("/api/v1/peripherals/") {
                if rest
                    .strip_suffix("/action")
                    .is_some_and(|id| !id.is_empty() && !id.contains('/'))
                {
                    return true;
                }
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

/// The non-empty remainder after `prefix`, at any depth.
fn any_depth_after<'a>(path: &'a str, prefix: &str) -> Option<&'a str> {
    path.strip_prefix(prefix).filter(|rest| !rest.is_empty())
}

/// Whether the query string carries `force=1` (or `force=true`).
fn query_forces(query: Option<&str>) -> bool {
    query.is_some_and(|q| {
        q.split('&')
            .any(|pair| matches!(pair, "force=1" | "force=true"))
    })
}

/// Whether a parsed JSON body carries `"force": true`.
fn body_forces(body: Option<&Value>) -> bool {
    body.and_then(|v| v.get("force").and_then(Value::as_bool)) == Some(true)
}

/// Whether this node's profile carries a vehicle (and so runs the router whose
/// state the guard reads). The profile comes from the cached config snapshot,
/// so a guarded request does not parse `config.yaml` on the reactor.
fn profile_has_vehicle(state: &AppState) -> bool {
    let cfg = crate::config::PairingConfig::cached(&state.pairing_paths.config);
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
    let method = request.method().as_str();
    let rule = body_rule(method, &path);
    if !is_guarded(method, &path) && rule.is_none() {
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
    let parsed = serde_json::from_slice::<Value>(&bytes).ok();
    let rule_matches = match (rule, parsed.as_ref()) {
        (Some(predicate), Some(value)) => predicate(value),
        // A body rule with an unparseable body: the route itself rejects it.
        (Some(_), None) => false,
        (None, _) => true,
    };
    if rule_matches && !body_forces(parsed.as_ref()) {
        return armed_refusal();
    }
    next.run(Request::from_parts(parts, Body::from(bytes)))
        .await
}

#[cfg(test)]
mod tests;
