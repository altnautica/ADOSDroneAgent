#[test]
fn the_media_plane_is_never_credential_free() {
    // A PAIRED node used to serve its live video to any peer on the LAN with
    // no credential, because the proxy exempted everything outside `/api/`.
    // That made a paired node LOOSER than the same node while unpaired,
    // where `auth::is_operator_ui` refuses `/whep` deliberately.
    for path in ["/whep", "/whep/abc123", "/hls", "/hls/main/index.m3u8"] {
        assert!(
            !is_exempt(path),
            "{path} is a live data plane and must require a credential"
        );
    }
}

#[test]
fn narrowing_the_media_plane_did_not_close_the_operator_ui() {
    // The SPA is mounted at `/` with a 404 -> index.html fallback, so its
    // client-side routes are not enumerable and must stay exempt. Breaking
    // these would take the whole UI down to fix a video leak.
    for path in [
        "/",
        "/index.html",
        "/assets/app.js",
        "/settings/region",
        "/logs",
    ] {
        assert!(
            is_exempt(path),
            "{path} is operator UI and must stay served"
        );
    }
    // `/docs` is NOT exempt any more. It enumerated the whole route
    // surface — including every path this file gates — to any peer that
    // could reach the proxy, and the residual app no longer serves it.
    for path in ["/docs", "/docs/oauth2-redirect", "/redoc", "/openapi.json"] {
        assert!(!is_exempt(path), "{path} must not be public");
    }
    // The live video lane is a data plane, not the shell.
    for path in ["/whep", "/whep/main", "/hls", "/hls/main/index.m3u8"] {
        assert!(!is_exempt(path), "{path} must not be public");
    }
    // Boundary: a path that merely SHARES a prefix with a denied one is
    // still SPA. A bare `starts_with` here would take a real UI route
    // down while reading as a tightening.
    assert!(is_exempt("/whepinar"), "/whepinar is not the video lane");
    assert!(is_exempt("/documentation"), "/documentation is not /docs");
}

/// Every non-`/api/` route the front serves natively is classified: it is
/// either a data plane this file denies, or it is the operator UI.
///
/// This is the gate that makes the SPA fallback safe. The fallback has to
/// stay open — the dashboard's client-side routes are not enumerable — so
/// the property that actually protects the surface is that a NEW
/// non-`/api/` route cannot appear without someone deciding which side of
/// the line it is on. Enforced against the real route table rather than
/// guessed from the shape of a URL.
#[test]
fn every_non_api_route_is_classified() {
    // Public by design: the liveness probe carries no data and a watchdog
    // must never be gated. Extend deliberately, with a reason.
    const KNOWN_PUBLIC: &[&str] = &["/healthz"];

    let unclassified: Vec<&str> = crate::routing::native_route_table()
        .into_iter()
        .map(|(_, path)| path)
        .filter(|path| !path.starts_with("/api/"))
        .filter(|path| !is_non_api_data_plane(path))
        .filter(|path| !KNOWN_PUBLIC.contains(path))
        .filter(|path| !crate::auth::is_operator_ui(path))
        .collect();

    assert!(
        unclassified.is_empty(),
        "these non-/api/ routes are served with no credential and nobody \
             classified them — add each to NON_API_DATA_PLANES (if it carries \
             data) or to KNOWN_PUBLIC (with a reason): {unclassified:?}",
    );
}

use super::*;

fn paired(key: &str) -> Pairing {
    Pairing::Paired(key.to_string())
}

fn cfg() -> SecuritySection {
    SecuritySection::default()
}

/// An auth with no configured key, HMAC off, no token file.
fn auth_basic() -> ProxiedAuth {
    ProxiedAuth::with_paths(
        cfg(),
        PathBuf::from("/nonexistent/setup-token"),
        ReplayDetector::default_security(),
    )
}

fn headers_with_key(key: &str) -> RequestHeaders {
    RequestHeaders {
        x_ados_key: Some(key.to_string()),
        ..Default::default()
    }
}

// ---- exempt + static-asset ----

#[test]
fn exempt_paths_accept_even_when_paired() {
    let auth = auth_basic();
    let p = paired("k");
    for path in EXEMPT_PATHS {
        assert_eq!(
            auth.decide_api_key(&Method::GET, path, &RequestHeaders::default(), false, &p),
            Decision::Accept,
            "{path} should be exempt"
        );
    }
}

