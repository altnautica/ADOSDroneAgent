//! Origin gate for mutating requests on the setup REST surface.
//!
//! When the agent binds the HTTP API to a non-loopback address (the
//! common operator path is `0.0.0.0` so a tablet can reach the wizard
//! over the LAN) the setup surface becomes accessible to any browser on
//! the same network. A user on that LAN visiting a malicious page can
//! cross-origin POST `/api/v1/setup/profile`, `/api/v1/setup/cloudflare/install`,
//! or `/api/v1/setup/finish` and reconfigure the agent. The same browser
//! can also open a WebSocket against `/api/v1/setup/cloudflare/logs` and
//! tail cloudflared install logs to harvest reconnaissance.
//!
//! This module ships an axum middleware that enforces a same-origin
//! policy on three classes of request:
//!
//! - POST / PUT / PATCH / DELETE under `/api/v1/setup/*` — the mutating
//!   surface that can reconfigure the agent.
//! - WebSocket upgrade requests (HTTP GET with `Upgrade: websocket`) —
//!   long-lived event streams that browsers can open without CORS
//!   preflight from any page on the LAN.
//! - The `/api/v1/diag` endpoint — a read-only diagnostic dump that
//!   surfaces reconnaissance (broker URL, identity, network counters)
//!   and is gated even though it is a plain GET.
//!
//! All three classes pass through unchanged when no `Origin` header is
//! present (curl, native HTTP clients, the wizard webapp's own no-CORS
//! fetches). Only requests that explicitly declare a foreign origin are
//! rejected. Other read methods on the setup surface (GET on
//! `/api/v1/setup/status`, etc.) and `/api/v1/health` are not gated; the
//! health probe remains the canonical unauthenticated liveness check.
//!
//! The allowlist is built once at agent startup from the configured
//! `api.bind` host + port and the device_id. An update to `agent.yaml`
//! that changes the bind address requires an agent restart to refresh
//! the allowlist, which matches the existing behavior for every other
//! bind-address-derived value in the agent.

use std::collections::HashSet;
use std::sync::Arc;

use axum::{
    extract::{Request, State},
    http::{Method, StatusCode},
    middleware::Next,
    response::{IntoResponse, Json, Response},
};
use serde_json::json;

/// Set of origin strings (`scheme://host[:port]`) that are accepted
/// on mutating requests.
#[derive(Debug, Clone)]
pub struct OriginAllowlist {
    allowed: HashSet<String>,
}

impl OriginAllowlist {
    /// Build the allowlist from the bind host + port + device_id. The
    /// device_id is used to derive the mDNS hostname variant
    /// (`http://ados-<device_id>.local:<port>`) so a wizard reached via
    /// mDNS is treated as same-origin.
    ///
    /// `bind_host` is the address the HTTP API binds to. When the agent
    /// is bound to `0.0.0.0` (the common LAN-wizard path), the allowlist
    /// is built around the loopback host plus the mDNS hostname; the
    /// caller passes whatever string was used at bind time. When the
    /// bind is a concrete IP (`192.168.1.50`), that IP is added to the
    /// allowlist.
    pub fn new(bind_host: &str, port: u16, device_id: &str) -> Self {
        let mut allowed: HashSet<String> = HashSet::new();
        let hosts: Vec<String> = if bind_host.is_empty() || bind_host == "0.0.0.0" || bind_host == "::" {
            // 0.0.0.0 / :: are bind-only sentinels; no browser is going
            // to address the agent that way. Loopback + mDNS cover the
            // realistic same-origin paths in this configuration.
            vec!["localhost".to_string(), "127.0.0.1".to_string()]
        } else {
            vec![bind_host.to_string(), "localhost".to_string(), "127.0.0.1".to_string()]
        };

        for host in &hosts {
            // Wrap raw IPv6 addresses in brackets per RFC 3986 origin
            // serialization. Hostnames + IPv4 pass through unchanged.
            let host_part = if host.contains(':') && !host.starts_with('[') {
                format!("[{}]", host)
            } else {
                host.clone()
            };
            for scheme in ["http", "https"] {
                allowed.insert(format!("{}://{}:{}", scheme, host_part, port));
                // Operators reverse-proxying on the default port for
                // the scheme (80 / 443) drop the port entirely; accept
                // both forms.
                allowed.insert(format!("{}://{}", scheme, host_part));
            }
        }

        // mDNS hostname: ados-<device_id>.local. When the wizard webapp
        // is reached via mDNS the browser sets Origin to this form; the
        // agent must accept it as same-origin.
        if !device_id.is_empty() {
            let mdns = format!("ados-{}.local", device_id);
            for scheme in ["http", "https"] {
                allowed.insert(format!("{}://{}:{}", scheme, mdns, port));
                allowed.insert(format!("{}://{}", scheme, mdns));
            }
        }

        Self { allowed }
    }

