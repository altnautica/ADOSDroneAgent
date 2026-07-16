//! Loopback dispatch for cloud commands whose work lives in the local API.
//!
//! A subset of cloud commands act on or read from surfaces owned by the local
//! API process on `127.0.0.1:8080` (systemd service control, peripheral scan,
//! fleet reads, WFB pair). The cloud relay process does not own that state, so
//! it forwards the command to the existing local route and returns the route's
//! real result as the ACK — the relay is a thin authenticated forwarder, the
//! work stays where its data already lives.
//!
//! [`route_for`] is a pure name+args -> request mapping (unit-tested without a
//! socket). [`forward`] performs the loopback request and turns the route's JSON
//! into a [`CommandResult`]. A command with no mapped route returns `None` from
//! [`route_for`]; the caller acks an honest `failed("not implemented: …")`
//! rather than fabricating success.

use super::CommandResult;

/// The local API base. The same loopback target the heartbeat enrichment and
/// log-push paths use. Overridable for tests via [`forward_to`].
pub const LOCAL_API_BASE: &str = "http://127.0.0.1:8080";

/// HTTP method for a loopback route. Only the two verbs the local routes use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Get,
    Post,
}

/// A resolved loopback request: the verb, the path under the API base, optional
/// query pairs, and an optional JSON body to POST.
#[derive(Debug, Clone, PartialEq)]
pub struct LoopbackRoute {
    pub method: Method,
    pub path: String,
    pub query: Vec<(String, String)>,
    pub body: Option<serde_json::Value>,
}

impl LoopbackRoute {
    fn get(path: impl Into<String>) -> Self {
        LoopbackRoute {
            method: Method::Get,
            path: path.into(),
            query: Vec::new(),
            body: None,
        }
    }
    fn post(path: impl Into<String>) -> Self {
        LoopbackRoute {
            method: Method::Post,
            path: path.into(),
            query: Vec::new(),
            body: None,
        }
    }
    fn with_query(mut self, query: Vec<(String, String)>) -> Self {
        self.query = query;
        self
    }
    fn with_body(mut self, body: serde_json::Value) -> Self {
        self.body = Some(body);
        self
    }
}

/// A path segment safe to interpolate into a route (`{name}` style). Keeps the
/// alphanumerics, dash and dot of a unit name and drops anything else so a
/// crafted arg cannot escape the path. The local route revalidates the name
/// against its own allowlist regardless; this is defense-in-depth.
fn sanitize_segment(raw: &str) -> String {
    raw.chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '.' || *c == '_')
        .collect()
}

/// Map a cloud command name + args to the local API route that does the work.
/// Returns `None` for any command without a loopback mapping (the caller acks an
/// honest failure).
///
/// The plugin lifecycle commands are NOT mapped here — they run in-process
/// against the held `PluginSupervisor` via [`super::install`] /
/// [`super::plugin_commands`].
pub fn route_for(name: &str, args: &serde_json::Value) -> Option<LoopbackRoute> {
    match name {
        // ── Service control ────────────────────────────────────────
        "restart_service" => {
            let svc = args.get("name").and_then(|v| v.as_str()).unwrap_or("");
            // An empty / missing name has no valid route; let the caller fail it
            // honestly rather than POST to `/services//restart`.
            if svc.is_empty() {
                return None;
            }
            Some(LoopbackRoute::post(format!(
                "/api/services/{}/restart",
                sanitize_segment(svc)
            )))
        }
        "get_services" => Some(LoopbackRoute::get("/api/services")),

        // ── Peripherals ────────────────────────────────────────────
        "scan_peripherals" => Some(LoopbackRoute::post("/api/peripherals/scan")),
        "get_peripherals" => Some(LoopbackRoute::get("/api/peripherals")),

        // ── Logs ───────────────────────────────────────────────────
        "get_logs" => {
            let mut q = Vec::new();
            if let Some(level) = args.get("level").and_then(|v| v.as_str()) {
                if !level.is_empty() {
                    q.push(("level".to_string(), level.to_string()));
                }
            }
            if let Some(limit) = args.get("limit").and_then(|v| v.as_u64()) {
                q.push(("limit".to_string(), limit.to_string()));
            }
            Some(LoopbackRoute::get("/api/logs").with_query(q))
        }

        // ── Fleet reads ────────────────────────────────────────────
        "get_peers" => Some(LoopbackRoute::get("/api/fleet/peers")),
        "get_enrollment" => Some(LoopbackRoute::get("/api/fleet/enrollment")),

        // ── WFB pair ───────────────────────────────────────────────
        // The remote-pair commands drive the same local-bind / unpair routes the
        // LAN-paired GCS uses; the relay just forwards them.
        "wfb_pair_init_remote" | "wfb_pair_apply_remote" => {
            let mut body = serde_json::Map::new();
            if let Some(role) = args.get("role").and_then(|v| v.as_str()) {
                body.insert("role".to_string(), serde_json::json!(role));
            }
            if let Some(peer) = args.get("peer_device_id").and_then(|v| v.as_str()) {
                body.insert("peer_device_id".to_string(), serde_json::json!(peer));
            }
            Some(
                LoopbackRoute::post("/api/wfb/pair/local-bind")
                    .with_body(serde_json::Value::Object(body)),
            )
        }
        "wfb_pair_unpair" => Some(LoopbackRoute::post("/api/wfb/pair/unpair")),

        // ── Raw FC command passthrough ─────────────────────────────
        // send_command carries { cmd, args } — the exact CommandRequest body
        // /api/command deserializes. Forward it verbatim; extracting only the inner
        // `args` array would POST a bare list and fail deserialization.
        "send_command" => Some(LoopbackRoute::post("/api/command").with_body(args.clone())),

        _ => None,
    }
}