#[test]
fn the_docs_surface_is_refused_and_static_assets_are_served() {
    let auth = auth_basic();
    let p = paired("k");
    // `/docs*` is REFUSED on a paired node. It enumerates the route
    // surface, and the residual app no longer constructs it at all.
    assert!(matches!(
        auth.decide_api_key(
            &Method::GET,
            "/docs/oauth2-redirect",
            &RequestHeaders::default(),
            false,
            &p
        ),
        Decision::Reject { .. }
    ));
    // Non-/api/ static SPA asset.
    assert_eq!(
        auth.decide_api_key(
            &Method::GET,
            "/assets/app.js",
            &RequestHeaders::default(),
            false,
            &p
        ),
        Decision::Accept
    );
    // An /api/ route is NOT a static asset → gated when paired with no key.
    assert!(matches!(
        auth.decide_api_key(
            &Method::GET,
            "/api/status",
            &RequestHeaders::default(),
            false,
            &p
        ),
        Decision::Reject { .. }
    ));
}

// ---- OPTIONS + on-box ----

#[test]
fn options_preflight_accepts() {
    let auth = auth_basic();
    let p = paired("k");
    assert_eq!(
        auth.decide_api_key(
            &Method::OPTIONS,
            "/api/status",
            &RequestHeaders::default(),
            false,
            &p
        ),
        Decision::Accept
    );
}

#[test]
fn on_box_accepts_a_paired_route_with_no_key() {
    let auth = auth_basic();
    let p = paired("k");
    assert_eq!(
        auth.decide_api_key(
            &Method::POST,
            "/api/command",
            &RequestHeaders::default(),
            true,
            &p
        ),
        Decision::Accept
    );
}

// ---- cloud-posture setup routes ----

#[test]
fn cloud_posture_paths_reject_without_credential_even_unpaired() {
    let auth = auth_basic();
    for path in SAME_ORIGIN_SETUP_CLOUD_PATHS {
        let d = auth.decide_api_key(
            &Method::POST,
            path,
            &RequestHeaders::default(),
            false,
            &Pairing::Unpaired,
        );
        assert_eq!(
            d,
            reject(
                StatusCode::UNAUTHORIZED,
                BodyField::Detail,
                messages::CLOUD_POSTURE
            ),
            "{path} must require a credential even while unpaired"
        );
        // Any non-empty key is not a credential on an unpaired node.
        let d = auth.decide_api_key(
            &Method::POST,
            path,
            &headers_with_key("x"),
            false,
            &Pairing::Unpaired,
        );
        assert_eq!(
            d,
            reject(
                StatusCode::UNAUTHORIZED,
                BodyField::Detail,
                messages::CLOUD_POSTURE
            ),
            "{path} must not accept an arbitrary key while unpaired"
        );
    }
}

#[test]
fn setup_status_needs_a_credential_once_paired() {
    let auth = auth_basic();
    let status = "/api/v1/setup/status";
    assert_eq!(
        auth.decide_api_key(
            &Method::GET,
            status,
            &RequestHeaders::default(),
            false,
            &Pairing::Unpaired
        ),
        Decision::Accept,
        "the setup wizard reads it before pairing"
    );
    assert_eq!(
        auth.decide_api_key(
            &Method::GET,
            status,
            &RequestHeaders::default(),
            false,
            &paired("k")
        ),
        reject(
            StatusCode::UNAUTHORIZED,
            BodyField::Detail,
            messages::MISSING_KEY
        )
    );
}

#[test]
fn cloud_posture_path_accepts_with_a_valid_key() {
    // Paired agent, the request carries the matching key.
    let auth = auth_basic();
    let p = paired("the-key");
    assert_eq!(
        auth.decide_api_key(
            &Method::POST,
            "/api/v1/setup/cloud-choice",
            &headers_with_key("the-key"),
            false,
            &p,
        ),
        Decision::Accept
    );
}

#[test]
fn cloud_posture_path_accepts_with_a_valid_setup_token() {
    let dir = tempfile::tempdir().unwrap();
    let token_path = dir.path().join("setup-token");
    std::fs::write(&token_path, "  tok-123\n").unwrap();
    let auth = ProxiedAuth::with_paths(cfg(), token_path, ReplayDetector::default_security());
    let headers = RequestHeaders {
        x_ados_setup_token: Some("tok-123".to_string()),
        ..Default::default()
    };
    assert_eq!(
        auth.decide_api_key(
            &Method::POST,
            "/api/v1/setup/remote-access/cloudflare",
            &headers,
            false,
            &Pairing::Unpaired,
        ),
        Decision::Accept
    );
}

