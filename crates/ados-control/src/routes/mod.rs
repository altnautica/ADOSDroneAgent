//! The route surface: one axum `Router` served on both listeners.
//!
//! The native control surface answers the same `/api/*` (+ `/healthz`) routes
//! the FastAPI surface does, byte-identically, so the same GCS works against
//! either. This surface registers `/healthz`, `/api/version`, `/api/status`,
//! `/api/telemetry`, `/api/time`, `/api/params`, `/api/services`, the two
//! `/api/fleet/*` routes, the three `/api/mavlink/signing/*` reads, the four
//! `/api/wfb*` reads, the four `/api/pairing/*` routes, and the two
//! `/api/command{,s}` routes. Every other path falls through to the proxy.
//!
//! Error bodies use FastAPI's `{"detail": "..."}` shape on 4xx/5xx, NOT the
//! logd read-API's `{"error": {...}}` envelope, because the GCS already parses
//! the agent's `{"detail"}` errors. The proxy fallback and the [`detail`] helper
//! enforce that one shape everywhere on this surface.
//!
//! INVARIANT: every route registered in [`build_router`] MUST have a matching
//! entry in [`crate::routing::native_routes`]. The LAN-edge auth applies its
//! posture only to native paths; a route served here but missing from the native
//! set would be served with the auth SKIPPED. The `native_set_matches_router`
//! test pins the full set so the two never drift.

pub mod command;
pub mod fleet;
pub mod gs_mesh;
pub mod gs_network;
pub mod gs_pairing;
pub mod gs_status;
pub mod pairing;
pub mod params;
pub mod services;
pub mod signing;
pub mod status;
pub mod status_full;
pub mod system;
pub mod video;
pub mod wfb;

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::json;

use crate::proxy::proxy_to_residual;
use crate::state::AppState;

/// Build a FastAPI-shaped error response: `(status, {"detail": message})`. Every
/// 4xx/5xx on this surface goes through this so the body shape never drifts to
/// the logd `{"error":{...}}` envelope. Used by the routes that land in later
/// chunks (pairing 409s, command 503/400) as well as the proxy's
/// graceful-degradation reply.
pub fn detail(status: StatusCode, message: impl Into<String>) -> Response {
    (status, Json(json!({ "detail": message.into() }))).into_response()
}

