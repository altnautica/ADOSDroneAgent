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
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::json;

use ados_protocol::mcp_token::{scope_allows_class, ScopeClass};
use ados_protocol::pairing_posture::Pairing;
use ados_protocol::ws_ticket::{WsTicketIssuer, DEFAULT_TTL_SECONDS, MAX_TTL_SECONDS};

use crate::mcp::MCP_SCOPES_HEADER;
use crate::routes::detail;
use crate::state::AppState;

/// The scopes the agent will mint tickets for, each with the MCP scope class a
/// ticket holder gains. The agent is both issuer and validator; pinning the set
/// to the routes it knows about stops a stray client minting tickets for a scope
/// the agent never checks. Each entry must have a handler that verifies a ticket
/// for it (`gs.*` in [`crate::routes::gs_ws`], the MAVLink-WS scope in the router
/// proxy, the vision + setup streams, the plugin install-job progress stream).
///
/// The class is what a scoped MCP token must hold to mint the ticket: the
/// MAVLink WebSocket carries arbitrary commands to the flight controller, so its
/// ticket is flight-class; the others are read-only event streams.
const TICKET_SCOPES: [(&str, ScopeClass); 8] = [
    ("setup.cloudflare_logs", ScopeClass::Read),
    ("gs.pic_events", ScopeClass::Read),
    ("gs.mavlink_ws", ScopeClass::Flight),
    ("gs.uplink_events", ScopeClass::Read),
    ("gs.mesh_events", ScopeClass::Read),
    ("gs.button_events", ScopeClass::Read),
    ("vision.detections", ScopeClass::Read),
    ("plugins.install_job", ScopeClass::Read),
];

/// The scope class a ticket for `scope` grants its holder, or `None` for a scope
/// the agent does not mint.
fn ticket_scope_class(scope: &str) -> Option<ScopeClass> {
    TICKET_SCOPES
        .iter()
        .find(|(s, _)| *s == scope)
        .map(|(_, class)| *class)
}

/// Whether the credential that reached this route may hold a ticket of `class`.
///
/// A request admitted on a scoped MCP token carries the token's granted groups
/// on the edge-stamped [`MCP_SCOPES_HEADER`] (the edge strips any client value
/// first). MCP scopes are flat, so an `admin` token does not imply `flight`:
/// the ticket is minted only when the token holds the class itself. With no
/// such header the caller authenticated with the pairing key, a dashboard
/// session or on-box access, all of which already reach every class. A header
/// that is present but unreadable is refused rather than ignored.
fn credential_allows(headers: &HeaderMap, class: ScopeClass) -> bool {
    let Some(raw) = headers.get(MCP_SCOPES_HEADER) else {
        return true;
    };
    let Ok(raw) = raw.to_str() else {
        return false;
    };
    let granted: Vec<String> = raw
        .split(',')
        .map(str::trim)
        .filter(|g| !g.is_empty())
        .map(str::to_string)
        .collect();
    scope_allows_class(class, &granted)
}

#[derive(Deserialize)]
pub struct TicketRequest {
    pub scope: String,
    #[serde(default)]
    pub ttl_seconds: Option<i64>,
}

/// Mint a short-lived ticket for the named WebSocket scope. Authenticated by the
/// LAN-edge auth (`X-ADOS-Key` when paired); a scoped MCP token additionally
/// needs the ticket's scope class (see [`credential_allows`]), else `403`.
/// Returns the FastAPI-compatible `{ok, ticket, scope, expires_at}` shape; an
/// unknown scope is a 400 carrying the same
/// `{"detail": {"error": {"code": "E_UNKNOWN_SCOPE", ...}}}` body the Python
/// route emitted.
pub async fn mint_ws_ticket(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<TicketRequest>,
) -> Response {
    let Some(class) = ticket_scope_class(&req.scope) else {
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
    };
    if !credential_allows(&headers, class) {
        tracing::warn!(scope = %req.scope, "ws_ticket_mcp_scope_denied");
        return detail(
            StatusCode::FORBIDDEN,
            format!(
                "The presented MCP token lacks the '{}' scope this ticket requires.",
                class.group_name()
            ),
        );
    }

    // Default 30 s, capped at 120 s, floored at 1 s (the Python field was
    // `ge=1, le=120` with a 30 s default).
    let ttl = req
        .ttl_seconds
        .unwrap_or(DEFAULT_TTL_SECONDS)
        .clamp(1, MAX_TTL_SECONDS);

    // Key the ticket off the same pairing key the router validates against. When
    // unpaired there is no key: the WS proxy then decides a caller by its class
    // alone and never verifies a ticket, so minting under the empty-key issuer
    // keeps the response shape valid for the GCS flow without special-casing it.
    let issuer = match state.pairing.current() {
        Pairing::Paired(key) => WsTicketIssuer::from_api_key(&key),
        Pairing::Unpaired => WsTicketIssuer::from_api_key(""),
        // No key anyone holds: nothing minted here could be verified.
        Pairing::Unreadable => {
            return detail(StatusCode::SERVICE_UNAVAILABLE, "The pairing state on this device is unreadable. Unpair it on the device itself to recover.")
        }
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
    fn ticket_scopes_cover_the_validating_handlers() {
        // Drift guard: the agent is issuer + validator, so every scope a handler
        // verifies must be mintable here. The MAVLink-WS scope the router
        // validates against must be in the set, and the four `gs.*` stream scopes
        // the ground-station relays check must all be present.
        assert!(ticket_scope_class(SCOPE_MAVLINK_WS).is_some());
        for scope in [
            "gs.pic_events",
            "gs.uplink_events",
            "gs.mesh_events",
            "gs.button_events",
        ] {
            assert!(
                ticket_scope_class(scope).is_some(),
                "{scope} must be mintable"
            );
        }
        assert!(ticket_scope_class("plugins.install_job").is_some());
        assert_eq!(TICKET_SCOPES.len(), 8);
        assert_eq!(ticket_scope_class("gs.unknown"), None);
    }

    fn scopes_header(value: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(MCP_SCOPES_HEADER, value.parse().unwrap());
        h
    }

    /// MCP scopes are flat: `admin` does not imply `flight`. The MAVLink
    /// WebSocket ticket is a flight-control credential, so an admin-only token
    /// must not be able to buy one.
    #[test]
    fn the_mavlink_ticket_needs_the_flight_scope() {
        assert_eq!(
            ticket_scope_class(SCOPE_MAVLINK_WS),
            Some(ScopeClass::Flight)
        );
        assert!(!credential_allows(
            &scopes_header("read,admin"),
            ScopeClass::Flight
        ));
        assert!(credential_allows(
            &scopes_header("read, flight"),
            ScopeClass::Flight
        ));
        // The read-only streams need `read`.
        assert!(credential_allows(&scopes_header("read"), ScopeClass::Read));
        assert!(!credential_allows(
            &scopes_header("flight"),
            ScopeClass::Read
        ));
    }

    /// No MCP header means the key, a dashboard session or on-box access
    /// authenticated the request; those reach every class.
    #[test]
    fn a_request_without_mcp_scopes_is_not_restricted() {
        assert!(credential_allows(&HeaderMap::new(), ScopeClass::Flight));
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