    /// Returns true when the origin string is in the allowlist. The
    /// match is case-sensitive on scheme + host (browsers always send
    /// lowercase) and includes the port form the operator's URL bar
    /// produced. No suffix matching: `evil.example.com` is not matched
    /// by an allowed `example.com` entry, and `localhost.evil.com` is
    /// not matched by `localhost`.
    pub fn contains(&self, origin: &str) -> bool {
        self.allowed.contains(origin)
    }

    #[cfg(test)]
    pub fn allowed_for_test(&self) -> Vec<String> {
        let mut v: Vec<String> = self.allowed.iter().cloned().collect();
        v.sort();
        v
    }
}

/// Returns true when the request looks like a WebSocket upgrade
/// handshake: HTTP GET carrying `Upgrade: websocket`. Browsers are the
/// only clients that initiate cross-origin WebSocket upgrades from a
/// hostile context, and they DO send `Origin` on the handshake; native
/// clients (CLI / curl) typically do not, which lets them connect for
/// debugging without forging a header.
fn is_websocket_upgrade(request: &Request) -> bool {
    if request.method() != Method::GET {
        return false;
    }
    request
        .headers()
        .get("upgrade")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.eq_ignore_ascii_case("websocket"))
        .unwrap_or(false)
}

/// axum middleware: rejects requests whose `Origin` header is outside
/// the allowlist when the request is one of:
///
/// - A mutating method (POST / PUT / PATCH / DELETE) — the original
///   defense against cross-origin reconfiguration POSTs.
/// - A WebSocket upgrade (HTTP GET with `Upgrade: websocket`) — long-
///   lived event streams that browsers can open from any page on the
///   LAN. Browsers send `Origin` on the handshake, so the gate has the
///   data it needs.
/// - A GET to `/api/v1/diag` — the diagnostic dump exposes
///   reconnaissance (broker URL, identity, network counters) and is
///   gated even though it is a read method.
///
/// Pass-through cases:
/// - All other GET / HEAD / OPTIONS requests on the setup surface —
///   `/api/v1/setup/status`, profile reads, etc. exist to be polled
///   and are public state.
/// - Missing `Origin` header on any of the above three classes — the
///   typical native-client path (curl / native HTTP / SDK probes) so a
///   monitoring agent on a neighbouring host can still reach `/diag`
///   and so a CLI can still tail cloudflared WS logs.
///
/// Reject case (HTTP 403):
/// - The request is in one of the three gated classes AND the `Origin`
///   header is present AND its value is NOT in the allowlist.
pub async fn check_origin(
    State(allowlist): State<Arc<OriginAllowlist>>,
    request: Request,
    next: Next,
) -> Response {
    let method = request.method();
    let mutating = matches!(
        method,
        &Method::POST | &Method::PUT | &Method::PATCH | &Method::DELETE
    );
    let websocket = is_websocket_upgrade(&request);
    let is_diag_get = method == Method::GET && request.uri().path() == "/api/v1/diag";

    if !(mutating || websocket || is_diag_get) {
        return next.run(request).await;
    }

    if let Some(origin) = request.headers().get("origin") {
        let s = origin.to_str().unwrap_or("");
        if !allowlist.contains(s) {
            let kind = if websocket {
                "websocket-upgrade"
            } else if is_diag_get {
                "diag-read"
            } else {
                "mutating"
            };
            tracing::warn!(
                origin = %s,
                method = %method,
                path = %request.uri().path(),
                kind = %kind,
                "rejecting cross-origin request on gated class"
            );
            return (
                StatusCode::FORBIDDEN,
                Json(json!({
                    "ok": false,
                    "error": "origin not allowed",
                    "detail": "cross-origin requests are blocked on the setup surface; use the wizard webapp on the agent's own host"
                })),
            )
                .into_response();
        }
    }

    next.run(request).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allowlist_includes_loopback_when_bind_is_unspecified() {
        let a = OriginAllowlist::new("0.0.0.0", 8080, "abc123");
        assert!(a.contains("http://localhost:8080"));
        assert!(a.contains("http://127.0.0.1:8080"));
        assert!(a.contains("https://localhost:8080"));
    }

    #[test]
    fn allowlist_includes_concrete_bind_host() {
        let a = OriginAllowlist::new("192.168.1.50", 8080, "abc123");
        assert!(a.contains("http://192.168.1.50:8080"));
        assert!(a.contains("http://localhost:8080"));
    }

    #[test]
    fn allowlist_includes_mdns_hostname() {
        let a = OriginAllowlist::new("0.0.0.0", 8080, "abc123");
        assert!(a.contains("http://ados-abc123.local:8080"));
        assert!(a.contains("https://ados-abc123.local:8080"));
        // Default-port form (no port) also accepted.
        assert!(a.contains("http://ados-abc123.local"));
    }

    #[test]
    fn allowlist_rejects_unrelated_origins() {
        let a = OriginAllowlist::new("192.168.1.50", 8080, "abc123");
        assert!(!a.contains("http://evil.example:8080"));
        assert!(!a.contains("http://192.168.1.51:8080"));
        assert!(!a.contains("http://localhost.evil.com:8080"));
    }

    #[test]
    fn empty_device_id_omits_mdns_entries() {
        let a = OriginAllowlist::new("0.0.0.0", 8080, "");
        assert!(!a.contains("http://ados-.local:8080"));
        // Loopback entries still present.
        assert!(a.contains("http://localhost:8080"));
    }

    #[test]
    fn websocket_upgrade_detected_case_insensitive() {
        // Browsers may send `WebSocket`, `websocket`, or `WEBSOCKET`
        // in the upgrade header per RFC 6455 token rules. The gate
        // must recognize all forms or it will leak the WS handshake.
        for variant in ["websocket", "WebSocket", "WEBSOCKET"] {
            let request = Request::builder()
                .method(Method::GET)
                .uri("/api/v1/setup/cloudflare/logs")
                .header("upgrade", variant)
                .body(axum::body::Body::empty())
                .unwrap();
            assert!(
                is_websocket_upgrade(&request),
                "upgrade={variant} should be recognized as a websocket upgrade"
            );
        }
    }

    #[test]
    fn non_websocket_upgrade_not_flagged() {
        // A plain GET with no upgrade header is a regular read; a GET
        // with `Upgrade: h2c` is an HTTP/2-cleartext upgrade we don't
        // route through the WS handler. Neither should trip the gate.
        let plain_get = Request::builder()
            .method(Method::GET)
            .uri("/api/v1/setup/status")
            .body(axum::body::Body::empty())
            .unwrap();
        assert!(!is_websocket_upgrade(&plain_get));

        let h2c_upgrade = Request::builder()
            .method(Method::GET)
            .uri("/api/v1/setup/status")
            .header("upgrade", "h2c")
            .body(axum::body::Body::empty())
            .unwrap();
        assert!(!is_websocket_upgrade(&h2c_upgrade));

        // A POST with the websocket upgrade header set (which would be
        // a malformed handshake) is also not treated as a WS upgrade —
        // the WS class is GET-only.
        let post_with_upgrade = Request::builder()
            .method(Method::POST)
            .uri("/api/v1/setup/cloudflare/logs")
            .header("upgrade", "websocket")
            .body(axum::body::Body::empty())
            .unwrap();
        assert!(!is_websocket_upgrade(&post_with_upgrade));
    }
}
