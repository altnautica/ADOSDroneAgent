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
//! `{"error":"E_NOT_A_VEHICLE","message":"..."}`; there is no override. The
//! one exception is a command-route request that stops the vehicle
//! (`disarm`, `killswitch`): it passes, so a hub that is somehow armed can
//! always be disarmed. Parameter reads and writes, sensor calibration,
//! firmware and info requests, time sync, signing and logs are not flight
//! actions and pass. Every other profile passes through unchanged.
//!
//! Like the armed interlock this is one router layer, so it covers the native
//! routes and the residual-proxied ones alike and runs before either.

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};

use ados_protocol::mcp_token::ScopeClass;

use crate::state::AppState;

/// The profile whose flight controller is a sensor hub.
const VISOR_PROFILE: &str = "visor";

/// The command route, bare or as a relay-proxy's inner path.
const COMMAND_ROUTE: &str = "/api/command";

/// Command-route commands that stop the vehicle (a plain and a forced
/// disarm); a visor passes them.
const STOP_COMMANDS: &[&str] = &["disarm", "killswitch"];

/// The largest command-route body the guard reads to find the command.
const COMMAND_BODY_LIMIT: usize = 64 * 1024;

/// Whether `method` on the decoded `path` is a flight action.
pub fn is_flight_action(method: &Method, path: &str) -> bool {
    crate::mcp::route_scope(method, path) == Some(ScopeClass::Flight)
}

/// Whether `path` is the command route, bare or wrapped in the relay-proxy.
fn is_command_route(path: &str) -> bool {
    crate::mcp::relay_proxy_inner_path(path).unwrap_or(path) == COMMAND_ROUTE
}

/// Whether a command-route body names a command that stops the vehicle
/// (matched as the route matches it, case-insensitively).
fn is_stop_command(body: &[u8]) -> bool {
    serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|v| v.get("cmd")?.as_str().map(str::to_lowercase))
        .is_some_and(|cmd| STOP_COMMANDS.contains(&cmd.as_str()))
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
    let path = path.strip_suffix('/').unwrap_or(&path).to_string();
    if !is_flight_action(request.method(), &path) || !node_is_visor(&state) {
        return next.run(request).await;
    }
    if is_command_route(&path) {
        let (parts, body) = request.into_parts();
        let Ok(bytes) = axum::body::to_bytes(body, COMMAND_BODY_LIMIT).await else {
            return not_a_vehicle_refusal();
        };
        if is_stop_command(&bytes) {
            return next
                .run(Request::from_parts(parts, Body::from(bytes)))
                .await;
        }
    }
    tracing::warn!(path = %path, "visor_flight_action_refused");
    not_a_vehicle_refusal()
}

#[cfg(test)]
mod tests;
