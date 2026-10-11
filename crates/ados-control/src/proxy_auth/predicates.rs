//! Path and header predicates, and HMAC verification of a signed request.

use super::*;

/// Non-`/api/` paths that are a DATA plane, not the operator's browser shell.
///
/// This list, not a path-shape heuristic, is what keeps the SPA fallback from
/// being a hole. Every entry is a prefix; see [`is_non_api_data_plane`].
pub(super) const NON_API_DATA_PLANES: &[&str] = &[
    // Live video. Whoever can read these watches what the aircraft sees.
    "/whep",
    "/hls",
    // The route-surface enumeration. The residual app is now constructed
    // with `docs_url=None, redoc_url=None, openapi_url=None`, so these serve
    // nothing — they stay listed so re-enabling the app's defaults cannot
    // quietly re-open them.
    "/docs",
    "/redoc",
    "/openapi.json",
];

/// Whether a non-`/api/` path is a data plane rather than the SPA shell.
///
/// Prefix-matched on a segment boundary, so `/whep/main` is covered and
/// `/whepinar` is not.
pub(crate) fn is_non_api_data_plane(path: &str) -> bool {
    NON_API_DATA_PLANES.iter().any(|denied| {
        path.starts_with(denied) && matches!(path.as_bytes().get(denied.len()), None | Some(b'/'))
    })
}

/// Whether a proxied route needs no credential.
///
/// Non-`/api/` paths fall through to the SPA, because the dashboard is
/// mounted at `/` with a 404-to-index fallback and its client-side routes
/// are genuinely not enumerable from here — `/settings/region` is a real
/// operator-UI path with no server-side registration to look up. An
/// allow-list of literal SPA routes cannot be written, and one that tried
/// would break a route the day the dashboard added it.
///
/// What IS enumerable is the other direction: the non-`/api/` paths that
/// are a data plane. `/whep` and `/hls` are the reason this matters —
/// under the old blanket rule a live camera feed was served to any peer on
/// the LAN of a PAIRED node with no credential at all, a posture LOOSER
/// than the same node has while unpaired. They are denied by name now,
/// alongside the docs surface, and
/// `tests::every_non_api_route_is_classified` fails when a new non-`/api/`
/// route appears that nobody has classified either way. That converts
/// "nobody has to notice" into "CI notices", enforced against the route
/// table rather than guessed from the shape of a URL.
///
/// `path` is the normalized decision path from `tcp_edge`, never a raw target.
pub(super) fn is_exempt(path: &str) -> bool {
    if is_non_api_data_plane(path) {
        return false;
    }
    if contains(EXEMPT_PATHS, path) {
        return true;
    }
    !path.starts_with("/api/")
}

/// The live-video paths. Outside `/api/` by URL shape, but a data plane by
/// nature: whoever can read these can watch what the aircraft sees.
/// One definition, two callers: the proxy exemption below and the TCP auth edge
/// in `serve`, which needs the same answer to decide whether a session may
/// travel in the query string. Two copies of this predicate would drift, and the
/// drift would silently widen or narrow where a URL-borne credential is accepted.
pub(crate) fn is_media_plane(path: &str) -> bool {
    path == "/whep" || path.starts_with("/whep/") || path == "/hls" || path.starts_with("/hls/")
}

/// True when the path is exempt from HMAC verification.
pub(super) fn is_hmac_exempt(path: &str) -> bool {
    contains(HMAC_EXEMPT_ROUTES, path)
}

/// The mutating methods the HMAC gate verifies.
pub(super) fn is_verified_method(method: &Method) -> bool {
    matches!(
        *method,
        Method::POST | Method::PUT | Method::DELETE | Method::PATCH
    )
}

/// True when the request's Origin/Referer host points at this agent. Mirrors
/// `auth.py` `_is_same_origin` + `_origin_host`: the host comes from Origin or
/// (falling back to) Referer; it is same-origin when it is one of the local
/// defaults OR equals the request's own `Host` header host.
pub(super) fn is_same_origin(headers: &RequestHeaders) -> bool {
    let Some(host) = origin_host(headers) else {
        // No Origin / Referer → a server-to-server caller, not same-origin.
        return false;
    };
    if LOCAL_HOST_DEFAULTS.contains(&host.as_str()) {
        return true;
    }
    let request_host = headers
        .host
        .as_deref()
        .map(|h| h.split(':').next().unwrap_or(h).to_string())
        .unwrap_or_default();
    !request_host.is_empty() && host == request_host
}