// ---- setup mutations ----

#[test]
fn setup_mutation_same_origin_accepts_when_token_not_required() {
    let auth = auth_basic(); // setup_token_required defaults false
    let headers = RequestHeaders {
        origin: Some("http://192.168.4.1:8080".to_string()),
        host: Some("192.168.4.1:8080".to_string()),
        ..Default::default()
    };
    assert_eq!(
        auth.decide_api_key(
            &Method::POST,
            "/api/v1/setup/finish",
            &headers,
            false,
            &Pairing::Unpaired
        ),
        Decision::Accept
    );
    // A prefix route (step/) too.
    assert_eq!(
        auth.decide_api_key(
            &Method::POST,
            "/api/v1/setup/step/profile",
            &headers,
            false,
            &Pairing::Unpaired
        ),
        Decision::Accept
    );
}

#[test]
fn setup_mutation_rejects_when_token_required_and_absent() {
    let mut c = cfg();
    c.setup_token_required = true;
    let auth = ProxiedAuth::with_paths(
        c,
        PathBuf::from("/nonexistent"),
        ReplayDetector::default_security(),
    );
    let headers = RequestHeaders {
        origin: Some("http://192.168.4.1:8080".to_string()),
        host: Some("192.168.4.1:8080".to_string()),
        ..Default::default()
    };
    assert_eq!(
        auth.decide_api_key(
            &Method::POST,
            "/api/v1/setup/skip",
            &headers,
            false,
            &Pairing::Unpaired
        ),
        reject(
            StatusCode::UNAUTHORIZED,
            BodyField::Detail,
            messages::SETUP_TOKEN
        ),
    );
}

#[test]
fn setup_mutation_accepts_when_token_required_and_valid() {
    let dir = tempfile::tempdir().unwrap();
    let token_path = dir.path().join("setup-token");
    std::fs::write(&token_path, "tok-xyz").unwrap();
    let mut c = cfg();
    c.setup_token_required = true;
    let auth = ProxiedAuth::with_paths(c, token_path, ReplayDetector::default_security());
    let headers = RequestHeaders {
        x_ados_setup_token: Some("tok-xyz".to_string()),
        ..Default::default()
    };
    assert_eq!(
        auth.decide_api_key(
            &Method::POST,
            "/api/v1/setup/reset",
            &headers,
            false,
            &Pairing::Unpaired
        ),
        Decision::Accept
    );
}

#[test]
fn setup_mutation_not_same_origin_falls_through_to_paired_gate() {
    // token not required, NOT same-origin, paired → falls through to the
    // paired-key requirement (matches the Python, which does not early-return
    // from the setup block in this branch).
    let auth = auth_basic();
    let p = paired("k");
    let headers = RequestHeaders::default(); // no origin → not same-origin
    let d = auth.decide_api_key(&Method::POST, "/api/v1/setup/finish", &headers, false, &p);
    assert_eq!(
        d,
        reject(
            StatusCode::UNAUTHORIZED,
            BodyField::Detail,
            messages::MISSING_KEY
        )
    );
}

#[test]
fn paired_same_origin_without_key_is_401() {
    // Origin and Host are caller-chosen; once paired they grant nothing.
    let auth = auth_basic();
    let p = paired("k");
    let headers = RequestHeaders {
        origin: Some("http://192.168.4.1:8080".to_string()),
        host: Some("192.168.4.1:8080".to_string()),
        ..Default::default()
    };
    let d = auth.decide_api_key(&Method::POST, "/api/v1/setup/finish", &headers, false, &p);
    assert_eq!(
        d,
        reject(
            StatusCode::UNAUTHORIZED,
            BodyField::Detail,
            messages::MISSING_KEY
        )
    );
}

// ---- unpaired open ----

#[test]
fn unpaired_opens_general_routes() {
    let auth = auth_basic();
    assert_eq!(
        auth.decide_api_key(
            &Method::POST,
            "/api/command",
            &RequestHeaders::default(),
            false,
            &Pairing::Unpaired
        ),
        Decision::Accept
    );
}

// ---- paired key ----

