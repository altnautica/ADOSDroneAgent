//! WebSocket-ticket and media-session credential extraction for the edge.

use super::*;

/// True when a WebSocket-upgrade request to a proxied route carries an authentic,
/// unexpired one-shot HMAC ticket in its `Sec-WebSocket-Protocol` list
/// (`["ados-ws-ticket", "<token>"]`) minted for the scope bound to `path` (see
/// [`edge_util::PROXIED_WS_SCOPES`]). A browser cannot set an `X-ADOS-Key`
/// header on a WS handshake, so the ticket subprotocol is the only data-plane
/// credential it can present for a proxied stream. A ticket for any other
/// scope, or an upgrade to a path with no bound scope, is refused here rather
/// than left to the residual route. Parallels [`native_ws_ticket_admits`].
pub(super) fn ws_upgrade_ticket_admits(
    headers: &http::HeaderMap,
    pairing: &Pairing,
    path: &str,
) -> bool {
    if !crate::proxy::is_websocket_upgrade(headers) {
        return false;
    }
    let Pairing::Paired(key) = pairing else {
        return false;
    };
    let (Some(scope), Some(token)) = (proxied_ws_scope(path), offered_ticket(headers)) else {
        return false;
    };
    WsTicketIssuer::from_api_key(key)
        .verify_once(&token, scope, now_unix())
        .is_ok()
}

/// True when a WebSocket upgrade to a native `path` carries a ticket minted for
/// the scope bound to that path (see
/// [`crate::routes::ws_ticket::path_bound_scope`]). The route itself therefore
/// needs no ticket check: only the one scope it serves gets through.
pub(super) fn native_ws_ticket_admits(
    headers: &http::HeaderMap,
    pairing: &Pairing,
    path: &str,
) -> bool {
    if !crate::proxy::is_websocket_upgrade(headers) {
        return false;
    }
    let Pairing::Paired(key) = pairing else {
        return false;
    };
    let (Some(scope), Some(token)) = (
        crate::routes::ws_ticket::path_bound_scope(path),
        offered_ticket(headers),
    ) else {
        return false;
    };
    WsTicketIssuer::from_api_key(key)
        .verify_once(&token, &scope, now_unix())
        .is_ok()
}

/// The ticket a WebSocket handshake offers: the entry after the
/// `ados-ws-ticket` marker in its `Sec-WebSocket-Protocol` list (comma-joined
/// within one header and/or split across several; a ticket has no comma).
pub(super) fn offered_ticket(headers: &http::HeaderMap) -> Option<String> {
    let offered: Vec<String> = headers
        .get_all("sec-websocket-protocol")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|raw| raw.split(','))
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .collect();
    let pos = offered.iter().position(|p| p == "ados-ws-ticket")?;
    offered.get(pos + 1).cloned()
}

/// The dashboard session a request presents: the `X-ADOS-Dashboard-Session`
/// header, or, on the media plane only, the `ados_session` query parameter.
///
/// The query form is not a convenience. A plain `<video>` element cannot attach
/// a custom header to the requests it makes for a playlist or its segments (the
/// element does the fetching, and there is no hook), so a header-only credential
/// is unreachable for element-driven playback and the operator gets a black
/// frame with no way to authenticate it.
///
/// It is confined to `/whep` and `/hls`. A credential in a URL lands in access
/// logs, browser history and `Referer`, which is why it is not accepted on
/// `/api/*`: those callers are all code that can set a header. Every branch of
/// the edge (unpaired PIN gate, native routes, proxied routes) reads the session
/// through this one function, because `/whep` is native and `/hls` is proxied,
/// and a check only one branch applied would leave the other refusing it.
pub(super) fn presented_session(path: &str, request: &Request) -> Option<String> {
    request
        .headers()
        .get(crate::dashboard_pin::DASHBOARD_SESSION_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
        .or_else(|| {
            crate::proxy_auth::is_media_plane(path)
                .then(|| session_token_from_query(request.uri().query()))
                .flatten()
        })
}

/// The query-string parameter carrying a dashboard session on the media plane.
pub(crate) const MEDIA_SESSION_QUERY_KEY: &str = "ados_session";

/// Pull a dashboard session token out of a query string, if one is there.
///
/// Hand-parsed rather than pulled through a URL crate: the input is a raw query
/// fragment, the only key that matters is one exact name, and an empty value is
/// treated as absent so `?ados_session=` cannot read as a credential. The value
/// is percent-decoded once: the token's field separator is `|`, which the
/// clients' `encodeURIComponent` sends as `%7C`. A malformed escape is absent.
pub(crate) fn session_token_from_query(query: Option<&str>) -> Option<String> {
    query?.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        if k != MEDIA_SESSION_QUERY_KEY || v.is_empty() {
            return None;
        }
        percent_decode_once(v)
    })
}

/// Decode `%XX` escapes once. `None` on a malformed escape or non-UTF-8 result.
pub(super) fn percent_decode_once(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hi = (*bytes.get(i + 1)? as char).to_digit(16)?;
            let lo = (*bytes.get(i + 2)? as char).to_digit(16)?;
            out.push((hi * 16 + lo) as u8);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}
