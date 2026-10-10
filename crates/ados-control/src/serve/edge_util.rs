//! Helpers for the LAN and Unix edges: request keying, the canonical request
//! target, the proxied-WebSocket scope table, the unreadable-config refusal,
//! the live CORS allow-list and the Unix-edge on-box stamp.

use std::fmt::Write as _;
use std::net::IpAddr;
use std::sync::Arc;

use axum::extract::Request;
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, Uri};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use tower_http::cors::AllowOrigin;

use crate::auth::PeerKey;
use crate::config::default_cors_origins;
use crate::security_state::{
    SecurityState, SecurityView, CONFIG_UNREADABLE_CODE, CONFIG_UNREADABLE_MESSAGE,
};

use super::ONBOX_HEADER;

/// The header cloudflared stamps with the real client address on a request it
/// tunnels in over loopback.
pub(super) const CF_CONNECTING_IP: &str = "cf-connecting-ip";

/// The key the TCP edge charges a request to, stamped on every request for the
/// rate limit and the PIN lockout. Keying only, never authorization: a
/// tunnelled caller arrives on loopback and is told apart by its forwarded
/// address, which nothing trusts for access.
#[derive(Clone, Copy, Debug)]
pub struct PeerKeyHint(pub PeerKey);

/// The keying address for a request: a loopback peer carrying a parseable
/// `cf-connecting-ip` is keyed on that address, so two tunnelled clients do not
/// share one budget; anyone else is keyed on the socket peer.
pub(super) fn keying_peer(peer_ip: Option<IpAddr>, headers: &HeaderMap) -> PeerKey {
    let forwarded = peer_ip
        .filter(|ip| ip.to_canonical().is_loopback())
        .and_then(|_| headers.get(CF_CONNECTING_IP))
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.trim().parse::<IpAddr>().ok());
    PeerKey::of(forwarded.or(peer_ip))
}

/// Methods that change state: everything but the safe reads and the preflight.
pub(super) fn is_mutating(method: &Method) -> bool {
    !matches!(*method, Method::GET | Method::HEAD | Method::OPTIONS)
}

/// The `503` an off-box mutation gets while the config has never been readable.
pub(super) fn config_unreadable() -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        axum::Json(serde_json::json!({
            "error": CONFIG_UNREADABLE_CODE,
            "message": CONFIG_UNREADABLE_MESSAGE,
        })),
    )
        .into_response()
}

/// Bytes a path segment carries unescaped (RFC 3986 `pchar` plus `/`).
fn is_path_safe(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b"-._~!$&'()*+,;=:@/".contains(&b)
}

/// `uri` with its path replaced by the canonical encoding of `decoded`, the
/// query kept as sent. `None` if the result is not a valid URI.
pub(super) fn canonical_uri(uri: &Uri, decoded: &str) -> Option<Uri> {
    let mut target = String::with_capacity(decoded.len() + 8);
    for b in decoded.bytes() {
        if is_path_safe(b) {
            target.push(char::from(b));
        } else {
            let _ = write!(target, "%{b:02X}");
        }
    }
    if let Some(q) = uri.query() {
        target.push('?');
        target.push_str(q);
    }
    let mut parts = uri.clone().into_parts();
    parts.path_and_query = Some(target.parse().ok()?);
    Uri::from_parts(parts).ok()
}

/// The residual (proxied) WebSocket routes and the ticket scope each one's
/// handler checks (`authenticate_websocket(..., scope=...)`). The edge admits a
/// proxied upgrade only on a ticket minted for the scope bound to its path.
pub(super) const PROXIED_WS_SCOPES: &[(&str, &str)] = &[
    ("/api/v1/setup/cloudflare/logs", "setup.cloudflare_logs"),
    ("/api/vision/detections/ws", "vision.detections"),
];

/// The ticket scope a proxied WebSocket `path` requires, `None` for a path
/// that is not a residual WebSocket route.
pub(super) fn proxied_ws_scope(path: &str) -> Option<&'static str> {
    PROXIED_WS_SCOPES
        .iter()
        .find(|(p, _)| *p == path)
        .map(|(_, scope)| *scope)
}

/// The CORS allow-list read per request from the live security posture, so a
/// `security.api.cors_origins*` write applies without a restart. While the
/// config is unreadable only the default origins are allowed.
pub(super) fn live_cors_origins(security: Arc<SecurityState>) -> AllowOrigin {
    AllowOrigin::predicate(move |origin: &HeaderValue, _parts| {
        let allowed = match security.current() {
            SecurityView::Known(cfg) => cfg.security.api.effective_cors_origins(),
            SecurityView::Unreadable => default_cors_origins(),
        };
        allowed.iter().any(|o| o.as_bytes() == origin.as_bytes())
    })
}

/// The Unix-edge middleware: every caller on the operator socket is on-box, so
/// any client-supplied `x-ados-onbox` is replaced with `1`. The residual
/// WebSocket auth then extends the same on-box trust the native routes do.
pub(super) async fn unix_edge(mut request: Request, next: Next) -> Response {
    request
        .headers_mut()
        .insert(ONBOX_HEADER, HeaderValue::from_static("1"));
    next.run(request).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_decoded_path_is_re_encoded_canonically() {
        let uri: Uri = "/%61pi/status?x=1".parse().unwrap();
        let out = canonical_uri(&uri, "/api/status").unwrap();
        assert_eq!(out.path(), "/api/status");
        assert_eq!(out.query(), Some("x=1"));
        let spaced = canonical_uri(&"/a%20b".parse().unwrap(), "/a b").unwrap();
        assert_eq!(spaced.path(), "/a%20b");
    }

    #[test]
    fn only_a_loopback_peer_is_keyed_on_its_forwarded_address() {
        let mut h = HeaderMap::new();
        h.insert(CF_CONNECTING_IP, "203.0.113.7".parse().unwrap());
        let loopback = Some(IpAddr::from([127, 0, 0, 1]));
        assert_eq!(
            keying_peer(loopback, &h),
            PeerKey::of(Some(IpAddr::from([203, 0, 113, 7])))
        );
        let mut h2 = HeaderMap::new();
        h2.insert(CF_CONNECTING_IP, "203.0.113.8".parse().unwrap());
        assert_ne!(keying_peer(loopback, &h), keying_peer(loopback, &h2));
        // A LAN peer cannot pick its own bucket with the header.
        let lan = Some(IpAddr::from([192, 168, 1, 50]));
        assert_eq!(keying_peer(lan, &h), PeerKey::of(lan));
        // An unparseable value keys on the socket peer.
        let mut junk = HeaderMap::new();
        junk.insert(CF_CONNECTING_IP, "not-an-ip".parse().unwrap());
        assert_eq!(keying_peer(loopback, &junk), PeerKey::of(loopback));
    }

    #[test]
    fn every_proxied_ws_route_has_a_mintable_scope() {
        for (path, scope) in PROXIED_WS_SCOPES {
            assert_eq!(proxied_ws_scope(path), Some(*scope));
        }
        assert_eq!(proxied_ws_scope("/api/status"), None);
    }
}