#[test]
fn paired_missing_key_is_a_distinct_401() {
    let auth = auth_basic();
    let p = paired("k");
    assert_eq!(
        auth.decide_api_key(
            &Method::GET,
            "/api/status",
            &RequestHeaders::default(),
            false,
            &p
        ),
        reject(
            StatusCode::UNAUTHORIZED,
            BodyField::Detail,
            messages::MISSING_KEY
        )
    );
    // An empty key reads as missing.
    assert_eq!(
        auth.decide_api_key(
            &Method::GET,
            "/api/status",
            &headers_with_key(""),
            false,
            &p
        ),
        reject(
            StatusCode::UNAUTHORIZED,
            BodyField::Detail,
            messages::MISSING_KEY
        )
    );
}

#[test]
fn paired_wrong_key_is_invalid_key() {
    let auth = auth_basic();
    let p = paired("right-key");
    assert_eq!(
        auth.decide_api_key(
            &Method::GET,
            "/api/status",
            &headers_with_key("wrong-key"),
            false,
            &p
        ),
        reject(
            StatusCode::UNAUTHORIZED,
            BodyField::Detail,
            messages::INVALID_KEY
        )
    );
}

#[test]
fn paired_pairing_key_accepts() {
    let auth = auth_basic();
    let p = paired("right-key");
    assert_eq!(
        auth.decide_api_key(
            &Method::GET,
            "/api/status",
            &headers_with_key("right-key"),
            false,
            &p
        ),
        Decision::Accept
    );
}

#[test]
fn paired_configured_key_accepts_independent_of_pairing_key() {
    // The manually-configured security.api.api_key passes even when it
    // differs from the pairing key.
    let mut c = cfg();
    c.api.api_key = "configured-key".to_string();
    let auth = ProxiedAuth::with_paths(
        c,
        PathBuf::from("/nonexistent"),
        ReplayDetector::default_security(),
    );
    let p = paired("a-different-pairing-key");
    assert_eq!(
        auth.decide_api_key(
            &Method::GET,
            "/api/status",
            &headers_with_key("configured-key"),
            false,
            &p
        ),
        Decision::Accept
    );
    // And the pairing key still works alongside it.
    assert_eq!(
        auth.decide_api_key(
            &Method::GET,
            "/api/status",
            &headers_with_key("a-different-pairing-key"),
            false,
            &p
        ),
        Decision::Accept
    );
}

// ---- same-origin host parsing ----

#[test]
fn origin_host_parses_like_urlparse() {
    assert_eq!(
        parse_hostname("http://localhost:8080"),
        Some("localhost".to_string())
    );
    assert_eq!(
        parse_hostname("https://192.168.4.1/path?x=1"),
        Some("192.168.4.1".to_string())
    );
    assert_eq!(
        parse_hostname("http://user:pw@host.local:9/p"),
        Some("host.local".to_string())
    );
    assert_eq!(
        parse_hostname("http://[::1]:8080/x"),
        Some("::1".to_string())
    );
    assert_eq!(parse_hostname("not-a-url"), None);
    assert_eq!(parse_hostname("http://"), None);
    assert_eq!(
        parse_hostname("HTTP://LocalHost"),
        Some("localhost".to_string())
    );
}

#[test]
fn same_origin_falls_back_to_referer_and_host_match() {
    // Referer used when Origin is absent; host == request Host host.
    let h = RequestHeaders {
        referer: Some("http://my-drone.local:8080/setup".to_string()),
        host: Some("my-drone.local:8080".to_string()),
        ..Default::default()
    };
    assert!(is_same_origin(&h));
    // A cross-origin host that matches neither a default nor the Host → not
    // same-origin.
    let h2 = RequestHeaders {
        origin: Some("http://evil.example/".to_string()),
        host: Some("my-drone.local:8080".to_string()),
        ..Default::default()
    };
    assert!(!is_same_origin(&h2));
    // No Origin/Referer → not same-origin.
    assert!(!is_same_origin(&RequestHeaders::default()));
}

// ---- HMAC / replay ----

/// A known secret + a hand-computed signature to lock the wire format.
fn hmac_auth(secret: &str) -> ProxiedAuth {
    let mut c = cfg();
    c.hmac_enabled = true;
    c.hmac_secret = secret.to_string();
    ProxiedAuth::with_paths(
        c,
        PathBuf::from("/nonexistent"),
        ReplayDetector::default_security(),
    )
}

