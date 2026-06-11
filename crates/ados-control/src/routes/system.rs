//! System routes: liveness and version/capability negotiation.
//!
//! These two routes are static (no IPC): `/healthz` is the liveness probe and
//! `/api/version` is the wire-protocol version + capability flag list the GCS
//! reads on first connect to decide which features it can rely on. The native
//! surface must answer both byte-identically to the FastAPI surface so the same
//! GCS works against either.

use axum::extract::State;
use axum::Json;
use serde_json::{json, Value};

use crate::state::AppState;

/// Wire-protocol contract version. Bump when the request/response shape of any
/// `/api/*` endpoint changes in a way the GCS must adapt to. The GCS reads this
/// and picks compatible code paths. Mirrors the Python `API_VERSION`.
pub const API_VERSION: &str = "1";

/// Capability flags. Add a new flag whenever a new endpoint or behaviour ships
/// that the GCS may want to gate on. Never rename or remove a flag once shipped
/// — an older GCS may rely on the absence to take a fallback code path. This
/// list is the canonical surface contract between the agent and the GCS, kept in
/// lock-step with the Python `CAPABILITIES` list (order included, since it is
/// emitted as a JSON array).
pub const CAPABILITIES: [&str; 16] = [
    // /api/status/full consolidated endpoint (fewer round-trips).
    "status.full",
    // /api/version endpoint (this one). Trivially true.
    "version.endpoint",
    // /api/services granular service control.
    "services.control",
    // /api/video/* live video pipeline state + transport switcher.
    "video.pipeline",
    // /api/wfb/* WFB-ng radio link control + telemetry.
    "wfb.link",
    // Retired capability. The endpoint it gated no longer ships, but the flag
    // stays in the list because this surface contract is append-only: an older
    // GCS may key a fallback path on its presence or absence, so the token is
    // never renamed or removed once shipped.
    "scripts.runtime",
    // /api/ota/* over-the-air updater.
    "ota.updater",
    // /api/pairing/* device-link mnemonic + token rotation.
    "pairing.mnemonic",
    // /api/pairing/info carries a folded bind_state + radio snapshot.
    "pairing.bind_state",
    // /api/peripherals/* legacy hardware scan + /v1 plugin registry.
    "peripherals.registry",
    // /api/fleet/* fleet roster surface.
    "fleet.roster",
    // /api/features/* HAL feature catalog.
    "features.catalog",
    // /api/ground-station/* full ground-agent profile surface.
    "ground_station.profile",
    // /api/signing/* MAVLink v2 signing key enrollment.
    "signing.mavlink",
    // WebRTC SDP signaling broker rejection surfaced via cloud status.
    "webrtc.signaling.last_error",
    // /api/can/passthrough route presence. Today the route returns 501; the flag
    // lets the GCS detect whether the surface exists at all so it can fall back
    // to MAVLink CAN_FORWARD without probing.
    "can.passthrough",
];

/// `GET /api/version` → `{api_version, agent_version, capabilities}`. Stable
/// shape; mirrors `version.py:get_version`.
pub async fn get_version(State(state): State<AppState>) -> Json<Value> {
    Json(json!({
        "api_version": API_VERSION,
        "agent_version": state.agent_version,
        "capabilities": CAPABILITIES,
    }))
}

/// `GET /healthz` → `{status: "ok", version}`. The liveness probe; mirrors
/// `server.py:health_check`.
pub async fn healthz(State(state): State<AppState>) -> Json<Value> {
    Json(json!({
        "status": "ok",
        "version": state.agent_version,
    }))
}
