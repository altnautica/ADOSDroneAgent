//! The LAN-edge auth state and the per-request TCP-edge middleware.

use super::*;

/// Per-edge auth state attached to the TCP layer. The Unix listener does not
/// install the layer at all, so on-box callers are never gated.
#[derive(Clone)]
pub(super) struct EdgeAuth {
    pub(super) pairing: Arc<PairingState>,
    pub(super) rate: Arc<RateLimiter>,
    /// The proxied-route auth decision, run on every forwarded request before
    /// it reaches the residual surface (the front is the single authenticator).
    pub(super) proxied: Arc<ProxiedAuth>,
    /// The dashboard-access PIN store: consulted only on a would-be-401 to accept
    /// a valid dashboard session token as an alternative data-plane credential.
    pub(super) dashboard_pin: Arc<crate::dashboard_pin::DashboardPin>,
    /// The MCP-token store: consulted only on a would-be-401 for a NATIVE route,
    /// and only when the accept flag is on, to admit a scoped MCP token as a
    /// last-resort credential (with per-route scope enforcement).
    pub(super) mcp_tokens: Arc<McpTokenStore>,
    /// The agent config path the edge reads this node's device id from, on the
    /// rare would-be-401-with-MCP-token path.
    pub(super) config_path: std::path::PathBuf,
    /// The live `security:` / `mcp:` posture: the MCP accept flag, and whether
    /// the config is unreadable (off-box mutations are then refused).
    pub(super) security: Arc<SecurityState>,
}

/// The outcome of consulting a presented MCP token at the auth edge.
pub(super) enum McpDecision {
    /// Admit the request; carry the comma-joined granted scope groups to stamp on
    /// the trusted `X-ADOS-MCP-Scopes` header for any downstream consumer.
    Admit(String),
    /// A token was presented and verified, but its scopes do not permit this
    /// route class (or the route is not token-reachable). Reject with `403` rather
    /// than the `401` fall-through, so the client learns it is a scope problem.
    ScopeDenied,
    /// No usable MCP token (absent header, the accept flag is off, or the token
    /// is invalid/expired/revoked). Fall through to the normal `401`.
    Fallthrough,
}

impl EdgeAuth {
    /// Consult a presented MCP token for a native route on a would-be-401. Reads
    /// the accept flag + this node's device id from the cached config snapshot
    /// and verifies the token against the current pairing key. Returns
    /// [`McpDecision::Fallthrough`] the moment anything is missing/invalid, so an
    /// absent flag or a bad token behaves exactly as before (a normal 401).
    pub(super) fn mcp_admits(
        &self,
        method: &http::Method,
        path: &str,
        headers: &http::HeaderMap,
    ) -> McpDecision {
        let Some(token) = headers.get(MCP_TOKEN_HEADER).and_then(|v| v.to_str().ok()) else {
            return McpDecision::Fallthrough;
        };
        // Opt-in: default off. An agent that has never enabled the flag never
        // honors an MCP token, so the whole path is inert until an operator opts in.
        if !self.security.config_or_default().mcp.token_accept_enabled {
            return McpDecision::Fallthrough;
        }
        let device_id = PairingConfig::cached(&self.config_path)
            .agent
            .device_id
            .clone();
        let pairing = self.pairing.current();
        let Some(claims) = self
            .mcp_tokens
            .verify(&pairing, token, now_unix_ms(), &device_id)
        else {
            return McpDecision::Fallthrough;
        };
        // A route that classifies the request from what it reads (a plugin
        // tool's declared safety class) is admitted here and enforces the
        // class itself against the scopes stamped on the request.
        if route_decides_scope(method, path) && !claims.scopes.is_empty() {
            return McpDecision::Admit(claims.scopes.join(","));
        }
        match route_scope(method, path) {
            Some(required) if scope_allows_class(required, &claims.scopes) => {
                McpDecision::Admit(claims.scopes.join(","))
            }
            _ => McpDecision::ScopeDenied,
        }
    }
}