/// The host part of the Origin (or Referer) header. Returns `None` when neither header
/// is present or the value has no host.
pub(super) fn origin_host(headers: &RequestHeaders) -> Option<String> {
    let raw = headers.origin.as_deref().or(headers.referer.as_deref())?;
    parse_hostname(raw)
}

/// Extract the hostname from a URL the way `urllib.parse.urlparse(...).hostname`
/// does: drop the scheme, drop any `userinfo@`, take up to the next `/?#`, drop
/// a `:port` suffix, lowercase. A bracketed IPv6 literal keeps its inner host.
/// Returns `None` when there is no host component.
pub(super) fn parse_hostname(url: &str) -> Option<String> {
    // Strip the scheme (`scheme://`); a value with no `://` has no netloc, so it
    // has no hostname (matches urlparse, which puts a bare `host/path` in the
    // path, not the netloc).
    let after_scheme = url.split_once("://").map(|(_, rest)| rest)?;
    // The netloc ends at the first `/`, `?`, or `#`.
    let netloc_end = after_scheme
        .find(['/', '?', '#'])
        .unwrap_or(after_scheme.len());
    let netloc = &after_scheme[..netloc_end];
    // Drop any `userinfo@`.
    let host_port = netloc.rsplit_once('@').map(|(_, h)| h).unwrap_or(netloc);
    // A bracketed IPv6 literal: take the inside of the brackets.
    let host = if let Some(rest) = host_port.strip_prefix('[') {
        rest.split_once(']').map(|(inner, _)| inner).unwrap_or(rest)
    } else {
        // Drop a `:port` suffix.
        host_port.split(':').next().unwrap_or(host_port)
    };
    if host.is_empty() {
        None
    } else {
        Some(host.to_ascii_lowercase())
    }
}

/// The parts of a request the HMAC signature covers.
#[derive(Clone, Copy)]
pub(crate) struct SignedRequest<'a> {
    pub method: &'a str,
    /// The normalized decision path.
    pub path: &'a str,
    /// The raw query string, without the `?`.
    pub query: Option<&'a str>,
    pub nonce: &'a str,
    pub timestamp: f64,
    pub body: &'a [u8],
}

impl SignedRequest<'_> {
    /// The signed message: `METHOD \n path[?query] \n nonce \n be_f64(ts) ||
    /// body`. The method, target and nonce are bound as well as the body: a
    /// signature over the timestamp and body alone could be replayed with a
    /// fresh nonce against any other route taking the same body.
    pub(super) fn feed(&self, mac: &mut HmacSha256) {
        mac.update(self.method.as_bytes());
        mac.update(b"\n");
        mac.update(self.path.as_bytes());
        if let Some(q) = self.query {
            mac.update(b"?");
            mac.update(q.as_bytes());
        }
        mac.update(b"\n");
        mac.update(self.nonce.as_bytes());
        mac.update(b"\n");
        mac.update(&self.timestamp.to_be_bytes());
        mac.update(self.body);
    }
}

/// Verify a hex HMAC-SHA256 signature over [`SignedRequest::feed`]'s message,
/// compared constant-time on the lowercase hex.
pub(super) fn verify_hmac(secret: &[u8], signed: &SignedRequest<'_>, presented_hex: &str) -> bool {
    let mut mac = match HmacSha256::new_from_slice(secret) {
        Ok(m) => m,
        // The caller only reaches here when the secret is >= 16 bytes, so a key
        // error cannot happen; reject defensively if it ever did.
        Err(_) => return false,
    };
    signed.feed(&mut mac);
    let expected = mac.finalize().into_bytes();
    let expected_hex = hex::encode(expected);
    // Compare the hex strings constant-time, mirroring Python's
    // `hmac.compare_digest(expected_hex, signature)`.
    constant_time_eq(expected_hex.as_bytes(), presented_hex.as_bytes())
}

/// `value in set`, for the `&[&str]` constant sets.
pub(super) fn contains(set: &[&str], value: &str) -> bool {
    set.contains(&value)
}

/// `Some(s)` when the option holds a non-empty string, else `None`. Mirrors the
/// Python truthiness check on a header (`if not api_key` etc.): a present-but-
/// empty header is treated as absent.
pub(super) fn non_empty(value: Option<&str>) -> Option<&str> {
    value.filter(|s| !s.is_empty())
}

/// Build a `Reject` decision.
pub(super) fn reject(status: StatusCode, field: BodyField, message: &'static str) -> Decision {
    Decision::Reject {
        status,
        field,
        message,
    }
}
