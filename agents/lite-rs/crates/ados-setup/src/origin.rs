//! Origin gate for mutating requests on the setup REST surface.
//!
//! When the agent binds the HTTP API to a non-loopback address (the
//! common operator path is `0.0.0.0` so a tablet can reach the wizard
//! over the LAN) the setup surface becomes accessible to any browser on
//! the same network. A user on that LAN visiting a malicious page can
//! cross-origin POST `/api/v1/setup/profile`, `/api/v1/setup/cloudflare/install`,
//! or `/api/v1/setup/finish` and reconfigure the agent.
//!
//! This module ships an axum middleware that enforces a same-origin
//! policy on POST / PUT / PATCH / DELETE requests under
//! `/api/v1/setup/*`. Read methods (GET / HEAD / OPTIONS) and requests
//! that arrive without an `Origin` header (curl / native HTTP clients /
//! the wizard webapp's own no-CORS fetches) are passed through; only
//! cross-origin POSTs that explicitly declare a foreign origin are
//! rejected.
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

/// axum middleware: rejects mutating requests whose `Origin` header is
/// outside the allowlist.
///
/// Pass-through cases:
/// - GET / HEAD / OPTIONS — read methods; no state mutation possible.
/// - Missing `Origin` header — same-host curl / native clients, plus
///   the wizard webapp's own fetches when the browser elides the header
///   on no-CORS same-origin requests.
///
/// Reject case (HTTP 403):
/// - The header is present and the value is NOT in the allowlist.
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
    if !mutating {
        return next.run(request).await;
    }

    if let Some(origin) = request.headers().get("origin") {
        let s = origin.to_str().unwrap_or("");
        if !allowlist.contains(s) {
            tracing::warn!(
                origin = %s,
                method = %method,
                path = %request.uri().path(),
                "rejecting cross-origin mutating request"
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
}