/// Build the route Router for a given app state. The same Router is served on
/// both edges; the auth/rate-limit layer is added per edge by the serve loop.
/// `/healthz` sits at the root; everything else is mounted under `/api`.
///
/// Any path not registered here falls through to the reverse-proxy fallback,
/// which forwards it to the residual Python over its internal Unix socket (and
/// degrades cleanly to a FastAPI-shaped `{"detail"}` when that upstream is
/// absent), so the front serves the migrated routes natively and proxies the
/// rest while the migration is in flight.
pub fn build_router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(system::healthz))
        .route("/api/version", get(system::get_version))
        .route("/api/status", get(status::get_status))
        .route("/api/telemetry", get(status::get_telemetry))
        .route("/api/time", get(system::get_time))
        // Pairing: the node-identity probe + the local pairing handshake. info /
        // code / claim are public (the auth-exempt set); unpair requires the key.
        .route("/api/pairing/info", get(pairing::get_pairing_info))
        .route("/api/pairing/code", get(pairing::get_pairing_code))
        .route("/api/pairing/claim", post(pairing::claim_pairing))
        .route("/api/pairing/unpair", post(pairing::unpair))
        // Command: the fire-and-forget text-command executor (auth-gated when
        // paired) + the catalog. The executor builds a MAVLink frame and writes
        // it to the mavlink socket; the catalog is the static command list.
        .route("/api/command", post(command::execute_command))
        .route("/api/commands", get(command::list_commands))
        // Params: the full cached FC parameter list (the single-param route is a
        // path-param path and stays proxied until the matcher lands).
        .route("/api/params", get(params::get_all_params))
        // Services: the live `ados-*.service` unit inventory with per-service
        // memory + the serving process's own metrics.
        .route("/api/services", get(services::list_services))
        // Fleet roster: the opt-in mesh awareness surface. Both static on this
        // device — enrollment reports not-enrolled, peers is the empty list.
        .route("/api/fleet/enrollment", get(fleet::get_enrollment))
        .route("/api/fleet/peers", get(fleet::list_peers))
        // MAVLink v2 signing reads: FC capability, the require-flag value, and the
        // observational signed-frame counters (the write routes stay proxied).
        .route("/api/mavlink/signing/capability", get(signing::capability))
        .route("/api/mavlink/signing/require", get(signing::require))
        .route("/api/mavlink/signing/counters", get(signing::counters))
        // WFB radio reads: link status, link-quality history, pair-state, and the
        // failover state (the channel / tx-power writes stay proxied).
        .route("/api/wfb", get(wfb::get_wfb_status))
        .route("/api/wfb/history", get(wfb::get_wfb_history))
        .route("/api/wfb/pair", get(wfb::get_wfb_pair_status))
        .route("/api/wfb/pair/failover-status", get(wfb::get_failover_status))
        // The consolidated status: agent info, services, resources, video,
        // telemetry, radio, and mesh in one round-trip.
        .route("/api/status/full", get(status_full::get_full_status))
        // Video reads: glass-to-glass latency, the air-side pipeline snapshot, and
        // the encoder/radio config (the snapshot/record/switch writes + the
        // camera-enumeration route stay proxied).
        .route("/api/video/latency", get(video::get_video_latency))
        .route("/api/v1/video/air-pipeline", get(video::get_air_pipeline_status))
        .route("/api/video/config", get(video::get_video_config))
        // Ground-station profile reads (404 off a drone): the status snapshot, the
        // stored radio config, and the three distributed-receive role reads.
        .route("/api/v1/ground-station/status", get(gs_status::get_status))
        .route("/api/v1/ground-station/wfb", get(gs_status::get_wfb))
        .route("/api/v1/ground-station/wfb/relay/status", get(gs_status::get_wfb_relay_status))
        .route("/api/v1/ground-station/wfb/receiver/relays", get(gs_status::get_wfb_receiver_relays))
        .route("/api/v1/ground-station/wfb/receiver/combined", get(gs_status::get_wfb_receiver_combined))
        // Ground-station mesh reads (profile-gated): the role + capability hint,
        // the batman-adv state + its three slices, and the configured transport.
        .route("/api/v1/ground-station/role", get(gs_mesh::get_role))
        .route("/api/v1/ground-station/mesh", get(gs_mesh::get_mesh_health))
        .route("/api/v1/ground-station/mesh/neighbors", get(gs_mesh::get_mesh_neighbors))
        .route("/api/v1/ground-station/mesh/routes", get(gs_mesh::get_mesh_routes))
        .route("/api/v1/ground-station/mesh/gateways", get(gs_mesh::get_mesh_gateways))
        .route("/api/v1/ground-station/mesh/config", get(gs_mesh::get_mesh_config))
        // Ground-station network uplink reads (404 off a drone): the aggregate
        // view, ethernet, client scan, modem, the priority list, and cellular.
        .route("/api/v1/ground-station/network", get(gs_network::get_ground_station_network))
        .route("/api/v1/ground-station/network/ethernet", get(gs_network::get_network_ethernet))
        .route("/api/v1/ground-station/network/client/scan", get(gs_network::get_network_client_scan))
        .route("/api/v1/ground-station/network/modem", get(gs_network::get_network_modem))
        .route("/api/v1/ground-station/network/priority", get(gs_network::get_network_priority))
        .route("/api/v1/ground-station/modem-status", get(gs_network::get_modem_status))
        // Ground-station reads (profile-gated): the mesh pairing snapshot, the PIC
        // arbiter state, and the captive-portal token mint.
        .route("/api/v1/ground-station/pair/pending", get(gs_pairing::get_pair_pending))
        .route("/api/v1/ground-station/pic", get(gs_pairing::get_pic_state))
        .route("/api/v1/ground-station/captive-token", get(gs_pairing::get_captive_token))
        // Everything else: reverse-proxy to the residual Python.
        .fallback(proxy_to_residual)
        .with_state(state)
}
