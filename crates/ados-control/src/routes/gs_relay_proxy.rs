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
//! The proxy's per-call timeout is 5 seconds. A timeout returns a 504 Gateway
//! Timeout to the HTTP caller, so an operator sees the wedge rather than a
//! hung request. An egress failure (radio pair not open) returns a 502 Bad
//! Gateway. An encode failure (request too large for one aux frame) returns a
//! 413.

use axum::body::{Body, Bytes};
use axum::extract::{Path, State};
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
    Path((_peer_device_id, path)): Path<(String, String)>,
    method: axum::http::Method,
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
    // strips the leading slash, so re-add it: `/api/pairing/info`.
    let full_path = format!("/{}", path);

    match proxy.call(rpc_method, full_path.as_bytes(), &body).await {
        Ok(resp) => {
            let status =
                StatusCode::from_u16(resp.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
            (status, Body::from(resp.body)).into_response()
        }
        Err(e) => {
            let (status, msg) = match e {
                ados_protocol::aux_rpc_proxy::RpcError::Encode => (
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "request exceeds one aux frame",
                ),
                ados_protocol::aux_rpc_proxy::RpcError::Send(_) => {
                    (StatusCode::BAD_GATEWAY, "aux egress failed to send request")
                }
                ados_protocol::aux_rpc_proxy::RpcError::Timeout => (
                    StatusCode::GATEWAY_TIMEOUT,
                    "no response from the linked drone within the bound",
                ),
                ados_protocol::aux_rpc_proxy::RpcError::ChannelClosed => (
                    StatusCode::SERVICE_UNAVAILABLE,
                    "relay-proxy channel closed mid-call",
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
