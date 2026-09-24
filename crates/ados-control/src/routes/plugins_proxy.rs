//! `/api/plugins/{plugin_id}/x/{*rest}`: a plugin's own HTTP API, reached
//! through the agent.
//!
//! A plugin whose manifest sets `agent.http: true` serves HTTP on
//! `<run dir>/plugin-http/<id>/http.sock`. This route forwards any method, and
//! a WebSocket upgrade, to that socket with the path rewritten to `/<rest>` and
//! the query kept, streaming both bodies. The caller passed the front's normal
//! pairing-key auth (or, for a browser's WebSocket, a ticket scoped
//! `plugins.http:<plugin_id>` checked at the edge against this path), so the
//! plugin treats every request on its socket as operator-authenticated.
//!
//! The operator's credentials stop here: the pairing key, dashboard session,
//! MCP token and the ticket are stripped before the request reaches plugin
//! code. When the browser offered the ticket subprotocol and the plugin selects
//! none, the `101` names `ados-ws-ticket`, since a browser fails a handshake
//! that ignores the subprotocols it offered.

use axum::extract::{Path as AxumPath, Request, State};
use axum::http::{HeaderValue, StatusCode, Uri};
use axum::response::Response;

use super::plugins_lifecycle::is_plugin_id;
use crate::routes::detail;
use crate::state::AppState;

/// The subprotocol marker a browser offers its ticket under.
const WS_TICKET_SUBPROTOCOL: &str = "ados-ws-ticket";

/// Request headers that carry an operator credential and never reach a plugin.
const CREDENTIAL_HEADERS: [&str; 4] = [
    "x-ados-key",
    crate::dashboard_pin::DASHBOARD_SESSION_HEADER,
    crate::mcp::MCP_TOKEN_HEADER,
    crate::mcp::MCP_SCOPES_HEADER,
];

/// The `503` a plugin with no live HTTP socket answers.
fn not_serving(plugin_id: &str) -> Response {
    detail(
        StatusCode::SERVICE_UNAVAILABLE,
        format!("plugin {plugin_id} is not serving HTTP"),
    )
}

/// The upstream path and query: the raw request path past
/// `/api/plugins/<id>/x`, percent-encoding intact, with the query kept.
fn upstream_target(uri: &Uri) -> Option<String> {
    let path = uri.path();
    let (cut, _) = path.match_indices('/').nth(4)?;
    let rest = &path[cut..];
    Some(match uri.query() {
        Some(q) => format!("{rest}?{q}"),
        None => rest.to_string(),
    })
}

/// Drop the ticket (and its marker) from the offered subprotocols, keeping any
/// the plugin itself speaks. Returns whether a ticket was offered.
fn strip_ticket(headers: &mut http::HeaderMap) -> bool {
    let offered: Vec<String> = headers
        .get_all(http::header::SEC_WEBSOCKET_PROTOCOL)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|raw| raw.split(','))
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    let Some(pos) = offered.iter().position(|p| p == WS_TICKET_SUBPROTOCOL) else {
        return false;
    };
    let kept: Vec<&str> = offered
        .iter()
        .enumerate()
        .filter(|(i, _)| *i != pos && *i != pos + 1)
        .map(|(_, p)| p.as_str())
        .collect();
    headers.remove(http::header::SEC_WEBSOCKET_PROTOCOL);
    if let Ok(value) = HeaderValue::from_str(&kept.join(", ")) {
        if !kept.is_empty() {
            headers.insert(http::header::SEC_WEBSOCKET_PROTOCOL, value);
        }
    }
    true
}

/// Forward one request to the plugin's HTTP socket.
pub async fn plugin_http(
    State(state): State<AppState>,
    AxumPath((plugin_id, _rest)): AxumPath<(String, String)>,
    mut request: Request,
) -> Response {
    if !is_plugin_id(&plugin_id) {
        return detail(
            StatusCode::NOT_FOUND,
            format!("no plugin {plugin_id:?} on this node"),
        );
    }
    let socket = state.plugins.http_socket(&plugin_id);
    if !socket.exists() {
        return not_serving(&plugin_id);
    }
    let Some(target) = upstream_target(request.uri()) else {
        return detail(StatusCode::NOT_FOUND, "Not Found");
    };
    match target.parse::<Uri>() {
        Ok(uri) => *request.uri_mut() = uri,
        Err(_) => return detail(StatusCode::BAD_REQUEST, "malformed plugin path"),
    }
    let headers = request.headers_mut();
    for name in CREDENTIAL_HEADERS {
        headers.remove(name);
    }
    let ticket_offered = strip_ticket(headers);

    let mut response = crate::proxy::forward(&socket, request, || not_serving(&plugin_id)).await;
    if ticket_offered
        && response.status() == StatusCode::SWITCHING_PROTOCOLS
        && !response
            .headers()
            .contains_key(http::header::SEC_WEBSOCKET_PROTOCOL)
    {
        response.headers_mut().insert(
            http::header::SEC_WEBSOCKET_PROTOCOL,
            HeaderValue::from_static(WS_TICKET_SUBPROTOCOL),
        );
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_upstream_target_keeps_encoding_and_query() {
        let uri: Uri = "/api/plugins/com.example.web/x/atlas/a%20b/c?x=1&y=2"
            .parse()
            .unwrap();
        assert_eq!(
            upstream_target(&uri).as_deref(),
            Some("/atlas/a%20b/c?x=1&y=2")
        );
        let bare: Uri = "/api/plugins/com.example.web/x/status".parse().unwrap();
        assert_eq!(upstream_target(&bare).as_deref(), Some("/status"));
    }

    #[test]
    fn the_ticket_is_stripped_and_other_subprotocols_survive() {
        let mut h = http::HeaderMap::new();
        h.insert(
            http::header::SEC_WEBSOCKET_PROTOCOL,
            HeaderValue::from_static("ados-ws-ticket, v1|plugins.http:com.x.y|1|2|ff, rerun"),
        );
        assert!(strip_ticket(&mut h));
        assert_eq!(
            h.get(http::header::SEC_WEBSOCKET_PROTOCOL).unwrap(),
            "rerun"
        );
        let mut only = http::HeaderMap::new();
        only.insert(
            http::header::SEC_WEBSOCKET_PROTOCOL,
            HeaderValue::from_static("ados-ws-ticket, v1|s|1|2|ff"),
        );
        assert!(strip_ticket(&mut only));
        assert!(!only.contains_key(http::header::SEC_WEBSOCKET_PROTOCOL));
    }
}