/// Forward a mapped command to the local API and return the route's result as a
/// [`CommandResult`]. Uses the default local API base.
pub async fn forward(
    http: &reqwest::Client,
    name: &str,
    args: &serde_json::Value,
    route: &LoopbackRoute,
) -> CommandResult {
    forward_to(http, LOCAL_API_BASE, name, args, route).await
}

/// Forward against an explicit base URL (tests point this at a local mock).
pub async fn forward_to(
    http: &reqwest::Client,
    base: &str,
    name: &str,
    _args: &serde_json::Value,
    route: &LoopbackRoute,
) -> CommandResult {
    let url = format!("{}{}", base.trim_end_matches('/'), route.path);
    let req = match route.method {
        Method::Get => http.get(&url).query(&route.query),
        Method::Post => {
            let r = http.post(&url).query(&route.query);
            match &route.body {
                Some(b) => r.json(b),
                // FastAPI routes that take a body model still accept an empty
                // object; the no-body routes ignore it.
                None => r.json(&serde_json::json!({})),
            }
        }
    };

    let resp = match req.send().await {
        Ok(r) => r,
        Err(e) => {
            return CommandResult::failed(format!("local api unreachable: {e}"));
        }
    };
    let status = resp.status();
    let payload: serde_json::Value = match resp.json().await {
        Ok(v) => v,
        Err(_) => {
            if status.is_success() {
                // A route that returns a non-JSON 2xx is still a success; carry
                // an empty doc so the ACK is honest about there being no data.
                return CommandResult::completed(format!("{name} ok"));
            }
            return CommandResult::failed(format!("local api error: HTTP {}", status.as_u16()));
        }
    };

    interpret(name, status.as_u16(), payload)
}

