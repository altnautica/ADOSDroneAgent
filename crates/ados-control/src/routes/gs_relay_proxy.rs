//! Ground-station relay-proxy route: forward an HTTP-shaped request to a
//! WFB-linked drone the ground station has no IP reach to.
//!
//! **`{GET,POST,PUT,DELETE,PATCH} /api/v1/ground-station/relay-proxy/{peer_device_id}/*path`**
//!
//! The ground station is paired to this drone only over WFB and has no IP
//! address for it. The aux lane's Request/Response channels carry the
//! HTTP-shaped request and response over the radio, and the proxy on this
//! ground station (in the ados-control process) bridges the HTTP call to the
//! radio egress. Ground-station profile only: the same gate as
//! `gs_relayed_status.rs`.
//!
//! ## What the route proves
//!
//! A 200 from this route means the drone decoded the request, dispatched it
//! against its own HTTP API, and returned a response with the same request id.
//! The response's HTTP status (which may be 404, 500, anything) travels inside
//! the RPC payload and is projected onto this route's response — callers see
//! the drone's actual HTTP status and body, not a wrapped envelope. A relay
//! ticket the drone refused arrives as its 401 with
//! `{"error":"E_RELAY_TICKET","reason":...}`, relayed verbatim.
//!
//! ## Refused here, before the radio
//!
//! - A peer that is not in the fleet registry: 404 `E_PEER_NOT_LINKED`.
//! - A drone this ground station holds no relay secret for, or one the
//!   reconciler found holding another station's secret: 401 `E_RELAY_TICKET`
//!   with reason `no_secret` or `secret_conflict`. Neither can succeed over
//!   the radio, and answering here says why instead of spending airtime on a
//!   refusal.
//! - A body past the relay's request ceiling: 413.
//!
//! ## Bounded failure
//!
//! The proxy's per-call timeout is 10 seconds. A timeout returns a 504 Gateway
//! Timeout to the HTTP caller, so an operator sees the wedge rather than a
//! hung request. A response that arrived only in part — the radio dropped a
//! fragment — is also a 504, but says so: "the drone never answered" and "the
//! radio dropped a fragment" are different faults with different operator
//! actions. An egress failure (radio pair not open) returns a 502 Bad Gateway.