/// Sign a `POST /api/command` the way a client must: method, target,
/// nonce, timestamp and body.
fn sign(secret: &str, nonce: &str, timestamp: f64, body: &[u8]) -> String {
    sign_for(secret, "POST", "/api/command", nonce, timestamp, body)
}

fn sign_for(
    secret: &str,
    method: &str,
    path: &str,
    nonce: &str,
    timestamp: f64,
    body: &[u8],
) -> String {
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).unwrap();
    mac.update(format!("{method}\n{path}\n{nonce}\n").as_bytes());
    mac.update(&timestamp.to_be_bytes());
    mac.update(body);
    hex::encode(mac.finalize().into_bytes())
}

#[test]
fn hmac_inactive_when_disabled_or_short_secret() {
    // Disabled → not active.
    let mut c = cfg();
    c.hmac_enabled = false;
    c.hmac_secret = "a-long-enough-secret-key".to_string();
    let a = ProxiedAuth::with_paths(c, PathBuf::from("/x"), ReplayDetector::default_security());
    assert!(!a.hmac_active());
    // Enabled but a < 16-byte secret → not active (matches the Python guard).
    let mut c2 = cfg();
    c2.hmac_enabled = true;
    c2.hmac_secret = "short".to_string();
    let a2 = ProxiedAuth::with_paths(c2, PathBuf::from("/x"), ReplayDetector::default_security());
    assert!(!a2.hmac_active());
    // A disabled gate accepts every body unconditionally.
    assert_eq!(
        a.decide_hmac(
            &Method::POST,
            "/api/command",
            None,
            &RequestHeaders::default(),
            b"{}"
        ),
        Decision::Accept
    );
}

#[test]
fn hmac_skips_non_mutating_and_exempt() {
    let auth = hmac_auth("a-long-enough-secret-key");
    assert!(auth.hmac_active());
    // GET is not verified.
    assert_eq!(
        auth.decide_hmac(
            &Method::GET,
            "/api/status",
            None,
            &RequestHeaders::default(),
            b""
        ),
        Decision::Accept
    );
    // An HMAC-exempt route.
    assert_eq!(
        auth.decide_hmac(
            &Method::POST,
            "/api/pairing/claim",
            None,
            &RequestHeaders::default(),
            b"{}"
        ),
        Decision::Accept
    );
    // The other pairing mutations are signed: a captured key alone must
    // not be enough to replay an unpair.
    for path in ["/api/pairing/unpair", "/api/pairing/accept"] {
        assert_eq!(
            auth.decide_hmac(&Method::POST, path, None, &RequestHeaders::default(), b"{}"),
            reject(
                StatusCode::UNAUTHORIZED,
                BodyField::Error,
                messages::MISSING_SECURITY_HEADERS
            ),
            "{path}"
        );
    }
    // needs_body is false for an exempt/non-mutating path, true for a real one.
    assert!(!auth.hmac_needs_body(&Method::GET, "/api/command"));
    assert!(!auth.hmac_needs_body(&Method::POST, "/api/pairing/claim"));
    assert!(auth.hmac_needs_body(&Method::POST, "/api/command"));
}

#[test]
fn hmac_missing_headers_is_401() {
    let auth = hmac_auth("a-long-enough-secret-key");
    assert_eq!(
        auth.decide_hmac(
            &Method::POST,
            "/api/command",
            None,
            &RequestHeaders::default(),
            b"{}"
        ),
        reject(
            StatusCode::UNAUTHORIZED,
            BodyField::Error,
            messages::MISSING_SECURITY_HEADERS
        ),
    );
}

#[test]
fn hmac_bad_timestamp_is_401() {
    let auth = hmac_auth("a-long-enough-secret-key");
    let headers = RequestHeaders {
        x_timestamp: Some("not-a-number".to_string()),
        x_nonce: Some("n1".to_string()),
        x_hmac_signature: Some("deadbeef".to_string()),
        ..Default::default()
    };
    assert_eq!(
        auth.decide_hmac(&Method::POST, "/api/command", None, &headers, b"{}"),
        reject(
            StatusCode::UNAUTHORIZED,
            BodyField::Error,
            messages::BAD_TIMESTAMP
        ),
    );
}

