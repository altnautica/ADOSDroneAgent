//! The visor interlock: a node on the `visor` profile carries a flight
//! controller as a sensor hub, never as a vehicle, so it refuses every flight
//! action whatever the client.
//!
//! A flight action is what the MCP scope map classifies as
//! [`ScopeClass::Flight`] ([`crate::mcp::route_scope`]): the command route
//! (arm, disarm, takeoff, land, return, mode, motor cut, mission pause and
//! resume), CAN passthrough, RC channel injection and taking pilot-in-command,
//! each also when wrapped in the ground station's relay-proxy. Sharing that map
//! keeps one definition of a flight action. A plugin tool whose manifest
//! declares a `flight` safety class is refused by the tool-invoke route, which
//! alone knows the manifest (see [`is_flight_tool`]).
//!
//! On a visor each one answers `409` with
//! `{"error":"E_NOT_A_VEHICLE","message":"..."}`; there is no override.
//! Parameter reads and writes, sensor calibration, firmware and info requests,
//! time sync, signing and logs are not flight actions and pass. Every other
//! profile passes through unchanged.
//!
//! Like the armed interlock this is one router layer, so it covers the native
//! routes and the residual-proxied ones alike and runs before either.

use axum::extract::{Request, State};
use axum::http::{Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;

use ados_protocol::mcp_token::ScopeClass;

use crate::state::AppState;

/// The profile whose flight controller is a sensor hub.
const VISOR_PROFILE: &str = "visor";

/// Whether `method` on the decoded `path` is a flight action.
pub fn is_flight_action(method: &Method, path: &str) -> bool {
    crate::mcp::route_scope(method, path) == Some(ScopeClass::Flight)
}

/// Whether a plugin tool's declared safety class in `manifest` makes it a
/// flight action.
pub fn is_flight_tool(manifest: &ados_plugin_host::manifest::PluginManifest, tool: &str) -> bool {
    crate::mcp::tool_safety_class(manifest, tool)
        .and_then(|class| crate::mcp::tool_scope_class(&class))
        == Some(ScopeClass::Flight)
}

/// This node's profile in wire form, from the cached config snapshot so a
/// request does not parse `config.yaml` on the reactor.
pub(crate) fn node_profile(state: &AppState) -> String {
    let cfg = crate::config::PairingConfig::cached(&state.pairing_paths.config);
    crate::profile::current_profile_and_role_at(
        &cfg.agent.profile,
        &state.pairing_paths.profile_conf,
        &state.pairing_paths.mesh_role,
    )
    .0
}

/// Whether this node runs the visor profile.
pub fn node_is_visor(state: &AppState) -> bool {
    node_profile(state) == VISOR_PROFILE
}

/// The refusal every flight action answers on a visor.
pub fn not_a_vehicle_refusal() -> Response {
    (
        StatusCode::CONFLICT,
        Json(json!({
            "error": "E_NOT_A_VEHICLE",
            "message": "This node is a visor: its flight controller is a sensor hub, not a vehicle",
        })),
    )
        .into_response()
}

/// The router layer. See the module docs.
pub async fn vehicle_guard(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Response {
    let raw_path = request.uri().path();
    let path = crate::auth::decision_path(raw_path).unwrap_or_else(|_| raw_path.to_string());
    // A trailing slash is the one empty segment the decision path keeps.
    let path = path.strip_suffix('/').unwrap_or(&path);
    if is_flight_action(request.method(), path) && node_is_visor(&state) {
        tracing::warn!(method = %request.method(), path = %path, "visor_flight_action_refused");
        return not_a_vehicle_refusal();
    }
    next.run(request).await
}

#[cfg(test)]
mod tests;