use axum::body::{Body, Bytes};
use axum::extract::{Path, RawQuery, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;

use ados_protocol::aux_rpc::{RequestParts, RpcMethod, MAX_REQUEST_BODY};
use ados_protocol::relay_ticket::{refusal_body, RelayTicketError, SECRET_CONFLICT_REASON};

use crate::routes::gs_fleet_slot::{relay_secret_state, RelaySecretState};
use crate::state::AppState;

/// Handle a relay-proxy request. The path captures `peer_device_id` (the
/// linked drone's device id) and `path` (the rest of the URL, starting with
/// a `/`). The HTTP method, content type and body are taken from the incoming
/// request.
pub async fn handle(
    State(state): State<AppState>,
    Path((peer_device_id, _decoded_path)): Path<(String, String)>,
    method: axum::http::Method,
    uri: axum::http::Uri,
    RawQuery(raw_query): RawQuery,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if !is_ground_station(&state) {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({
                "detail": {"error": {"code": "E_PROFILE_MISMATCH"}}
            })),
        )
            .into_response();
    }

    let Some(proxy) = &state.aux_rpc_proxy else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({
                "detail": "relay-proxy not initialised on this node"
            })),
        )
            .into_response();
    };

    let Some(rpc_method) = RpcMethod::from_http_method(method.as_str()) else {
        return (
            StatusCode::METHOD_NOT_ALLOWED,
            Json(serde_json::json!({
                "detail": format!("method {} not supported by relay-proxy", method)
            })),
        )
            .into_response();
    };

    // Build the full path the drone's HTTP API will see, from the RAW request
    // path: the wildcard capture is percent-decoded, and a decoded space, `?`,
    // `#` or `/` would change or break the request line the drone builds. The
    // query string travels too — `/api/logs?limit=5` reaching the drone as
    // `/api/logs` would silently serve endpoint defaults instead of what the
    // caller asked for.
    let Some(path) = raw_forward_path(uri.path()) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"detail": "relay path is missing"})),
        )
            .into_response();
    };
    let full_path = match raw_query.as_deref().filter(|q| !q.is_empty()) {
        Some(q) => format!("{path}?{q}"),
        None => path.to_string(),
    };

    if !path_is_safe(&full_path) {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "detail": "relay path contains control characters or spaces"
            })),
        )
            .into_response();
    }

    if body.len() > MAX_REQUEST_BODY {
        return (
            StatusCode::PAYLOAD_TOO_LARGE,
            Json(serde_json::json!({
                "detail": format!("request body exceeds the {MAX_REQUEST_BODY}-byte relay limit")
            })),
        )
            .into_response();
    }

    let content_type = headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    let slots = crate::routes::gs_fleet_slot::registered_slots();
    match forward(
        proxy,
        &slots,
        &peer_device_id,
        rpc_method,
        full_path.as_bytes(),
        content_type.as_bytes(),
        &body,
    )
    .await
    {
        Ok(resp) => relayed_response(resp),
        Err(Refusal::NotLinked) => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({
                "detail": {"error": {
                    "code": "E_PEER_NOT_LINKED",
                    "message": format!("{peer_device_id} is not a drone linked to this ground station"),
                }}
            })),
        )
            .into_response(),
        Err(Refusal::Ticket(reason)) => ticket_refusal(reason),
        Err(Refusal::Rpc(e)) => {
            // Without this the lane's only witness is the one HTTP caller that
            // happened to be waiting; a failing radio left no trace anywhere.
            tracing::warn!(
                error = %e,
                peer = %peer_device_id,
                path = %full_path,
                "relay_proxy_call_failed"
            );
            let (status, msg) = match e {
                ados_protocol::aux_rpc_proxy::RpcError::TooLarge => (
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "request exceeds the relay limit".to_string(),
                ),
                ados_protocol::aux_rpc_proxy::RpcError::Encode => (
                    StatusCode::BAD_REQUEST,
                    "request path, content type or target is too long to relay".to_string(),
                ),
                ados_protocol::aux_rpc_proxy::RpcError::Send(_) => (
                    StatusCode::BAD_GATEWAY,
                    "aux egress failed to send request".to_string(),
                ),
                ados_protocol::aux_rpc_proxy::RpcError::Timeout => (
                    StatusCode::GATEWAY_TIMEOUT,
                    "no response from the linked drone within the bound".to_string(),
                ),
                ados_protocol::aux_rpc_proxy::RpcError::Incomplete { received, total } => (
                    StatusCode::GATEWAY_TIMEOUT,
                    format!("relay response incomplete: {received}/{total} fragments"),
                ),
                ados_protocol::aux_rpc_proxy::RpcError::ChannelClosed => (
                    StatusCode::SERVICE_UNAVAILABLE,
                    "relay-proxy channel closed mid-call".to_string(),
                ),
                ados_protocol::aux_rpc_proxy::RpcError::Busy => (
                    StatusCode::SERVICE_UNAVAILABLE,
                    "relay-proxy saturated; retry shortly".to_string(),
                ),
            };
            (status, Json(serde_json::json!({ "detail": msg }))).into_response()
        }
    }
}

