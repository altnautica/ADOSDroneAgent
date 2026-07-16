//! The plugin MCP-tool invocation route:
//! `POST /api/plugins/{plugin_id}/tools/{tool}/invoke`.
//!
//! An MCP client (the `ADOS-MCP` connector) invokes a plugin's declared tool
//! through this native route. It forwards `{arguments, timeout_ms?}` to the
//! plugin host's on-box control socket via [`PluginControlClient::tool_invoke`],
//! which reaches the plugin's LIVE connection, runs the tool, and returns its
//! result. The connector is the authoritative gate on each tool's declared
//! safety class (a `flight_action` tool needs the flight scope); the plugin host
//! additionally gates the send on the plugin's own token carrying `mcp.expose`,
//! and the tool's effect is bounded by the plugin's other granted capabilities.
//!
//! RUST-FIRST: like the plugin-config write, a tool invocation is a control-plane
//! operation, so it is a native `ados-control` route, not a residual-Python one.
//!
//! Auth: a native write route — the LAN edge requires the pairing key when paired
//! (or a scoped MCP token covering the route's class); an unreachable daemon is a
//! 503, never a silent drop.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::json;

use crate::ipc::plugin_control_client::PluginControlError;
use crate::ipc::PluginControlClient;
use crate::routes::detail;
use crate::state::AppState;

/// The `POST /api/plugins/{plugin_id}/tools/{tool}/invoke` body. Both fields are
/// optional: `arguments` defaults to an empty object, `timeout_ms` to the plugin
/// host's default.
#[derive(Debug, Default, Deserialize)]
pub struct InvokeToolBody {
    /// The tool's argument value (any JSON scalar/array/object). Absent → `{}`.
    #[serde(default)]
    pub arguments: Option<rmpv::Value>,
    /// Bound on the wait in milliseconds. Absent → the daemon default.
    #[serde(default)]
    pub timeout_ms: Option<u64>,
}

/// Map a daemon error string to the closest HTTP status. The daemon returns an
/// envelope `error` (surfaced as `Rpc`) for a not-connected plugin, an ungranted
/// capability, an unknown tool, a timeout, or a handler error; each maps to the
/// status a client expects, and the message carries the detail.
fn status_for_rpc(msg: &str) -> StatusCode {
    if msg.starts_with("plugin_not_running") || msg.starts_with("plugin_disconnected") {
        StatusCode::NOT_FOUND
    } else if msg.starts_with("capability_denied") {
        StatusCode::FORBIDDEN
    } else if msg.starts_with("tool_not_found") {
        StatusCode::NOT_FOUND
    } else if msg.starts_with("tool_timeout") {
        StatusCode::GATEWAY_TIMEOUT
    } else {
        StatusCode::BAD_REQUEST
    }
}

/// `POST /api/plugins/{plugin_id}/tools/{tool}/invoke` — run a plugin's MCP tool
/// and return its result.
pub async fn invoke_plugin_tool(
    State(_state): State<AppState>,
    Path((plugin_id, tool)): Path<(String, String)>,
    body: Option<Json<InvokeToolBody>>,
) -> Response {
    if plugin_id.trim().is_empty() || tool.trim().is_empty() {
        return detail(
            StatusCode::BAD_REQUEST,
            "plugin_id and tool must be non-empty",
        );
    }
    let Json(body) = body.unwrap_or_default();
    let arguments = body.arguments.unwrap_or(rmpv::Value::Map(vec![]));
    let client = PluginControlClient::default_socket();
    match client
        .tool_invoke(&plugin_id, &tool, arguments, body.timeout_ms)
        .await
    {
        Ok(result) => {
            // Round-trip the msgpack result back out as JSON. A non-serializable
            // result degrades to null rather than failing the route.
            let value: serde_json::Value = serde_json::to_value(&result).unwrap_or(json!(null));
            (
                StatusCode::OK,
                Json(json!({
                    "plugin_id": plugin_id,
                    "tool": tool,
                    "result": value,
                })),
            )
                .into_response()
        }
        Err(PluginControlError::Rpc(msg)) => detail(status_for_rpc(&msg), msg),
        Err(e) => detail(
            StatusCode::SERVICE_UNAVAILABLE,
            format!("plugin host unavailable: {e}"),
        ),
    }
}