/// The TCP-edge middleware: trustworthy on-box header stamping, then (for native
/// routes) public-path bypass, on-box loopback trust, rate-limit, and auth. The
/// Unix edge does not mount this, so trusted on-box callers bypass all of it.
///
/// Two distinct posture decisions happen here:
///
/// 1. **On-box header.** Every inbound request first has any client-supplied
///    `X-ADOS-Onbox` STRIPPED, then the header is set to `1` only when the
///    front's own on-box check passes (loopback peer + no proxy-forwarding
///    header). Stripping first means a value arriving from off-box cannot be
///    spoofed in, so the residual Python can trust the header the front forwards.
///    This is done for native AND proxied requests so the forwarded value is
///    always trustworthy.
/// 2. **Auth.** A route the front serves natively keeps the full agent auth
///    posture (public bypass, on-box trust, rate-limit, `X-ADOS-Key`). A route
///    that is NOT native falls through to the reverse-proxy: the Rust auth is
///    SKIPPED and the residual FastAPI applies its own auth on the forwarded
///    request (which now carries the trustworthy `X-ADOS-Onbox`).
pub(super) async fn tcp_edge(
    State(edge): State<EdgeAuth>,
    mut request: Request,
    next: Next,
) -> Response {
    // ONE normalized path, computed once, threaded through every gate below.
    //
    // Each predicate used to call `request.uri().path()` for itself. That is
    // the raw target from the request line, so `/%61pi/v1/setup/reboot` misses
    // the relay denylist, misses the public list, misses `routing::is_native`
    // — and therefore falls through to the reverse proxy, where the residual
    // FastAPI decodes it the way every ASGI server does and serves
    // `/api/v1/setup/reboot` to a caller with no credential. A refusal is the
    // only safe answer for a path this layer and the next would read
    // differently.
    let path = match auth::decision_path(request.uri().path()) {
        Ok(p) => p,
        Err(reason) => {
            tracing::warn!(
                raw = %request.uri().path(),
                ?reason,
                "non_canonical_path_refused"
            );
            return detail(StatusCode::BAD_REQUEST, auth::PathRejection::MESSAGE);
        }
    };
    // The router behind this edge, the fallback proxy and the residual all
    // read the request target, so they must read the string this edge just
    // authorized: an accepted encoded spelling is rewritten to the canonical
    // encoding of the decoded path before anything downstream sees it.
    if path != request.uri().path() {
        match canonical_uri(request.uri(), &path) {
            Some(uri) => *request.uri_mut() = uri,
            None => return detail(StatusCode::BAD_REQUEST, auth::PathRejection::MESSAGE),
        }
    }

    // Classify the caller ONCE, from the peer address and the forwarding
    // headers, and hand the one value to every gate below and to every handler
    // (as a request extension, overwriting anything already there). A loopback
    // peer with no forwarding header is the local operator (the `ados` CLI over
    // `127.0.0.1:<port>`); any request that carries a forwarding header was
    // relayed by a proxy or tunnel and is remote, wherever its socket says it
    // came from.
    let peer_ip = request.extensions().get::<PeerAddr>().map(|p| p.0.ip());
    let local_ip = request.extensions().get::<LocalAddr>().map(|l| l.0.ip());
    let caller = classify_caller(peer_ip, local_ip, |h| request.headers().contains_key(h));
    request.extensions_mut().insert(caller);
    let on_box = caller == CallerClass::OnBox;
    // Who the request is charged to (rate limit, PIN lockout): a tunnelled
    // caller on loopback by its forwarded address, anyone else by its peer.
    let peer_key = keying_peer(peer_ip, request.headers());
    request.extensions_mut().insert(PeerKeyHint(peer_key));

    // A config that has never parsed since this process started leaves the
    // security posture unknown. Failing open would mean "HMAC off, no
    // configured key"; instead every off-box write is refused until the file
    // is repaired on the node (the on-box operator and the Unix socket stay
    // open for exactly that).
    if !on_box && is_mutating(request.method()) && edge.security.is_unreadable() {
        tracing::warn!(path = %path, "config_unreadable_mutation_refused");
        return config_unreadable();
    }

    // A request that crossed the radio relay arrives on loopback, so it is
    // on-box by the check above. The drone's relay handler admitted it only
    // after verifying a per-pair ticket bound to this exact request, so it
    // carries the linked ground station's authority. What it must not do is
    // change who the node trusts: pairing, credentials, cloud posture. Those
    // paths are refused here, before the posture is applied. Trusting the
    // marker is safe because it can only ever REMOVE authority: a caller who
    // sets it on a local request loses access to these paths, which is no
    // attack, and a caller who omits it off-box was never on-box to begin with.
    let is_relayed = request.headers().contains_key(auth::RELAYED_HEADER);
    // The relay ticket already authenticated this exact request (method, path
    // and body), so the HMAC gate is not asked to authenticate it a second
    // time with a secret the relay does not hold. Only an on-box caller can be
    // relayed: the marker on an off-box request changes nothing here.
    let hmac_exempt = on_box && is_relayed;
    if is_relayed && auth::relay_forbidden(&path) {
        tracing::warn!(
            path = %path,
            method = %request.method(),
            "relay_forbidden_path_refused"
        );
        return detail(
            StatusCode::FORBIDDEN,
            "This path cannot be reached over the radio relay.",
        );
    }

    // The config write stays relay-reachable (the slot reconciler and the
    // relayed settings surface use it), but not for the trust-root keys. The
    // body is small JSON; it is read here, checked, and handed on unchanged.
    if is_relayed
        && request.method() == axum::http::Method::PUT
        && path == auth::RELAY_CONFIG_WRITE_PATH
    {
        let (parts, body) = request.into_parts();
        let Ok(bytes) = axum::body::to_bytes(body, RELAY_CONFIG_BODY_LIMIT).await else {
            return detail(
                StatusCode::PAYLOAD_TOO_LARGE,
                "Config write body too large.",
            );
        };
        let key = serde_json::from_slice::<serde_json::Value>(&bytes)
            .ok()
            .and_then(|v| v.get("key").and_then(|k| k.as_str()).map(str::to_string));
        match key {
            Some(key) if !auth::relay_config_key_forbidden(&key) => {}
            other => {
                tracing::warn!(key = ?other, "relay_config_key_refused");
                return detail(
                    StatusCode::FORBIDDEN,
                    "This config key cannot be written over the radio relay.",
                );
            }
        }
        request = Request::from_parts(parts, Body::from(bytes));
    }

    // While UNPAIRED the node would answer every route to anyone, flight
    // control included, for as long as it stays unpaired. Narrow that by caller:
    // the local operator and the first-boot lifelines are served, an
    // operator-LAN browser needs a dashboard PIN session for data, and a remote
    // caller gets neither the data nor the claim that would hand it the key.
    //
    // The other public paths stay open to everyone deliberately:
    // `/api/pairing/{info,code}` is how a device is discovered over the LAN, and
    // the claim stays open to every caller on the device's own networks, which
    // is the documented local-first flow.
    //
    // Checked before the native/proxied split so it covers both surfaces, and
    // read per request through the TTL-cached pairing state, so the posture
    // widens the moment the device is paired and narrows again on unpair with no
    // restart — which a bind-time decision cannot do. See
    // `pairing_posture::CallerClass` for why this is not a bind.
    // The operator's own UI is allowed through alongside the public routes. It
    // serves no data — every `/api/*` call it then makes is refused exactly as
    // before — but withholding the shell left an unpaired node unable to show
    // the operator its own pairing code, and left a browser that already had an
    // old copy with no way to fetch a newer one.
    let unpaired = matches!(
        edge.pairing.current(),
        ados_protocol::pairing_posture::Pairing::Unpaired
    );
    match auth::unpaired_decision(request.method(), &path, unpaired, caller) {
        auth::UnpairedDecision::Refuse => {
            tracing::warn!(
                path = %path,
                peer = ?peer_ip,
                ?caller,
                "unpaired_peer_refused"
            );
            return detail(
                StatusCode::FORBIDDEN,
                "This device is not paired yet. Pair it first, or reach it over its hotspot or USB connection.",
            );
        }
        auth::UnpairedDecision::RequirePin => {
            // A trusted operator-LAN browser to a DATA route on an UNPAIRED node:
            // it must hold a dashboard PIN session minted via
            // `/api/dashboard/pin/{set,verify}` (which the edge public-exempts so the
            // operator's browser can obtain one). No session -> 403 exactly as the
            // old flat refusal; with a valid session the data loads. The node is
            // unpaired here, so the session validates under the empty-key issuer
            // used when the data plane had no pairing key.
            let session_ok = presented_session(&path, &request)
                .map(|tok| edge.dashboard_pin.session_valid_unpaired(&tok))
                .unwrap_or(false);
            if !session_ok {
                tracing::warn!(
                    path = %path,
                    peer = ?peer_ip,
                    "unpaired_pin_required"
                );
                return detail(
                    StatusCode::FORBIDDEN,
                    "This device is not paired yet. Set up access with the dashboard PIN, or pair it first.",
                );
            }
        }
        auth::UnpairedDecision::Allow => {}
    }

    // Strip any client-supplied on-box header first (it cannot be trusted), then
    // set it only when the front's own check passes — for every request, native
    // or proxied, so the forwarded value is always trustworthy.
    request.headers_mut().remove(ONBOX_HEADER);
    if on_box {
        request
            .headers_mut()
            .insert(ONBOX_HEADER, axum::http::HeaderValue::from_static("1"));
    }

    // Strip any client-supplied MCP-scopes header (it cannot be trusted). It is set
    // below only when a valid MCP token is admitted — for every request, native or
    // proxied — so a value arriving from a client can never be spoofed in. Mirrors
    // the on-box header's strip-then-set discipline.
    request.headers_mut().remove(MCP_SCOPES_HEADER);

    // Every off-box request is charged to its own caller's budget, on both
    // lanes. A single shared bucket let any host on the network 429 every
    // operator; per caller, a flood exhausts only the flooder. The liveness,
    // version and pairing-handshake paths stay exempt so a watchdog or a fresh
    // GCS is never starved; the PIN login is charged, since it is the one
    // public path a guesser would loop on. Field tile reads that present a
    // valid key are exempt too, since a single map pan fetches dozens of
    // tiles; a keyless or wrong-key tile read is charged like any request.
    let tile_read_authed = auth::is_tile_read(&path)
        && request
            .headers()
            .get("X-ADOS-Key")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|k| edge.pairing.authorize(&path, Some(k)));
    let exempt_from_budget =
        on_box || (auth::is_public(&path) && !auth::is_pin_login(&path)) || tile_read_authed;
    if !exempt_from_budget && !edge.rate.check(peer_key) {
        return detail(
            StatusCode::TOO_MANY_REQUESTS,
            "Request budget exceeded; slow down.",
        );
    }

    // A route the front does not serve natively falls through to the reverse
    // proxy. The front runs the ported auth decision itself before forwarding,
    // so the residual surface no longer carries its own auth layers — the front
    // is the single authenticator for every route it serves or forwards.
    if !crate::routing::is_native(request.method(), &path) {
        return proxied_auth_then_forward(
            &edge,
            on_box,
            hmac_exempt,
            // The SAME normalized path, not a second read of the raw target.
            // A second read is how the two halves of this edge ended up
            // authorizing different strings for one request.
            path,
            request,
            next,
        )
        .await;
    }

    // Liveness, version, and the pairing handshake are public and must always
    // answer before any gate: a fresh GCS has no key yet.
    if auth::is_public(&path) {
        return next.run(request).await;
    }

    if hmac_exempt {
        return next.run(request).await;
    }
    if on_box {
        return hmac_then_run(&edge.proxied, &path, request, next).await;
    }

    let presented = request
        .headers()
        .get("X-ADOS-Key")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    if !edge.pairing.authorize(&path, presented.as_deref()) {
        // Before rejecting, accept a valid dashboard session token (minted by the
        // PIN gate) as an alternative data-plane credential. This is the ONLY
        // place the PIN record is read on the native path, and only on a
        // would-be-401 — an on-box or key-bearing request already passed above, so
        // an authenticated dashboard poll does not stat the record every request.
        // The media plane additionally accepts the session in the QUERY STRING
        // (see [`presented_session`]).
        let session_ok = presented_session(&path, &request)
            // `session_valid`, not `session_valid_for`: the latter returns false
            // whenever the node is unpaired, because an unpaired node mints its
            // sessions under a different issuer. This edge used it anyway, which
            // was survivable only while an unpaired node's data plane was open —
            // the stricter check was never reached. Gating the media plane in
            // both states made it reachable, and the result was an operator who
            // had set a PIN getting telemetry but a black video frame: the same
            // session accepted by one layer and refused by the next.
            .map(|tok| {
                edge.dashboard_pin
                    .session_valid(&edge.pairing.current(), &tok)
            })
            .unwrap_or(false);
        // A browser cannot set `X-ADOS-Key` on a WebSocket handshake. A native
        // stream whose ticket scope names the path's own id (an install job's
        // progress, a plugin's HTTP passthrough) is admitted on a ticket minted
        // for exactly that scope; any other ticket opens nothing here.
        let ticket_ok = !session_ok
            && native_ws_ticket_admits(request.headers(), &edge.pairing.current(), &path);
        if !session_ok && !ticket_ok {
            // Last-resort: a scoped MCP token (behind the default-off accept flag).
            // Admitted only for a native route whose class the token's scopes cover;
            // a verified-but-wrong-scope token is a 403, an absent/invalid one falls
            // through to the same 401 as before.
            match edge.mcp_admits(request.method(), &path, request.headers()) {
                McpDecision::Admit(scopes) => {
                    // Stamp the trusted scope groups (the client value was stripped
                    // at the top) for any downstream consumer, then admit.
                    if let Ok(v) = axum::http::HeaderValue::from_str(&scopes) {
                        request.headers_mut().insert(MCP_SCOPES_HEADER, v);
                    }
                }
                McpDecision::ScopeDenied => {
                    return detail(
                        StatusCode::FORBIDDEN,
                        "The presented MCP token's scope does not permit this route.",
                    );
                }
                McpDecision::Fallthrough => {
                    // Match the FastAPI message so a GCS that surfaces the body reads
                    // the same text against either surface.
                    return detail(
                        StatusCode::UNAUTHORIZED,
                        "Missing X-ADOS-Key header. This agent is paired and requires authentication.",
                    );
                }
            }
        }
    }
    hmac_then_run(&edge.proxied, &path, request, next).await
}
