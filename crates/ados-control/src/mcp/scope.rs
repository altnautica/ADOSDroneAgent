//! Route and tool scope classification for MCP tokens.

use super::*;

/// The [`ScopeClass`] a native `(method, path)` requires of an MCP token, or
/// `None` when the route is not reachable by a token (fail-closed: the caller must
/// use the full `X-ADOS-Key`). Every native `GET` is a read; writes are classified
/// by their effect, defaulting to `None` so a new/unclassified write is denied.
///
/// Public-exempt routes (the pairing handshake, `/healthz`, the dashboard-PIN
/// status/verify/set) never reach the MCP branch (the edge admits them before
/// auth), and proxied routes (config/vision/plugins under the permanent-Python
/// prefixes) are gated by the residual surface, so this map covers only the native
/// gated routes an MCP token can present against.
pub fn route_scope(method: &Method, path: &str) -> Option<ScopeClass> {
    use ScopeClass::*;
    // The ground station's relay-proxy carries a request to a linked drone,
    // which runs it with the ground station's full authority. It needs exactly
    // the class the same request would need on the drone, so a token cannot
    // reach a flight action (or a secret read) by wrapping it in the relay.
    if let Some(inner) = relay_proxy_inner_path(path) {
        return route_scope(method, inner);
    }
    // The plugin lifecycle and a plugin's own HTTP API moved here from the
    // Python surface, where no MCP token ever reached them. They stay out of an
    // MCP token's reach, reads included (an install record or a plugin's API is
    // not a telemetry read).
    if is_plugin_lifecycle(path) {
        return None;
    }
    // A GET carries no side effect on the agent, so every native read is `read` —
    // EXCEPT a GET whose response body carries a secret, which needs the
    // `secret_read` scope so a plain `read` token cannot reach it. No native GET is
    // secret-bearing today (the pairing key / WFB bind key / WS ticket are never
    // returned by a GET); a future one MUST be listed in `SECRET_GET_ROUTES` or a
    // `read` token would reach it (fail-open for that one route).
    if method == Method::GET {
        return Some(if is_secret_get(path) {
            SecretRead
        } else {
            Read
        });
    }
    match *method {
        Method::POST => match path {
            // Raw MAVLink command passthrough can command the vehicle -> flight.
            "/api/command" => Some(Flight),
            // Param set reconfigures the FC (arming/failsafe params etc.) -> admin.
            p if is_param_write(p) => Some(Admin),
            // Service restart / supervisor restart / signing writes.
            p if is_service_restart(p) => Some(Admin),
            "/api/v1/system/restart-supervisor" => Some(Admin),
            "/api/mavlink/signing/enroll-fc" | "/api/mavlink/signing/disable-on-fc" => Some(Admin),
            // WFB radio + MAC-pin writes.
            "/api/wfb/channel" | "/api/v1/network/mac/pin" => Some(Admin),
            // Vision custom-model upload + MCP self-revoke + WS-ticket mint (mints
            // a data-plane credential) are admin actions.
            "/api/vision/models/upload" | "/api/mcp/revoke" | "/api/_ws/ticket" => Some(Admin),
            // CAN passthrough injects arbitrary CAN frames to the FC / ESCs /
            // servos — a vehicle-command write on par with /api/command -> flight.
            "/api/can/passthrough" => Some(Flight),
            // Operator-facing safe writes.
            "/api/vision/designate" | "/api/logs/push" => Some(SafeWrite),
            // Unpairing tears down the trust relationship.
            "/api/pairing/unpair" => Some(Destructive),
            // RC channel injection and taking pilot-in-command command the
            // aircraft.
            "/api/v1/ground-station/crsf/channels" | "/api/v1/ground-station/pic/claim" => {
                Some(Flight)
            }
            // Ground-station control writes.
            p if p.starts_with("/api/v1/ground-station/") => Some(Admin),
            _ => None,
        },
        Method::PUT => match path {
            "/api/wfb/tx-power" | "/api/wfb/pair/auto-pair" => Some(Admin),
            "/api/vision/detector" => Some(Admin),
            // The LoRa region / channel / power write.
            "/api/v1/lora" => Some(Admin),
            p if is_plugin_config(p) => Some(Admin),
            p if p.starts_with("/api/v1/network/") => Some(Admin),
            p if p.starts_with("/api/v1/ground-station/") => Some(Admin),
            _ => None,
        },
        Method::DELETE => match path {
            "/api/vision/detector" => Some(Admin),
            p if p.starts_with("/api/v1/network/") => Some(Admin),
            p if p.starts_with("/api/v1/ground-station/") => Some(Admin),
            _ => None,
        },
        _ => None,
    }
}

/// Whether the scope a token needs for `(method, path)` depends on what the
/// route itself reads, so the edge admits any verified token and the route
/// enforces the class: a plugin tool invocation needs the scope named by the
/// tool's declared `safety_class` (see [`tool_scope_class`]), which only the
/// installed manifest knows.
pub fn route_decides_scope(method: &Method, path: &str) -> bool {
    *method == Method::POST && is_plugin_tool_invoke(path)
}