/// Turn a local-route HTTP status + JSON body into a [`CommandResult`].
///
/// The honesty rule: a body that signals failure acks `failed`, not `completed`.
/// The local routes use two failure conventions: an HTTP error status, and a
/// 200 body with `{"status": "error", ...}` (the service-restart route returns
/// the latter for an unknown unit and for the polkit-no-op case). Both map to
/// `failed`. The route's larger payload rides in `data`.
pub fn interpret(name: &str, http_status: u16, payload: serde_json::Value) -> CommandResult {
    // Body-level error convention (`{"status": "error", "message": …}`), used by
    // the service-restart route even on a 200, so a rejected restart acks failed.
    let body_status = payload.get("status").and_then(|v| v.as_str());
    let body_message = payload
        .get("message")
        .and_then(|v| v.as_str())
        .map(str::to_string);

    let http_ok = (200..300).contains(&http_status);

    let failed = body_status == Some("error") || !http_ok;
    if failed {
        let msg = body_message
            .or_else(|| {
                // FastAPI raises wrap the error under `detail`.
                payload
                    .get("detail")
                    .map(|d| d.to_string())
                    .filter(|s| s != "null")
            })
            .unwrap_or_else(|| format!("{name} failed: HTTP {http_status}"));
        return CommandResult::failed(msg).with_data(payload);
    }

    let msg = body_message.unwrap_or_else(|| format!("{name} ok"));
    CommandResult::completed(msg).with_data(payload)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dispatch::CommandStatus;

    #[test]
    fn send_command_forwards_the_full_cmd_args_body() {
        // The relay enqueues send_command as { cmd, args } — the exact CommandRequest
        // body. It must reach /api/command verbatim; forwarding only the inner args
        // array would POST a bare list and fail deserialization.
        let r = route_for(
            "send_command",
            &serde_json::json!({"cmd": "arm", "args": []}),
        )
        .unwrap();
        assert_eq!(r.method, Method::Post);
        assert_eq!(r.path, "/api/command");
        assert_eq!(r.body, Some(serde_json::json!({"cmd": "arm", "args": []})));

        let t = route_for(
            "send_command",
            &serde_json::json!({"cmd": "takeoff", "args": [25]}),
        )
        .unwrap();
        assert_eq!(
            t.body,
            Some(serde_json::json!({"cmd": "takeoff", "args": [25]}))
        );
    }

    #[test]
    fn restart_service_maps_to_named_route() {
        let r = route_for(
            "restart_service",
            &serde_json::json!({"name": "ados-video"}),
        )
        .unwrap();
        assert_eq!(r.method, Method::Post);
        assert_eq!(r.path, "/api/services/ados-video/restart");
    }

    #[test]
    fn restart_service_sanitizes_the_unit_segment() {
        // A path-traversal attempt is stripped to safe characters; the local
        // route's own allowlist is the real gate, this is defense-in-depth.
        let r = route_for(
            "restart_service",
            &serde_json::json!({"name": "../../etc/shadow"}),
        )
        .unwrap();
        assert_eq!(r.path, "/api/services/....etcshadow/restart");
    }

    #[test]
    fn restart_service_without_name_has_no_route() {
        assert!(route_for("restart_service", &serde_json::json!({})).is_none());
        assert!(route_for("restart_service", &serde_json::json!({"name": ""})).is_none());
    }

    #[test]
    fn scan_and_get_peripherals_map() {
        let scan = route_for("scan_peripherals", &serde_json::Value::Null).unwrap();
        assert_eq!(scan.method, Method::Post);
        assert_eq!(scan.path, "/api/peripherals/scan");
        let get = route_for("get_peripherals", &serde_json::Value::Null).unwrap();
        assert_eq!(get.method, Method::Get);
        assert_eq!(get.path, "/api/peripherals");
    }

    #[test]
    fn get_logs_carries_level_and_limit_query() {
        let r = route_for(
            "get_logs",
            &serde_json::json!({"level": "WARN", "limit": 200}),
        )
        .unwrap();
        assert_eq!(r.path, "/api/logs");
        assert!(r.query.contains(&("level".to_string(), "WARN".to_string())));
        assert!(r.query.contains(&("limit".to_string(), "200".to_string())));
    }

    #[test]
    fn fleet_reads_map() {
        assert_eq!(
            route_for("get_peers", &serde_json::Value::Null)
                .unwrap()
                .path,
            "/api/fleet/peers"
        );
        assert_eq!(
            route_for("get_enrollment", &serde_json::Value::Null)
                .unwrap()
                .path,
            "/api/fleet/enrollment"
        );
    }

    #[test]
    fn unmapped_command_returns_none() {
        assert!(route_for("totally_unknown", &serde_json::Value::Null).is_none());
        // Plugin commands are handled in-process, not via loopback.
        assert!(route_for("plugin.enable", &serde_json::json!({"pluginId": "p"})).is_none());
    }

    #[test]
    fn interpret_ok_body_is_completed() {
        let r = interpret(
            "restart_service",
            200,
            serde_json::json!({"status": "ok", "message": "Restarted ados-video"}),
        );
        assert_eq!(r.status, CommandStatus::Completed);
        assert_eq!(r.result["message"], "Restarted ados-video");
        assert_eq!(r.data.unwrap()["status"], "ok");
    }

    #[test]
    fn interpret_error_body_on_200_is_failed() {
        // The service-restart route returns `{"status":"error"}` with HTTP 200
        // for an unknown / rejected unit. That must ack failed, never completed.
        let r = interpret(
            "restart_service",
            200,
            serde_json::json!({"status": "error", "message": "Unknown service: bogus"}),
        );
        assert_eq!(r.status, CommandStatus::Failed);
        assert_eq!(r.result["message"], "Unknown service: bogus");
    }

    #[test]
    fn interpret_http_error_is_failed() {
        let r = interpret("get_logs", 503, serde_json::json!({"detail": "store down"}));
        assert_eq!(r.status, CommandStatus::Failed);
    }

    #[test]
    fn interpret_array_body_is_completed() {
        // `/api/fleet/peers` returns a bare array; no `status` field, HTTP 200.
        let r = interpret("get_peers", 200, serde_json::json!([]));
        assert_eq!(r.status, CommandStatus::Completed);
        assert!(r.data.unwrap().is_array());
    }
}
