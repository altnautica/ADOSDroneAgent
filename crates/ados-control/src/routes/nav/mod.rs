//! The host-owned navigation and live-map surface:
//!
//! * `WS /api/v1/nav/stream` (ticket scope [`SCOPE_NAV_STREAM`]): frames
//!   `{kind: pose|scan|nav|follow|map|pilot|obstacles|ar|depth|surfaces, ts_ms, data}`,
//!   each kind at most 20 Hz,
//!   latest wins, plus `{kind: "follow_event", ts_ms, topic, data}` for
//!   `follow.reacquired` and `follow.lost`.
//! * `GET /api/v1/nav/status[?view=<camera_id>:<layer,…>]`: the latest of
//!   each kind as `{ts_ms, age_ms, data}`, `null` for a kind nothing has
//!   published, plus `view_demand` (the union of the live view leases).
//!
//! Host modules publish the kinds `pilot`, `obstacles`, `ar`, `depth` and
//! `surfaces` through [`NavHub::ingest_host`]. A stream client sends
//! `{"type":"view","camera_id","layers":[…]}` to lease the camera and overlay
//! layers it shows (see [`view`]); it then receives `ar`, `depth`,
//! `surfaces` and the per-camera part of `obstacles` for that camera only.
//! * `GET /api/v1/mission`: the router's mission mirror and the live mission
//!   state (see [`mission`]).
//! * `GET /api/v1/map/live?since=<seq>`: the live occupancy tiles changed after
//!   `seq`.
//!
//! The sources are plugin event-bus topics: the world-engine extension's pose,
//! scan, occupancy and map-status topics, vision-nav's `nav.state` and
//! follow-me's `follow.state` and follow events. They reach this process over
//! the plugin host's event export ([`crate::ipc::plugin_events_client`]), so
//! the operator surfaces read navigation without loading an extension UI. `data`
//! is the source payload verbatim.

mod hub;
mod mission;
mod stream;
mod tiles;
mod view;

#[cfg(test)]
mod tests;

pub use hub::{
    route, NavHub, NavKind, Routed, EXPORT_TOPICS, FOLLOW_ME_ID, KIND_COUNT, VISION_NAV_ID,
    WORLD_ENGINE_ID,
};
pub use mission::{get_mission, mission_body};
pub use view::{ViewDemand, ViewLease, VIEW_LAYERS, VIEW_LEASE_TTL};

use ados_protocol::pairing_posture::CallerClass;
use axum::extract::ws::WebSocketUpgrade;
use axum::extract::{Extension, Query, State};
use axum::http::HeaderMap;
use axum::response::Response;
use axum::Json;
use serde::Deserialize;
use serde_json::Value;

use crate::routes::gs_ws::{caller_of, decide_ws_auth, upgrade_with, ws_reject};
use crate::state::AppState;

/// The ticket scope a `/api/v1/nav/stream` upgrade must carry.
pub const SCOPE_NAV_STREAM: &str = "nav.stream";

/// The `GET /api/v1/nav/status` query.
#[derive(Debug, Default, Deserialize)]
pub struct NavStatusQuery {
    /// A polling client's view lease, `<camera_id>:<layer,…>`. Each request
    /// refreshes it; the per-camera kinds are narrowed to it.
    #[serde(default)]
    pub view: Option<String>,
}

/// `GET /api/v1/nav/status[?view=<camera_id>:<layer,…>]`.
pub async fn get_nav_status(
    State(state): State<AppState>,
    Query(query): Query<NavStatusQuery>,
) -> Json<Value> {
    Json(state.nav.status_body_for(query.view.as_deref()))
}

/// The `GET /api/v1/map/live` query.
#[derive(Debug, Deserialize)]
pub struct MapLiveQuery {
    /// The seq the client already holds; absent means "everything".
    #[serde(default)]
    pub since: u64,
}

/// `GET /api/v1/map/live?since=<seq>`.
pub async fn get_map_live(
    State(state): State<AppState>,
    Query(query): Query<MapLiveQuery>,
) -> Json<Value> {
    Json(state.nav.map_live_body(query.since))
}

/// `WS /api/v1/nav/stream`. The upgrade bypasses the HTTP key gate (a browser
/// cannot set a header on it), so the handshake is authenticated here exactly
/// like the ground-station streams: a header key, or a one-shot ticket minted
/// for [`SCOPE_NAV_STREAM`]. Served on every profile.
pub async fn ws_nav_stream(
    State(state): State<AppState>,
    headers: HeaderMap,
    caller: Option<Extension<CallerClass>>,
    ws: WebSocketUpgrade,
) -> Response {
    let auth = decide_ws_auth(&state, &headers, SCOPE_NAV_STREAM, caller_of(caller));
    let Some((ws, auth)) = upgrade_with(ws, auth) else {
        return ws_reject();
    };
    let hub = state.nav.clone();
    ws.on_upgrade(move |socket| stream::nav_stream_loop(socket, hub, auth))
}
