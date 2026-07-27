//! Ground-station relay-proxy route: forward an HTTP-shaped request to a
//! WFB-linked drone the ground station has no IP reach to.
//!
//! **`{GET,POST,PUT,DELETE} /api/v1/ground-station/relay-proxy/{peer_device_id}/*path`**
//!
//! The ground station is paired to this drone only over WFB and has no IP
//! address for it. The aux lane's new Request/Response channels carry the
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
//! the drone's actual HTTP status, not a wrapped envelope.
//!
//! ## Bounded failure
//!
//! The proxy's per-call timeout is 10 seconds. A timeout returns a 504 Gateway
//! Timeout to the HTTP caller, so an operator sees the wedge rather than a
//! hung request. A response that arrived only in part — the radio dropped a
//! fragment — is also a 504, but says so: "the drone never answered" and "the
//! radio dropped a fragment" are different faults with different operator
//! actions. An egress failure (radio pair not open) returns a 502 Bad Gateway.
//! An encode failure (request too large for one aux frame) returns a 413.

use axum::body::{Body, Bytes};
use axum::extract::{Path, RawQuery, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;

use crate::state::AppState;

/// Handle a relay-proxy request. The path captures `peer_device_id` (the
/// linked drone's device id) and `path` (the rest of the URL, starting with
/// a `/`). The HTTP method and request body are matched from the incoming
/// request.
pub async fn handle(
    State(state): State<AppState>,
    Path((peer_device_id, path)): Path<(String, String)>,
    method: axum::http::Method,
    RawQuery(raw_query): RawQuery,
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

    let rpc_method = match method.as_str() {
        "GET" => ados_protocol::aux_rpc::RpcMethod::Get,
        "POST" => ados_protocol::aux_rpc::RpcMethod::Post,
        "PUT" => ados_protocol::aux_rpc::RpcMethod::Put,
        "DELETE" => ados_protocol::aux_rpc::RpcMethod::Delete,
        _ => {
            return (
                StatusCode::METHOD_NOT_ALLOWED,
                Json(serde_json::json!({
                    "detail": format!("method {} not supported by relay-proxy", method)
                })),
            )
                .into_response();
        }
    };

    // Build the full path the drone's HTTP API will see. The wildcard capture
    // strips the leading slash, so re-add it: `/api/pairing/info`. The query
    // string travels too — `/api/logs?limit=5` reaching the drone as
    // `/api/logs` would silently serve endpoint defaults instead of what the
    // caller asked for.
    let full_path = match raw_query.as_deref().filter(|q| !q.is_empty()) {
        Some(q) => format!("/{path}?{q}"),
        None => format!("/{path}"),
    };

    if !path_is_safe(&full_path) {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "detail": "relay path contains control characters"
            })),
        )
            .into_response();
    }

    match proxy
        .call(
            peer_device_id.as_bytes(),
            rpc_method,
            full_path.as_bytes(),
            &body,
        )
        .await
    {
        Ok(resp) => {
            let status =
                StatusCode::from_u16(resp.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
            (status, Body::from(resp.body)).into_response()
        }
        Err(e) => {
            // Without this the lane's only witness is the one HTTP caller that
            // happened to be waiting; a failing radio left no trace anywhere.
            tracing::warn!(
                error = %e,
                peer = %peer_device_id,
                path = %full_path,
                "relay_proxy_call_failed"
            );
            let (status, msg) = match e {
                ados_protocol::aux_rpc_proxy::RpcError::Encode => (
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "request exceeds one aux frame".to_string(),
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

fn is_ground_station(state: &AppState) -> bool {
    let cfg = crate::config::PairingConfig::load_from(&state.pairing_paths.config);
    let (profile, _role) = crate::profile::current_profile_and_role_at(
        &cfg.agent.profile,
        &state.pairing_paths.profile_conf,
        &state.pairing_paths.mesh_role,
    );
    profile == "ground-station"
}

/// Whether a relay path may be forwarded.
///
/// axum percent-decodes the wildcard capture, so `%0D%0A` in the URL arrives as
/// literal CRLF. The drone interpolates this path straight into the request
/// line of the HTTP/1.1 request it makes against its OWN API — a call that
/// arrives over genuine loopback and is therefore fully on-box trusted — so an
/// unfiltered control character is header injection into a trusted context.
/// The drone rejects it independently; neither end may trust the other's
/// validation.
fn path_is_safe(path: &str) -> bool {
    !path.bytes().any(|b| b < 0x20 || b == 0x7F)
}

#[cfg(test)]
mod tests {
    use super::path_is_safe;

    #[test]
    fn an_ordinary_path_with_a_query_string_is_forwarded() {
        assert!(path_is_safe("/api/version"));
        assert!(path_is_safe("/api/logs?limit=5&level=warn"));
        assert!(path_is_safe("/api/status/full"));
    }

    #[test]
    fn a_decoded_crlf_is_rejected_before_it_reaches_the_radio() {
        // `%0d%0aX-Injected:%201` after percent-decoding.
        assert!(!path_is_safe("/api/version\r\nX-Injected: 1"));
        assert!(!path_is_safe("/api/version\nX-Injected: 1"));
        assert!(!path_is_safe("/api/version\rX-Injected: 1"));
    }

    #[test]
    fn other_control_characters_are_rejected_too() {
        assert!(!path_is_safe("/api/\0version"));
        assert!(!path_is_safe("/api/\tversion"));
        assert!(!path_is_safe("/api/version\x7f"));
    }
}
