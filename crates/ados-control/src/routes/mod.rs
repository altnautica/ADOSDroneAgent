//! The route surface: one axum `Router` served on both listeners.
//!
//! Every native route is registered through the [`table::Routes`] builder in
//! `register/`, one file per domain. The builder records each registration in
//! the native route table as it adds it to the router, so the table the LAN-edge
//! auth consults ([`crate::routing`]) and the router that serves are one list:
//! a route cannot be served without being native, nor listed without being
//! served. Every other path goes to the fallback dispatch.
//!
//! Error bodies use FastAPI's `{"detail": "..."}` shape on 4xx/5xx, NOT the
//! logd read-API's `{"error": {...}}` envelope, because the GCS already parses
//! the agent's `{"detail"}` errors. The proxy fallback and the [`detail`] helper
//! enforce that one shape everywhere on this surface.

pub mod battery;
pub mod camera_config;
pub mod can;
pub mod cloud_link;
pub mod command;
pub mod config_rw;
pub mod config_schema;
pub mod dashboard;
pub mod dashboard_pin;
pub mod diag_storage;
pub mod diagnostics;
pub mod fc_identity;
pub mod fleet;
pub mod gs_bluetooth;
pub mod gs_camera_write;
pub mod gs_cmd;
pub mod gs_crsf;
pub mod gs_factory_reset;
pub mod gs_fleet_enroll;
pub mod gs_fleet_hero;
pub mod gs_fleet_slot;
pub mod gs_gamepad_write;
pub mod gs_input_read;
pub mod gs_mesh;
pub mod gs_mesh_write;
pub mod gs_network;
pub mod gs_network_write;
pub mod gs_pairing;
pub mod gs_pic;
pub mod gs_recording;
pub mod gs_recording_list;
pub mod gs_relay_proxy;
pub mod gs_relayed_status;
pub mod gs_status;
pub mod gs_tunnel_config;
pub mod gs_ui_read;
pub mod gs_ui_write;
pub mod gs_wfb_invite;
pub mod gs_wfb_pair;
pub mod gs_wfb_write;
pub mod gs_ws;
pub mod hls;
pub mod logs;
pub mod logs_write;
pub mod mac_adapters;
pub mod mac_pin;
pub mod mavlink_ports;
pub mod mcp;
pub mod network_client_read;
pub mod network_write;
pub mod observability;
pub mod pairing;
pub mod params;
pub mod params_single;
pub mod params_write;
pub mod peer_backfill;
pub mod plugins_config;
pub mod plugins_lifecycle;
pub mod plugins_proxy;
pub mod plugins_state;
pub mod plugins_tools;
pub mod reachable_addr;
pub mod relay_secret;
pub mod service_control;
pub mod services;
pub mod signing;
pub mod signing_write;
pub mod static_ui;
pub mod status;
pub mod status_full;
pub mod swarm;
pub mod system;
pub mod system_resources;
pub mod table;
pub mod video;
pub mod video_config_write;
pub mod video_profile;
pub mod video_snapshot;
pub mod video_status;
pub mod vision;
pub mod vision_detector;
pub mod vision_upload;
pub mod wfb;
pub mod wfb_local_bind;
pub mod wfb_pair_write;
pub mod wfb_write;
pub mod whep;
pub mod ws_ticket;

mod register;

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::{Json, Router};
use serde_json::json;

use crate::proxy::non_native_fallback;
use crate::state::AppState;
use table::{NativeRoute, Routes};

/// Build a FastAPI-shaped error response: `(status, {"detail": message})`. Every
/// 4xx/5xx on this surface goes through this so the body shape never drifts to
/// the logd `{"error":{...}}` envelope.
pub fn detail(status: StatusCode, message: impl Into<String>) -> Response {
    (status, Json(json!({ "detail": message.into() }))).into_response()
}

/// Every native route: the router (state not yet applied, no fallback) and the
/// table of what it registers. The HID writes are included only when
/// `hid_native` is set, because their daemons' sockets exist only then.
pub fn build(hid_native: bool) -> (Router<AppState>, Vec<NativeRoute>) {
    register::register_all(Routes::new(), hid_native).into_parts()
}

/// The native route table alone, for the tools that enumerate the surface (the
/// `docs/api-surface.md` generator, the edge's own tests).
pub fn describe(include_hid: bool) -> Vec<NativeRoute> {
    build(include_hid).1
}

/// Build the served Router for a given app state, and install its route table
/// as the one the LAN edge consults. The same Router is served on both edges;
/// the auth/rate-limit layer is added per edge by the serve loop.
///
/// Any path not registered here goes to the fallback
/// ([`crate::proxy::non_native_fallback`]), which dispatches on
/// [`crate::routing::classify`]: only the permanent-Python prefixes are
/// forwarded to the residual app over its internal Unix socket; `/hls/*` is
/// relayed to mediamtx, the operator UI is served from its bundles, and any
/// other `/api` path is a FastAPI-shaped 404.
pub fn build_router(state: AppState, hid_native: bool) -> Router {
    let (router, table) = build(hid_native);
    crate::routing::install(table);
    // The armed interlock wraps the native routes and the fallback alike, so it
    // runs before either.
    router
        .fallback(non_native_fallback)
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            crate::armed_guard::armed_guard,
        ))
        .with_state(state)
}

#[cfg(test)]
#[path = "router_tests.rs"]
mod router_tests;

#[cfg(test)]
#[path = "recording_route_tests.rs"]
mod recording_route_tests;