/// Why a relayed call never reached the drone, or failed on the way.
#[derive(Debug)]
enum Refusal {
    /// The peer is not in the fleet registry.
    NotLinked,
    /// No ticket can be minted that the drone would accept.
    Ticket(&'static str),
    /// The radio round trip failed.
    Rpc(ados_protocol::aux_rpc_proxy::RpcError),
}

/// A 401 in the same shape the drone uses for its own ticket refusals, so the
/// caller handles both identically.
fn ticket_refusal(reason: &str) -> Response {
    (
        StatusCode::UNAUTHORIZED,
        [(axum::http::header::CONTENT_TYPE, "application/json")],
        refusal_body(reason),
    )
        .into_response()
}

/// Send one relayed request to `peer`, carrying a relay ticket bound to it,
/// minted from the registry snapshot `slots`.
///
/// The drone authorizes every relayed request before it reaches its own API,
/// and one without a ticket bound to it is answered 401 — so the ticket is
/// what makes this route reach a drone at all.
async fn forward(
    proxy: &ados_protocol::aux_rpc_proxy::AuxRpcProxy,
    slots: &[ados_groundlink::FleetSlot],
    peer: &str,
    method: RpcMethod,
    path: &[u8],
    content_type: &[u8],
    body: &[u8],
) -> Result<ados_protocol::aux_rpc_proxy::RpcResponseOwned, Refusal> {
    if !slots.iter().any(|s| s.device_id == peer) {
        return Err(Refusal::NotLinked);
    }
    if relay_secret_state(peer) == RelaySecretState::Conflict {
        return Err(Refusal::Ticket(SECRET_CONFLICT_REASON));
    }
    let ticket = crate::routes::gs_fleet_slot::mint_ticket(slots, peer, method, path, body)
        .ok_or(Refusal::Ticket(RelayTicketError::NoSecret.reason()))?;
    proxy
        .call_request(
            peer.as_bytes(),
            method,
            &RequestParts {
                path,
                content_type,
                ticket: ticket.as_bytes(),
                body,
            },
        )
        .await
        .map_err(Refusal::Rpc)
}

/// Project a completed relay call onto the HTTP response this route returns:
/// the drone's status, its body, and the headers that crossed with it.
///
/// The headers are the point. The route used to return a bare
/// `(StatusCode, Body)` tuple, which sets no headers at all, so every relayed
/// response arrived untyped — a log artifact, a CSV export, a `text/plain`
/// error and the WHEP `Location` that routes session teardown back through the
/// proxy were each delivered as a body a strict consumer refuses or a browser
/// renders as a download. The drone allow-lists what it sends
/// (`aux_rpc_handler::FORWARDED_RESPONSE_HEADERS`), so this replays that set
/// verbatim and adds nothing of its own.
fn relayed_response(resp: ados_protocol::aux_rpc_proxy::RpcResponseOwned) -> Response {
    let status = StatusCode::from_u16(resp.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let mut builder = Response::builder().status(status);
    for (name, value) in &resp.headers {
        match (
            axum::http::HeaderName::from_bytes(name.as_bytes()),
            axum::http::HeaderValue::from_str(value),
        ) {
            (Ok(name), Ok(value)) => builder = builder.header(name, value),
            // A name or value the radio damaged into something the HTTP layer
            // refuses. Drop that one header and keep the body: a missing
            // content-type degrades, a failed response does not.
            _ => tracing::warn!(
                header = %name,
                "relay_proxy_dropped_malformed_response_header"
            ),
        }
    }
    builder
        .body(Body::from(resp.body))
        .unwrap_or_else(|_| (status, Body::empty()).into_response())
}

fn is_ground_station(state: &AppState) -> bool {
    let cfg = crate::config::PairingConfig::load_from(&state.pairing_paths.config);
    let (profile, _role) = crate::profile::current_profile_and_role_at(
        &cfg.agent.profile,
        &state.pairing_paths.profile_conf,
        &state.pairing_paths.mesh_role,
    );
    profile == "ground-station"
}

/// The still-encoded path to forward: everything after the peer segment of the
/// raw request path, starting with its `/`. `None` when there is nothing after
/// the peer.
fn raw_forward_path(uri_path: &str) -> Option<&str> {
    let rest = uri_path.strip_prefix("/api/v1/ground-station/relay-proxy/")?;
    let slash = rest.find('/')?;
    Some(&rest[slash..]).filter(|p| p.len() > 1)
}

/// Whether a relay path may be forwarded.
///
/// The path is forwarded still percent-encoded, so a raw control character or
/// space here came from a client that sent one unencoded. The drone
/// interpolates this path straight into the request line of the HTTP/1.1
/// request it makes against its OWN API — a call that arrives over genuine
/// loopback and is therefore fully on-box trusted — so an unfiltered control
/// character is header injection into a trusted context, and a space splits
/// the request line. The drone rejects both independently; neither end may
/// trust the other's validation.
fn path_is_safe(path: &str) -> bool {
    !path.bytes().any(|b| b <= 0x20 || b == 0x7F)
}

#[cfg(test)]
mod tests;