#[test]
fn hmac_expired_timestamp_is_403() {
    let secret = "a-long-enough-secret-key";
    let auth = hmac_auth(secret);
    // A timestamp far outside the 300s window.
    let ts = unix_now() - 10_000.0;
    let body = b"{\"cmd\":\"arm\"}";
    let sig = sign(secret, "n-expired", ts, body);
    let headers = RequestHeaders {
        x_timestamp: Some(format!("{ts}")),
        x_nonce: Some("n-expired".to_string()),
        x_hmac_signature: Some(sig),
        ..Default::default()
    };
    assert_eq!(
        auth.decide_hmac(&Method::POST, "/api/command", None, &headers, body),
        reject(StatusCode::FORBIDDEN, BodyField::Error, messages::REPLAY),
    );
}

#[test]
fn hmac_duplicate_nonce_is_403() {
    let secret = "a-long-enough-secret-key";
    let auth = hmac_auth(secret);
    let ts = unix_now();
    let body = b"{\"cmd\":\"land\"}";
    let sig = sign(secret, "dup-nonce", ts, body);
    let headers = RequestHeaders {
        x_timestamp: Some(format!("{ts}")),
        x_nonce: Some("dup-nonce".to_string()),
        x_hmac_signature: Some(sig),
        ..Default::default()
    };
    // First passes.
    assert_eq!(
        auth.decide_hmac(&Method::POST, "/api/command", None, &headers, body),
        Decision::Accept
    );
    // The replay of the same nonce is rejected.
    assert_eq!(
        auth.decide_hmac(&Method::POST, "/api/command", None, &headers, body),
        reject(StatusCode::FORBIDDEN, BodyField::Error, messages::REPLAY),
    );
}

#[test]
fn hmac_bad_signature_is_401() {
    let secret = "a-long-enough-secret-key";
    let auth = hmac_auth(secret);
    let ts = unix_now();
    let headers = RequestHeaders {
        x_timestamp: Some(format!("{ts}")),
        x_nonce: Some("n-badsig".to_string()),
        x_hmac_signature: Some("00".repeat(32)), // wrong but valid hex
        ..Default::default()
    };
    assert_eq!(
        auth.decide_hmac(&Method::POST, "/api/command", None, &headers, b"{}"),
        reject(
            StatusCode::UNAUTHORIZED,
            BodyField::Error,
            messages::INVALID_SIGNATURE
        ),
    );
}

#[test]
fn hmac_valid_signature_accepts() {
    let secret = "a-long-enough-secret-key";
    let auth = hmac_auth(secret);
    let ts = unix_now();
    let body = b"{\"cmd\":\"takeoff\",\"alt\":10}";
    let sig = sign(secret, "n-valid", ts, body);
    let headers = RequestHeaders {
        x_timestamp: Some(format!("{ts}")),
        x_nonce: Some("n-valid".to_string()),
        x_hmac_signature: Some(sig),
        ..Default::default()
    };
    assert_eq!(
        auth.decide_hmac(&Method::POST, "/api/command", None, &headers, body),
        Decision::Accept
    );
}

#[test]
fn a_digest_signature_covers_the_declared_digest_not_the_body() {
    let secret = "a-long-enough-secret-key";
    let auth = hmac_auth(secret);
    let ts = unix_now();
    let path = "/api/plugins/install";
    let digest = "ab".repeat(32);
    let signed = |nonce: &str| RequestHeaders {
        x_timestamp: Some(format!("{ts}")),
        x_nonce: Some(nonce.to_string()),
        x_hmac_signature: Some(sign_for(
            secret,
            "POST",
            path,
            nonce,
            ts,
            format!("\n{digest}").as_bytes(),
        )),
        ..Default::default()
    };
    assert_eq!(
        auth.decide_hmac_digest(&Method::POST, path, None, &signed("n-d1"), &digest),
        Decision::Accept
    );
    // The same signature does not vouch for a different digest.
    assert_eq!(
        auth.decide_hmac_digest(&Method::POST, path, None, &signed("n-d2"), &"cd".repeat(32)),
        reject(
            StatusCode::UNAUTHORIZED,
            BodyField::Error,
            messages::INVALID_SIGNATURE
        ),
    );
    for route in DIGEST_SIGNED_ROUTES {
        assert!(ProxiedAuth::is_digest_signed_route(&Method::POST, route));
    }
    assert!(!ProxiedAuth::is_digest_signed_route(
        &Method::POST,
        "/api/command"
    ));
    assert!(!ProxiedAuth::is_digest_signed_route(&Method::GET, path));
    assert!(is_sha256_hex(&digest));
    assert!(!is_sha256_hex("ab"));
    assert!(!is_sha256_hex(&"zz".repeat(32)));
}