/// The scope a plugin tool's declared `safety_class` requires of an MCP token.
/// An unknown or missing class is `None`: the tool is not token-reachable.
pub fn tool_scope_class(safety_class: &str) -> Option<ScopeClass> {
    match safety_class {
        "read" => Some(ScopeClass::Read),
        "secret_read" => Some(ScopeClass::SecretRead),
        "safe_write" => Some(ScopeClass::SafeWrite),
        "admin" => Some(ScopeClass::Admin),
        "flight" | "flight_action" => Some(ScopeClass::Flight),
        "destructive" => Some(ScopeClass::Destructive),
        _ => None,
    }
}

/// The `safety_class` an installed manifest declares for `tool` in its agent
/// half's `contributes.tools`, or `None` when the tool is not declared there.
pub fn tool_safety_class(
    manifest: &ados_plugin_host::manifest::PluginManifest,
    tool: &str,
) -> Option<String> {
    let contributes = manifest.agent.as_ref()?.extra.get("contributes")?;
    let tools = serde_json::to_value(contributes.get("tools")?).ok()?;
    tools.as_array()?.iter().find_map(|t| {
        (t.get("name")?.as_str()? == tool)
            .then(|| t.get("safety_class")?.as_str().map(str::to_string))
            .flatten()
    })
}

/// The inner path of a ground-station relay-proxy route
/// (`/api/v1/ground-station/relay-proxy/{peer}/{*inner}`), starting with `/`.
pub(super) fn relay_proxy_inner_path(path: &str) -> Option<&str> {
    let rest = path.strip_prefix("/api/v1/ground-station/relay-proxy/")?;
    let slash = rest.find('/')?;
    (slash > 0 && rest.len() > slash + 1).then(|| &rest[slash..])
}

/// `POST /api/params/{name}` — a single-segment param name after `/api/params/`.
pub(super) fn is_param_write(path: &str) -> bool {
    match path.strip_prefix("/api/params/") {
        Some(rest) => !rest.is_empty() && !rest.contains('/'),
        None => false,
    }
}

/// `POST /api/services/{name}/restart`.
pub(super) fn is_service_restart(path: &str) -> bool {
    match path
        .strip_prefix("/api/services/")
        .and_then(|r| r.strip_suffix("/restart"))
    {
        Some(name) => !name.is_empty() && !name.contains('/'),
        None => false,
    }
}

/// `/api/plugins/{plugin_id}/config`: the `PUT` write and its `GET` read-back.
pub(super) fn is_plugin_config(path: &str) -> bool {
    match path
        .strip_prefix("/api/plugins/")
        .and_then(|r| r.strip_suffix("/config"))
    {
        Some(id) => !id.is_empty() && !id.contains('/'),
        None => false,
    }
}

/// `GET /api/plugins/{plugin_id}/state`: a plugin's published-state read.
pub(super) fn is_plugin_state(path: &str) -> bool {
    match path
        .strip_prefix("/api/plugins/")
        .and_then(|r| r.strip_suffix("/state"))
    {
        Some(id) => !id.is_empty() && !id.contains('/'),
        None => false,
    }
}

/// The plugin lifecycle routes (`/api/plugins`, `/api/v1/plugins/*`, and every
/// `/api/plugins/*` path other than a plugin's config, published state and MCP
/// tool invocation), including the `/x/` passthrough to a plugin's own API.
pub(super) fn is_plugin_lifecycle(path: &str) -> bool {
    if path == "/api/plugins" || path == "/api/v1/plugins" || path.starts_with("/api/v1/plugins/") {
        return true;
    }
    path.starts_with("/api/plugins/")
        && !is_plugin_config(path)
        && !is_plugin_state(path)
        && !is_plugin_tool_invoke(path)
}

/// Native `GET` routes whose response body carries a secret and therefore need the
/// `secret_read` scope rather than plain `read`, by exact path. Empty today: no
/// fixed-path native GET returns the pairing key, the WFB bind key, or a WS ticket (`/api/pairing/code` is public
/// and 409s when paired, `wfb/pair` returns only a fingerprint, `signing/counters`
/// reports unmeasured nulls). Add any future secret-bearing GET here, or a plain `read` token
/// would reach it; this is a deliberate security surface, not a convenience list.
pub(super) const SECRET_GET_ROUTES: &[&str] = &[];

/// Whether a native GET route returns a secret (needs `secret_read`, not `read`).
/// A plugin's config is whatever that plugin stores, which can be a credential,
/// so reading it back is treated as secret-bearing.
pub(super) fn is_secret_get(path: &str) -> bool {
    SECRET_GET_ROUTES.contains(&path) || is_plugin_config(path)
}

/// `POST /api/plugins/{plugin_id}/tools/{tool}/invoke` — a two-param template.
pub(super) fn is_plugin_tool_invoke(path: &str) -> bool {
    let Some(rest) = path.strip_prefix("/api/plugins/") else {
        return false;
    };
    let Some(rest) = rest.strip_suffix("/invoke") else {
        return false;
    };
    // rest is `{plugin_id}/tools/{tool}`: exactly three non-empty segments with
    // the middle literal `tools`.
    let parts: Vec<&str> = rest.split('/').collect();
    parts.len() == 3 && parts[1] == "tools" && !parts[0].is_empty() && !parts[2].is_empty()
}
