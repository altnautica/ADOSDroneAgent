//! Native WebSocket-ticket mint: `POST /api/_ws/ticket`.
//!
//! A browser cannot set `X-ADOS-Key` on a WebSocket handshake, so the GCS first
//! exchanges its pairing key (enforced by this surface's LAN-edge auth, since the
//! route is NOT in the public-exempt set) for a short-lived ticket and hands it to
//! `new WebSocket(url, ["ados-ws-ticket", <ticket>])`. Unlike the prior Python
//! design (a random string in an in-process store), the ticket is a self-contained
//! HMAC token keyed off the same `pairing.json` both daemons read, so the native
//! MAVLink-router WS proxy validates it with no shared state — see
//! [`ados_protocol::ws_ticket`]. This route replaces the Python `ws_tickets.py`
//! mint; registering it natively shadows that proxied route.

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::json;

use ados_protocol::pairing_posture::Pairing;
use ados_protocol::ws_ticket::{WsTicketIssuer, DEFAULT_TTL_SECONDS, MAX_TTL_SECONDS};

use crate::state::AppState;

/// The scopes the agent will mint tickets for. The agent is both issuer and
/// validator; pinning the set to the routes it knows about stops a stray client
/// minting tickets for a scope the agent never checks. Mirrors
/// `ados.api.routes.ws_tickets.ALLOWED_SCOPES` exactly.
const ALLOWED_SCOPES: [&str; 6] = [
    "setup.cloudflare_logs",
    "gs.pic_events",
    "gs.mavlink_ws",
    "gs.uplink_events",
    "gs.mesh_events",
    "vision.detections",
];

#[derive(Deserialize)]
pub struct TicketRequest {
    pub scope: String,
    #[serde(default)]
    pub ttl_seconds: Option<i64>,
}

/// Mint a short-lived ticket for the named WebSocket scope. Authenticated by the
/// LAN-edge auth (`X-ADOS-Key` when paired). Returns the FastAPI-compatible
/// `{ok, ticket, scope, expires_at}` shape; an unknown scope is a 400 carrying the
/// same `{"detail": {"error": {"code": "E_UNKNOWN_SCOPE", ...}}}` body the Python
/// route emitted.
pub async fn mint_ws_ticket(
    State(state): State<AppState>,
    Json(req): Json<TicketRequest>,
) -> Response {
    if !ALLOWED_SCOPES.contains(&req.scope.as_str()) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "detail": {
                    "error": {
                        "code": "E_UNKNOWN_SCOPE",
                        "message": format!(
                            "scope '{}' is not a known WebSocket route",
                            req.scope
                        ),
                    }
                }
            })),
        )
            .into_response();
    }

    // Default 30 s, capped at 120 s, floored at 1 s (the Python field was
    // `ge=1, le=120` with a 30 s default).
    let ttl = req
        .ttl_seconds
        .unwrap_or(DEFAULT_TTL_SECONDS)
        .clamp(1, MAX_TTL_SECONDS);

    // Key the ticket off the same pairing key the router validates against. When
    // unpaired the data plane is open (the WS proxy admits without a ticket), so
    // the ticket is never verified; minting under the empty-key issuer keeps the
    // response shape valid for the GCS flow without special-casing it.
    let issuer = match state.pairing.current() {
        Pairing::Paired(key) => WsTicketIssuer::from_api_key(&key),
        Pairing::Unpaired => WsTicketIssuer::from_api_key(""),
    };
    let ticket = issuer.mint(&req.scope, ttl);

    Json(json!({
        "ok": true,
        "ticket": ticket.token,
        "scope": ticket.scope,
        "expires_at": ticket.expires_at,
    }))
    .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ados_protocol::ws_ticket::SCOPE_MAVLINK_WS;

    #[test]
    fn allowed_scopes_match_the_python_set() {
        // Drift guard: the agent is issuer + validator, so this set must equal
        // `ados.api.routes.ws_tickets.ALLOWED_SCOPES`. The MAVLink-WS scope the
        // router validates against must be in it.
        assert!(ALLOWED_SCOPES.contains(&SCOPE_MAVLINK_WS));
        assert_eq!(ALLOWED_SCOPES.len(), 6);
    }

    #[test]
    fn a_minted_ticket_verifies_under_the_same_key() {
        // What the route does (paired branch) must produce a token the router's
        // identical `from_api_key(key).verify(...)` accepts for the scope + now.
        let issuer = WsTicketIssuer::from_api_key("ados_secret");
        let t = issuer.mint(SCOPE_MAVLINK_WS, DEFAULT_TTL_SECONDS);
        let now = ados_protocol::ws_ticket::now_unix();
        assert!(WsTicketIssuer::from_api_key("ados_secret")
            .verify(&t.token, SCOPE_MAVLINK_WS, now)
            .is_ok());
    }
}