#[test]
fn a_forged_signature_does_not_burn_the_nonce() {
    let secret = "a-long-enough-secret-key";
    let auth = hmac_auth(secret);
    let ts = unix_now();
    let body = b"{\"cmd\":\"land\"}";
    let forged = RequestHeaders {
        x_timestamp: Some(format!("{ts}")),
        x_nonce: Some("n-shared".to_string()),
        x_hmac_signature: Some("00".repeat(32)),
        ..Default::default()
    };
    assert_eq!(
        auth.decide_hmac(&Method::POST, "/api/command", None, &forged, body),
        reject(
            StatusCode::UNAUTHORIZED,
            BodyField::Error,
            messages::INVALID_SIGNATURE
        ),
    );
    // The genuinely signed request carrying the same nonce still passes.
    let signed = RequestHeaders {
        x_hmac_signature: Some(sign(secret, "n-shared", ts, body)),
        ..forged
    };
    assert_eq!(
        auth.decide_hmac(&Method::POST, "/api/command", None, &signed, body),
        Decision::Accept
    );
}

#[test]
fn a_non_finite_timestamp_is_refused_even_when_signed() {
    let secret = "a-long-enough-secret-key";
    let auth = hmac_auth(secret);
    let body = b"{}";
    for (raw, ts) in [("NaN", f64::NAN), ("inf", f64::INFINITY)] {
        let headers = RequestHeaders {
            x_timestamp: Some(raw.to_string()),
            x_nonce: Some(format!("n-{raw}")),
            x_hmac_signature: Some(sign(secret, &format!("n-{raw}"), ts, body)),
            ..Default::default()
        };
        assert_eq!(
            auth.decide_hmac(&Method::POST, "/api/command", None, &headers, body),
            reject(
                StatusCode::UNAUTHORIZED,
                BodyField::Error,
                messages::BAD_TIMESTAMP
            ),
            "{raw}"
        );
    }
}

#[test]
fn hmac_wire_format_binds_method_target_nonce_timestamp_and_body() {
    // Lock the exact wire format against a hand-built vector:
    // `METHOD \n path?query \n nonce \n be_f64(ts) || body`.
    let secret = b"a-long-enough-secret-key";
    let ts: f64 = 1_700_000_000.5;
    let body = b"payload-bytes";
    let signed = SignedRequest {
        method: "POST",
        path: "/api/command",
        query: Some("force=1"),
        nonce: "n-1",
        timestamp: ts,
        body,
    };

    let mut mac = HmacSha256::new_from_slice(secret).unwrap();
    mac.update(b"POST\n/api/command?force=1\nn-1\n");
    mac.update(&ts.to_be_bytes());
    mac.update(body);
    let good = hex::encode(mac.finalize().into_bytes());
    assert!(verify_hmac(secret, &signed, &good));

    // The same signature does not carry to another route, method, query or
    // nonce: a captured request cannot be re-aimed with a fresh nonce.
    for other in [
        SignedRequest {
            path: "/api/config",
            ..signed
        },
        SignedRequest {
            method: "PUT",
            ..signed
        },
        SignedRequest {
            query: None,
            ..signed
        },
        SignedRequest {
            nonce: "n-2",
            ..signed
        },
    ] {
        assert!(!verify_hmac(secret, &other, &good));
    }

    // The old timestamp-and-body-only signature no longer verifies.
    let mut old = HmacSha256::new_from_slice(secret).unwrap();
    old.update(&ts.to_be_bytes());
    old.update(body);
    let old_hex = hex::encode(old.finalize().into_bytes());
    assert!(!verify_hmac(secret, &signed, &old_hex));
}

#[test]
fn replay_detector_window_and_dedup() {
    let det = ReplayDetector::new(300.0, 50_000);
    let now = 1_000_000.0;
    // In-window, first sight → fresh.
    assert!(det.check_at(now, "a", now));
    // Same nonce → rejected.
    assert!(!det.check_at(now, "a", now));
    // A new nonce in-window → fresh.
    assert!(det.check_at(now - 100.0, "b", now));
    // Out of window (older than 300s) → rejected.
    assert!(!det.check_at(now - 1000.0, "c", now));
    // Out of window in the FUTURE (clock skew) → also rejected (abs()).
    assert!(!det.check_at(now + 1000.0, "d", now));
}
